//! WebSocket-driven pool event watcher.
//!
//! Subscribes via `starknet_subscribeEvents` for each configured pool, filtered
//! to the four v2-fork event selectors (`Sync`/`Swap`/`Mint`/`Burn`). On each
//! notification, decodes the payload into a `domain::PoolEvent` and
//! forwards it through an mpsc channel for the detector to consume.
//!
//! Reconnect strategy: on any connection error, sleep for an exponentially
//! growing delay (capped) and reconnect. The per-pool event counter resets at
//! each reconnect — synthetic but monotonic within a connection, which is
//! sufficient for the adapter's dedup check.
//!
//! Step 9b ships subscribe + decode + forward. **Catch-up after reconnect**
//! (querying `starknet_getEvents` for events missed during the disconnect) is
//! deliberately not in this step; we assume reconnects are rare and short.

use std::collections::HashMap;
use std::time::Duration;

use domain::{EventMeta, PoolEvent, PoolEventKind, PoolId};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use starknet_types_core::felt::Felt;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

/// Selectors for the four v2-fork pool events.
#[derive(Debug, Clone, Copy)]
pub struct PoolEventSelectors {
    pub sync: Felt,
    pub swap: Felt,
    pub mint: Felt,
    pub burn: Felt,
}

/// One subscribed pool plus its event selectors.
#[derive(Debug, Clone)]
pub struct PoolSubscription {
    pub pool: PoolId,
    pub selectors: PoolEventSelectors,
}

#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub ws_url: String,
    pub subscriptions: Vec<PoolSubscription>,
    pub reconnect_initial_delay_ms: u64,
    pub reconnect_max_delay_ms: u64,
}

#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("connection closed by peer")]
    Closed,
}

pub struct WsWatcher {
    config: WatcherConfig,
    tx: mpsc::Sender<PoolEvent>,
}

impl WsWatcher {
    pub fn new(config: WatcherConfig, tx: mpsc::Sender<PoolEvent>) -> Self {
        Self { config, tx }
    }

    /// Run the watcher loop forever. The caller is expected to spawn this on
    /// its own task and cancel it via task cancellation / drop the channel
    /// receiver on shutdown.
    pub async fn run(self) {
        let mut delay = self.config.reconnect_initial_delay_ms;
        loop {
            info!(url = %self.config.ws_url, "watcher connecting");
            match self.run_once().await {
                Ok(()) => {
                    info!("watcher loop exited gracefully; reconnecting");
                    delay = self.config.reconnect_initial_delay_ms;
                }
                Err(e) => {
                    warn!(error = %e, backoff_ms = delay, "watcher errored; backing off");
                    sleep(Duration::from_millis(delay)).await;
                    delay = delay
                        .saturating_mul(2)
                        .min(self.config.reconnect_max_delay_ms);
                }
            }
        }
    }

    async fn run_once(&self) -> Result<(), WatcherError> {
        let (ws_stream, _) = connect_async(&self.config.ws_url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Build address → subscription lookup before subscribing so we can
        // decode notifications as they arrive without locking later.
        let mut by_addr: HashMap<Felt, &PoolSubscription> = HashMap::new();
        for (i, sub) in self.config.subscriptions.iter().enumerate() {
            let addr = *sub.pool.address.as_felt();
            #[allow(clippy::cast_possible_truncation)]
            let request_id = (i as u64).saturating_add(1);
            let req = build_subscribe_request(request_id, sub);
            write.send(Message::text(req.to_string())).await?;
            by_addr.insert(addr, sub);
        }
        info!(
            count = self.config.subscriptions.len(),
            "watcher subscribed"
        );

        let mut counter: HashMap<PoolId, u32> = HashMap::new();

        while let Some(msg) = read.next().await {
            let raw = msg?;
            if raw.is_close() {
                return Err(WatcherError::Closed);
            }
            // Skip binary / ping frames.
            let Ok(text) = raw.into_text() else { continue };
            let v: Value = serde_json::from_str(&text)?;

            // Only the subscription notification carries events. RPC responses
            // to our subscribe calls also flow through here and are skipped.
            if v.get("method").and_then(Value::as_str) != Some("starknet_subscriptionEvents") {
                continue;
            }

            if let Some(event) = decode_event(&v, &by_addr, &mut counter)? {
                if self.tx.send(event).await.is_err() {
                    info!("event consumer dropped; watcher exiting");
                    return Ok(());
                }
            }
        }

        Err(WatcherError::Closed)
    }
}

fn build_subscribe_request(id: u64, sub: &PoolSubscription) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "starknet_subscribeEvents",
        "params": {
            "from_address": felt_to_hex(*sub.pool.address.as_felt()),
            "keys": [[
                felt_to_hex(sub.selectors.sync),
                felt_to_hex(sub.selectors.swap),
                felt_to_hex(sub.selectors.mint),
                felt_to_hex(sub.selectors.burn),
            ]]
        }
    })
}

fn felt_to_hex(f: Felt) -> String {
    format!("{f:#x}")
}

fn parse_felt(v: &Value) -> Result<Felt, WatcherError> {
    let s = v
        .as_str()
        .ok_or_else(|| WatcherError::Decode("expected hex string".into()))?;
    Felt::from_hex(s).map_err(|e| WatcherError::Decode(format!("invalid felt {s}: {e}")))
}

/// Decode a `starknet_subscriptionEvents` notification into a `PoolEvent`.
///
/// Returns `Ok(None)` if the event comes from an unknown pool, or if its
/// `keys[0]` doesn't match any of the four tracked selectors. Returns `Err`
/// only on malformed payloads.
#[allow(clippy::implicit_hasher)]
pub fn decode_event(
    notification: &Value,
    by_addr: &HashMap<Felt, &PoolSubscription>,
    counter: &mut HashMap<PoolId, u32>,
) -> Result<Option<PoolEvent>, WatcherError> {
    let result = notification
        .pointer("/params/result")
        .ok_or_else(|| WatcherError::Decode("missing params.result".into()))?;

    let from_address_v = result
        .get("from_address")
        .ok_or_else(|| WatcherError::Decode("missing from_address".into()))?;
    let from_address = parse_felt(from_address_v)?;

    let Some(sub) = by_addr.get(&from_address) else {
        return Ok(None);
    };

    let keys_arr = result
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| WatcherError::Decode("missing keys array".into()))?;
    let key0_v = keys_arr
        .first()
        .ok_or_else(|| WatcherError::Decode("empty keys array".into()))?;
    let key0 = parse_felt(key0_v)?;

    let kind = if key0 == sub.selectors.sync {
        PoolEventKind::Sync
    } else if key0 == sub.selectors.swap {
        PoolEventKind::Swap
    } else if key0 == sub.selectors.mint {
        PoolEventKind::Mint
    } else if key0 == sub.selectors.burn {
        PoolEventKind::Burn
    } else {
        return Ok(None);
    };

    let block = result
        .get("block_number")
        .and_then(Value::as_u64)
        .ok_or_else(|| WatcherError::Decode("missing block_number".into()))?;

    let data_arr = result
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| WatcherError::Decode("missing data array".into()))?;
    let data: Vec<Felt> = data_arr.iter().map(parse_felt).collect::<Result<_, _>>()?;

    let event_index = {
        let entry = counter.entry(sub.pool).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    };

    Ok(Some(PoolEvent {
        pool: sub.pool,
        meta: EventMeta {
            block,
            tx_index: 0,
            event_index,
        },
        kind,
        data,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use domain::{ContractAddress, DexKind, FeeBps};

    fn felt(n: u64) -> Felt {
        Felt::from(n)
    }

    fn pid(addr: u64) -> PoolId {
        PoolId {
            address: ContractAddress::new(felt(addr)),
            dex: DexKind::JediSwapV1,
            fee: FeeBps::new(30),
        }
    }

    fn selectors() -> PoolEventSelectors {
        PoolEventSelectors {
            sync: felt(0xa11),
            swap: felt(0xa22),
            mint: felt(0xa33),
            burn: felt(0xa44),
        }
    }

    fn sample_notification(from_addr: u64, key0: u64, block: u64) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "starknet_subscriptionEvents",
            "params": {
                "subscription_id": "sub-1",
                "result": {
                    "from_address": felt_to_hex(felt(from_addr)),
                    "keys": [felt_to_hex(felt(key0))],
                    "data": [
                        felt_to_hex(felt(100)),
                        felt_to_hex(felt(0)),
                        felt_to_hex(felt(200)),
                        felt_to_hex(felt(0)),
                    ],
                    "block_hash": "0x123",
                    "block_number": block
                }
            }
        })
    }

    #[test]
    fn decode_sync_event() {
        let pool = pid(0xdead);
        let sub = PoolSubscription {
            pool,
            selectors: selectors(),
        };
        let mut by_addr = HashMap::new();
        by_addr.insert(felt(0xdead), &sub);
        let mut counter = HashMap::new();

        let notif = sample_notification(0xdead, 0xa11, 42);
        let event = decode_event(&notif, &by_addr, &mut counter)
            .unwrap()
            .unwrap();
        assert_eq!(event.kind, PoolEventKind::Sync);
        assert_eq!(event.pool, pool);
        assert_eq!(event.meta.block, 42);
        assert_eq!(event.meta.tx_index, 0);
        assert_eq!(event.meta.event_index, 1);
        assert_eq!(event.data.len(), 4);
    }

    #[test]
    fn counter_increments_per_pool() {
        let pool = pid(0xdead);
        let sub = PoolSubscription {
            pool,
            selectors: selectors(),
        };
        let mut by_addr = HashMap::new();
        by_addr.insert(felt(0xdead), &sub);
        let mut counter = HashMap::new();

        let n1 = sample_notification(0xdead, 0xa11, 10);
        let n2 = sample_notification(0xdead, 0xa22, 11);
        let n3 = sample_notification(0xdead, 0xa11, 12);

        let e1 = decode_event(&n1, &by_addr, &mut counter).unwrap().unwrap();
        let e2 = decode_event(&n2, &by_addr, &mut counter).unwrap().unwrap();
        let e3 = decode_event(&n3, &by_addr, &mut counter).unwrap().unwrap();

        assert_eq!(e1.meta.event_index, 1);
        assert_eq!(e2.meta.event_index, 2);
        assert_eq!(e3.meta.event_index, 3);
        assert_eq!(e2.kind, PoolEventKind::Swap);
    }

    #[test]
    fn from_address_not_in_subscriptions_skipped() {
        let pool = pid(0xdead);
        let sub = PoolSubscription {
            pool,
            selectors: selectors(),
        };
        let mut by_addr = HashMap::new();
        by_addr.insert(felt(0xdead), &sub);
        let mut counter = HashMap::new();

        let notif = sample_notification(0xbeef, 0xa11, 1);
        let r = decode_event(&notif, &by_addr, &mut counter).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn unknown_selector_skipped() {
        let pool = pid(0xdead);
        let sub = PoolSubscription {
            pool,
            selectors: selectors(),
        };
        let mut by_addr = HashMap::new();
        by_addr.insert(felt(0xdead), &sub);
        let mut counter = HashMap::new();

        let notif = sample_notification(0xdead, 0x9999, 1);
        let r = decode_event(&notif, &by_addr, &mut counter).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn missing_block_number_is_decode_error() {
        let pool = pid(0xdead);
        let sub = PoolSubscription {
            pool,
            selectors: selectors(),
        };
        let mut by_addr = HashMap::new();
        by_addr.insert(felt(0xdead), &sub);
        let mut counter = HashMap::new();

        let mut notif = sample_notification(0xdead, 0xa11, 1);
        // Strip block_number
        notif
            .pointer_mut("/params/result")
            .and_then(Value::as_object_mut)
            .unwrap()
            .remove("block_number");
        let r = decode_event(&notif, &by_addr, &mut counter);
        assert!(matches!(r, Err(WatcherError::Decode(_))));
    }

    #[test]
    fn malformed_felt_is_decode_error() {
        let pool = pid(0xdead);
        let sub = PoolSubscription {
            pool,
            selectors: selectors(),
        };
        let mut by_addr = HashMap::new();
        by_addr.insert(felt(0xdead), &sub);
        let mut counter = HashMap::new();

        let mut notif = sample_notification(0xdead, 0xa11, 1);
        // Replace a data entry with garbage
        notif.pointer_mut("/params/result/data").unwrap()[0] = json!("not-hex");
        let r = decode_event(&notif, &by_addr, &mut counter);
        assert!(matches!(r, Err(WatcherError::Decode(_))));
    }

    #[test]
    fn build_subscribe_request_shape() {
        let pool = pid(0xabc);
        let sub = PoolSubscription {
            pool,
            selectors: selectors(),
        };
        let req = build_subscribe_request(7, &sub);
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 7);
        assert_eq!(req["method"], "starknet_subscribeEvents");
        let from_addr = req["params"]["from_address"].as_str().unwrap();
        assert!(from_addr.starts_with("0x"));
        let keys = req["params"]["keys"][0].as_array().unwrap();
        assert_eq!(keys.len(), 4);
    }
}
