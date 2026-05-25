//! Inclusion watcher.
//!
//! `submitter.submit` returns a tx hash immediately; inclusion is decided
//! later by the sequencer. This module polls each submitted hash until it
//! reaches a terminal state, then closes the loop:
//!
//! * **Succeeded** → parse the `Executed` event for realized profit, update
//!   ledger row to `Executed`, `risk.record(TradeOutcome::Executed)`.
//! * **Reverted** → mark ledger row `Reverted`, `risk.record(Reverted)` with
//!   the actual fee paid.
//! * **Timeout** → mark `SimulationFailed` (closest existing status) with a
//!   "inclusion timeout" reason; `risk.record(SkippedSimulation)`.
//!
//! One task is spawned per pending tx — the bot's submit rate is far below
//! the cost of an idle polling task, and per-tx isolation simplifies
//! reasoning about deadlines.

use std::sync::Arc;
use std::time::Duration;

use chain::{EventLog, Provider, RpcProvider, TxStatus};
use chrono::Utc;
use domain::{Felt, SignedAmount, TokenId, U256};
use ledger::{AttemptStatus, AttemptStatusUpdate, Ledger};
use metrics::{counter, histogram};
use risk::{RiskManager, SystemClock, TradeOutcome};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, Instant, MissedTickBehavior};
use tracing::{info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::shutdown::ShutdownToken;

/// One in-flight tx awaiting inclusion.
#[derive(Debug, Clone)]
pub struct PendingTx {
    pub attempt_id: Uuid,
    pub tx_hash: Felt,
    pub token: TokenId,
    /// What we expect `final_balance - initial_balance` to be on-chain.
    /// Used as a fallback if we can't decode the `Executed` event.
    pub expected_gross: U256,
}

/// Knobs for the inclusion watcher.
#[derive(Debug, Clone)]
pub struct InclusionConfig {
    pub poll_interval: Duration,
    pub max_wait: Duration,
    /// Address of the on-chain `ArbExecutor` — used to filter events.
    pub executor_address: Felt,
    /// `starknet_keccak("Executed")` — selector of the event we want to parse.
    pub executed_event_selector: Felt,
}

/// Spawn the inclusion watcher task. The returned handle resolves when the
/// task exits (shutdown signal or rx closure).
pub fn spawn_inclusion(
    config: InclusionConfig,
    provider: Arc<RpcProvider>,
    ledger: Ledger,
    risk: Arc<RiskManager<SystemClock>>,
    mut rx: mpsc::Receiver<PendingTx>,
    shutdown: ShutdownToken,
) -> JoinHandle<()> {
    tokio::spawn(
        async move {
            info!("inclusion watcher up");
            let mut shut = shutdown.clone();
            loop {
                tokio::select! {
                    biased;
                    () = shut.wait() => {
                        info!("inclusion: shutdown received");
                        return;
                    }
                    msg = rx.recv() => {
                        let Some(pending) = msg else {
                            info!("inclusion: rx closed");
                            return;
                        };
                        let provider_clone = provider.clone();
                        let ledger_clone = ledger.clone();
                        let risk_clone = risk.clone();
                        let cfg_clone = config.clone();
                        let shutdown_clone = shutdown.clone();
                        tokio::spawn(async move {
                            watch_tx(
                                pending,
                                provider_clone,
                                ledger_clone,
                                risk_clone,
                                cfg_clone,
                                shutdown_clone,
                            )
                            .await;
                        });
                    }
                }
            }
        }
        .instrument(info_span!("inclusion")),
    )
}

/// Drop-in replacement when no `RpcProvider` is configured. Drains the rx
/// channel with a warn so misconfigurations are visible.
pub fn spawn_inclusion_stub(
    mut rx: mpsc::Receiver<PendingTx>,
    mut shutdown: ShutdownToken,
) -> JoinHandle<()> {
    tokio::spawn(
        async move {
            info!("inclusion stub up (no provider configured)");
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.wait() => return,
                    msg = rx.recv() => {
                        let Some(p) = msg else { return };
                        warn!(?p, "received PendingTx but inclusion is disabled");
                    }
                }
            }
        }
        .instrument(info_span!("inclusion_stub")),
    )
}

async fn watch_tx(
    pending: PendingTx,
    provider: Arc<RpcProvider>,
    ledger: Ledger,
    risk: Arc<RiskManager<SystemClock>>,
    cfg: InclusionConfig,
    mut shutdown: ShutdownToken,
) {
    let started = Instant::now();
    let deadline = started + cfg.max_wait;
    let mut tick = interval(cfg.poll_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // First tick fires immediately — skip to the actual poll cadence.
    tick.tick().await;

    loop {
        tokio::select! {
            () = shutdown.wait() => return,
            _ = tick.tick() => {}
        }

        if Instant::now() > deadline {
            counter!("derrick_tx_outcome_total", "outcome" => "timeout").increment(1);
            histogram!("derrick_inclusion_duration_seconds")
                .record(started.elapsed().as_secs_f64());
            warn!(
                attempt_id = ?pending.attempt_id,
                tx_hash = %format!("{:#x}", pending.tx_hash),
                "inclusion timeout"
            );
            let upd = AttemptStatusUpdate {
                completed_at: Some(Utc::now()),
                reason: Some("inclusion timeout".into()),
                ..Default::default()
            };
            let _ = ledger
                .update_attempt_status(pending.attempt_id, AttemptStatus::SimulationFailed, upd)
                .await;
            risk.record(TradeOutcome::SkippedSimulation {
                token: pending.token,
            });
            return;
        }

        match provider.get_tx_status(pending.tx_hash).await {
            Ok(TxStatus::NotFound | TxStatus::Pending) => {}
            Ok(TxStatus::Succeeded { actual_fee, events }) => {
                histogram!("derrick_inclusion_duration_seconds")
                    .record(started.elapsed().as_secs_f64());
                handle_succeeded(pending, actual_fee, events, &cfg, &ledger, &risk).await;
                return;
            }
            Ok(TxStatus::Reverted { reason, actual_fee }) => {
                histogram!("derrick_inclusion_duration_seconds")
                    .record(started.elapsed().as_secs_f64());
                handle_reverted(pending, reason, actual_fee, &ledger, &risk).await;
                return;
            }
            Err(e) => {
                warn!(
                    attempt_id = ?pending.attempt_id,
                    error = %e,
                    "receipt poll error; will retry"
                );
            }
        }
    }
}

async fn handle_succeeded(
    pending: PendingTx,
    actual_fee: U256,
    events: Vec<EventLog>,
    cfg: &InclusionConfig,
    ledger: &Ledger,
    risk: &RiskManager<SystemClock>,
) {
    counter!("derrick_tx_outcome_total", "outcome" => "executed").increment(1);
    let realized = parse_executed_profit(&events, cfg).unwrap_or_else(|| {
        warn!(
            attempt_id = ?pending.attempt_id,
            "Executed event not found in receipt; falling back to expected_gross"
        );
        pending.expected_gross
    });
    info!(
        attempt_id = ?pending.attempt_id,
        realized = %realized,
        gas = %actual_fee,
        "tx executed"
    );
    let upd = AttemptStatusUpdate {
        completed_at: Some(Utc::now()),
        realized_profit: Some(realized),
        gas_paid: Some(actual_fee),
        ..Default::default()
    };
    let _ = ledger
        .update_attempt_status(pending.attempt_id, AttemptStatus::Executed, upd)
        .await;

    let signed = SignedAmount::positive(pending.token, realized);
    risk.record(TradeOutcome::Executed {
        token: pending.token,
        realized_profit: signed,
    });
}

async fn handle_reverted(
    pending: PendingTx,
    reason: String,
    actual_fee: U256,
    ledger: &Ledger,
    risk: &RiskManager<SystemClock>,
) {
    counter!("derrick_tx_outcome_total", "outcome" => "reverted").increment(1);
    warn!(
        attempt_id = ?pending.attempt_id,
        reason = %reason,
        gas = %actual_fee,
        "tx reverted on chain"
    );
    let upd = AttemptStatusUpdate {
        completed_at: Some(Utc::now()),
        gas_paid: Some(actual_fee),
        reason: Some(format!("reverted: {reason}")),
        ..Default::default()
    };
    let _ = ledger
        .update_attempt_status(pending.attempt_id, AttemptStatus::Reverted, upd)
        .await;
    risk.record(TradeOutcome::Reverted {
        token: pending.token,
        gas_paid: actual_fee,
    });
}

/// Locate the `Executed` event from our executor in the receipt's events and
/// decode `profit: u256`. Returns `None` if the event isn't present or the
/// payload is the wrong shape.
///
/// The Cairo event:
///
/// ```cairo
/// #[derive(Drop, starknet::Event)]
/// struct Executed {
///     #[key] operator: ContractAddress,
///     token_in: ContractAddress,
///     profit: u256,    // two felts: low, high
///     num_calls: u32,
/// }
/// ```
///
/// Layout: `keys = [Executed_selector, operator]`, `data = [token_in, profit_lo, profit_hi, num_calls]`.
fn parse_executed_profit(events: &[EventLog], cfg: &InclusionConfig) -> Option<U256> {
    for e in events {
        if e.from_address != cfg.executor_address {
            continue;
        }
        if e.keys.first() != Some(&cfg.executed_event_selector) {
            continue;
        }
        if e.data.len() < 3 {
            continue;
        }
        let profit_lo = u128::try_from(e.data[1]).ok()?;
        let profit_hi = u128::try_from(e.data[2]).ok()?;
        return Some((U256::from(profit_hi) << 128) | U256::from(profit_lo));
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn felt(n: u64) -> Felt {
        Felt::from(n)
    }

    fn u256_to_felts(n: U256) -> (Felt, Felt) {
        let low = n.low_u128();
        let high = (n >> 128).low_u128();
        (Felt::from(low), Felt::from(high))
    }

    fn cfg() -> InclusionConfig {
        InclusionConfig {
            poll_interval: Duration::from_millis(10),
            max_wait: Duration::from_secs(1),
            executor_address: felt(0xabc),
            executed_event_selector: felt(0xeee),
        }
    }

    #[test]
    fn parses_executed_event_profit() {
        let (lo, hi) = u256_to_felts(U256::from(12_345u64));
        let events = vec![EventLog {
            from_address: felt(0xabc),
            keys: vec![felt(0xeee), felt(0x0fe)],
            data: vec![felt(0xa0ce), lo, hi, felt(2u64)],
        }];
        let profit = parse_executed_profit(&events, &cfg()).unwrap();
        assert_eq!(profit, U256::from(12_345u64));
    }

    #[test]
    fn ignores_events_from_other_addresses() {
        let (lo, hi) = u256_to_felts(U256::from(999u64));
        let events = vec![EventLog {
            from_address: felt(0xdead_beef),
            keys: vec![felt(0xeee), felt(0x0fe)],
            data: vec![felt(0xa0ce), lo, hi, felt(2u64)],
        }];
        assert!(parse_executed_profit(&events, &cfg()).is_none());
    }

    #[test]
    fn ignores_events_with_wrong_selector() {
        let (lo, hi) = u256_to_felts(U256::from(999u64));
        let events = vec![EventLog {
            from_address: felt(0xabc),
            keys: vec![felt(0x222), felt(0x0fe)],
            data: vec![felt(0xa0ce), lo, hi, felt(2u64)],
        }];
        assert!(parse_executed_profit(&events, &cfg()).is_none());
    }

    #[test]
    fn malformed_data_returns_none() {
        let events = vec![EventLog {
            from_address: felt(0xabc),
            keys: vec![felt(0xeee)],
            data: vec![felt(0xa0ce)], // missing profit_lo/profit_hi
        }];
        assert!(parse_executed_profit(&events, &cfg()).is_none());
    }
}
