//! Inclusion watcher.
//!
//! `submitter.submit` returns a tx hash immediately; inclusion is decided
//! later by the sequencer. This module polls each submitted hash until it
//! reaches a terminal state, then closes the loop:
//!
//! * **Succeeded** → record `expected_gross` as realized
//!   (`risk.record(TradeOutcome::Executed)`). The contract's on-chain
//!   `assert(final >= initial + min_profit)` guarantees the trade was at
//!   least profitable enough to satisfy `min_profit`; the exact realized
//!   profit isn't surfaced separately because the contract emits no event.
//! * **Reverted** → `risk.record(Reverted)` with the actual fee paid.
//! * **Timeout** → `risk.record(SkippedSimulation)`.
//!
//! One task is spawned per pending tx — the bot's submit rate is far below
//! the cost of an idle polling task, and per-tx isolation simplifies
//! reasoning about deadlines.

use std::sync::Arc;
use std::time::Duration;

use chain::{Provider, RpcProvider, TxStatus};
use domain::{Felt, SignedAmount, TokenId, U256};
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
    /// Correlation id assigned by the detector — used to grep one attempt
    /// across detector and inclusion log streams.
    pub attempt_id: Uuid,
    pub tx_hash: Felt,
    pub token: TokenId,
    /// What we expect `final_balance - initial_balance` to be on-chain.
    /// Recorded verbatim as realized profit on success — the contract emits
    /// no event, so we trust the off-chain prediction here.
    pub expected_gross: U256,
}

/// Knobs for the inclusion watcher.
#[derive(Debug, Clone)]
pub struct InclusionConfig {
    pub poll_interval: Duration,
    pub max_wait: Duration,
}

/// Spawn the inclusion watcher task. The returned handle resolves when the
/// task exits (shutdown signal or rx closure).
pub fn spawn_inclusion(
    config: InclusionConfig,
    provider: Arc<RpcProvider>,
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
                        let risk_clone = risk.clone();
                        let cfg_clone = config.clone();
                        let shutdown_clone = shutdown.clone();
                        tokio::spawn(async move {
                            watch_tx(
                                pending,
                                provider_clone,
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
            risk.record(TradeOutcome::SkippedSimulation {
                token: pending.token,
            });
            return;
        }

        match provider.get_tx_status(pending.tx_hash).await {
            Ok(TxStatus::NotFound | TxStatus::Pending) => {}
            Ok(TxStatus::Succeeded { actual_fee, .. }) => {
                histogram!("derrick_inclusion_duration_seconds")
                    .record(started.elapsed().as_secs_f64());
                handle_succeeded(pending, actual_fee, &risk).await;
                return;
            }
            Ok(TxStatus::Reverted { reason, actual_fee }) => {
                histogram!("derrick_inclusion_duration_seconds")
                    .record(started.elapsed().as_secs_f64());
                handle_reverted(pending, reason, actual_fee, &risk).await;
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
    risk: &RiskManager<SystemClock>,
) {
    counter!("derrick_tx_outcome_total", "outcome" => "executed").increment(1);
    let realized = pending.expected_gross;
    info!(
        attempt_id = ?pending.attempt_id,
        realized = %realized,
        gas = %actual_fee,
        "tx executed"
    );

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
    risk: &RiskManager<SystemClock>,
) {
    counter!("derrick_tx_outcome_total", "outcome" => "reverted").increment(1);
    warn!(
        attempt_id = ?pending.attempt_id,
        reason = %reason,
        gas = %actual_fee,
        "tx reverted on chain"
    );
    risk.record(TradeOutcome::Reverted {
        token: pending.token,
        gas_paid: actual_fee,
    });
}

