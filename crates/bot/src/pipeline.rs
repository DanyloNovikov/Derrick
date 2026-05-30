//! mpsc topology connecting the bot's modules.
//!
//! Step 10.1 lands the full detector flow up to (but not including) on-chain
//! simulation and submission:
//!
//! * `watcher` — runs `WsWatcher` when given a config, else idles.
//! * `state_updater` — applies each [`PoolEvent`] to the [`PoolRegistry`]
//!   and forwards a [`PoolStateUpdate`] downstream.
//! * `detector` — on each state update, snapshots all pools that share the
//!   updated pool's token pair, runs `detect_spatial_opportunities`, then for
//!   each candidate evaluates risk gating, simulates, and (optionally) submits.
//!   Simulation + submission are wired only when `provider`/`submitter` are
//!   present.
//!
//! Each spawned task `select!`s on a shutdown signal so the main loop can
//! cooperatively terminate everything.

use std::sync::Arc;

use std::time::Duration;

use chain::{
    simulate_execute, ChainError, ExecutorSubmitter, RpcProvider, WatcherConfig, WsWatcher,
};
use domain::{Pool, PoolEvent, PoolId, PoolMeta};
use metrics::{counter, histogram};
use risk::{RiskManager, SystemClock, TradeOutcome, TradeProposal};
use strategy::{detect_spatial_opportunities, SizedTrade, SpatialParams};
use tokio::time::Instant;

use crate::calls::build_path_calls;
use crate::inclusion::{spawn_inclusion, spawn_inclusion_stub, InclusionConfig, PendingTx};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::registry::PoolRegistry;
use crate::shutdown::ShutdownToken;

const POOL_EVENT_CAPACITY: usize = 1024;
const POOL_UPDATE_CAPACITY: usize = 1024;
const INCLUSION_CAPACITY: usize = 128;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(300);

/// Stable string codes used as Prometheus label values for
/// `derrick_attempts_total{status=...}`. Kept centralized so the cardinality
/// is bounded and the dashboard queries don't break on typos.
mod attempt_status {
    pub const SIZED: &str = "sized";
    pub const RISK_REJECTED: &str = "risk_rejected";
    pub const SIMULATION_FAILED: &str = "simulation_failed";
    pub const SUBMITTED: &str = "submitted";
    pub const PAPER_TRADED: &str = "paper_traded";
}

/// Notification emitted by the state updater once a pool has been mutated.
#[derive(Debug, Clone, Copy)]
pub struct PoolStateUpdate {
    pub pool: PoolId,
    pub state_version: u64,
}

/// Everything the pipeline needs at spawn time. Optional fields gate the
/// corresponding task into a no-op when None.
pub struct PipelineConfig {
    pub registry: Arc<PoolRegistry>,
    pub risk: Arc<RiskManager<SystemClock>>,
    pub watcher_config: Option<WatcherConfig>,
    pub provider: Option<RpcProvider>,
    pub submitter: Option<ExecutorSubmitter>,
    /// Spatial-arb parameters. `None` keeps the detector in passive mode
    /// (logs updates without acting). Some(...) activates the full flow.
    pub spatial: Option<SpatialParams>,
    /// If true, sim runs as usual but submit is suppressed — the attempt is
    /// logged as `paper_traded`. Useful for shadow-running pre-launch.
    pub paper_trading: bool,
}

#[derive(Debug)]
pub struct PipelineHandles {
    pub watcher: JoinHandle<()>,
    pub state_updater: JoinHandle<()>,
    pub detector: JoinHandle<()>,
    pub inclusion: JoinHandle<()>,
}

pub fn spawn(cfg: PipelineConfig, shutdown: ShutdownToken) -> PipelineHandles {
    let (pool_event_tx, pool_event_rx) = mpsc::channel::<PoolEvent>(POOL_EVENT_CAPACITY);
    let (pool_update_tx, pool_update_rx) = mpsc::channel::<PoolStateUpdate>(POOL_UPDATE_CAPACITY);
    let (inclusion_tx, inclusion_rx) = mpsc::channel::<PendingTx>(INCLUSION_CAPACITY);

    let watcher = spawn_watcher(cfg.watcher_config, pool_event_tx, shutdown.clone());
    let state_updater = spawn_state_updater(
        cfg.registry.clone(),
        pool_event_rx,
        pool_update_tx,
        shutdown.clone(),
    );

    // Inclusion: real watcher if we have both a provider and a submitter;
    // otherwise a stub that drains the channel.
    let inclusion = match (cfg.provider.as_ref(), cfg.submitter.as_ref()) {
        (Some(provider), Some(_)) => {
            let inc_cfg = InclusionConfig {
                poll_interval: DEFAULT_POLL_INTERVAL,
                max_wait: DEFAULT_MAX_WAIT,
            };
            spawn_inclusion(
                inc_cfg,
                std::sync::Arc::new(provider.clone()),
                cfg.risk.clone(),
                inclusion_rx,
                shutdown.clone(),
            )
        }
        _ => spawn_inclusion_stub(inclusion_rx, shutdown.clone()),
    };

    let detector = spawn_detector(
        cfg.registry,
        cfg.risk,
        cfg.provider,
        cfg.submitter,
        cfg.spatial,
        cfg.paper_trading,
        inclusion_tx,
        pool_update_rx,
        shutdown,
    );

    PipelineHandles {
        watcher,
        state_updater,
        detector,
        inclusion,
    }
}

fn spawn_watcher(
    config: Option<WatcherConfig>,
    tx: mpsc::Sender<PoolEvent>,
    mut shutdown: ShutdownToken,
) -> JoinHandle<()> {
    tokio::spawn(
        async move {
            let Some(wcfg) = config else {
                info!("watcher disabled (no config) — idling until shutdown");
                shutdown.wait().await;
                return;
            };
            let watcher = WsWatcher::new(wcfg, tx);
            tokio::select! {
                () = shutdown.wait() => info!("watcher: shutdown received"),
                () = watcher.run() => info!("watcher.run() returned (channel closed)"),
            }
        }
        .instrument(info_span!("watcher")),
    )
}

fn spawn_state_updater(
    registry: Arc<PoolRegistry>,
    mut event_rx: mpsc::Receiver<PoolEvent>,
    update_tx: mpsc::Sender<PoolStateUpdate>,
    mut shutdown: ShutdownToken,
) -> JoinHandle<()> {
    tokio::spawn(
        async move {
            info!("state_updater up — applying PoolEvents to registry");
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.wait() => {
                        info!("state_updater: shutdown received");
                        return;
                    }
                    msg = event_rx.recv() => {
                        let Some(event) = msg else {
                            info!("state_updater: event channel closed");
                            return;
                        };
                        counter!(
                            "derrick_pool_events_total",
                            "kind" => event.kind.name(),
                        )
                        .increment(1);
                        match registry.apply_event(&event).await {
                            Ok(state_version) => {
                                let upd = PoolStateUpdate {
                                    pool: event.pool,
                                    state_version,
                                };
                                if update_tx.send(upd).await.is_err() {
                                    info!("state_updater: update consumer dropped");
                                    return;
                                }
                            }
                            Err(e) => {
                                counter!("derrick_apply_event_errors_total").increment(1);
                                warn!(
                                    error = %e,
                                    pool = ?event.pool,
                                    block = event.meta.block,
                                    "apply_event failed"
                                );
                            }
                        }
                    }
                }
            }
        }
        .instrument(info_span!("state_updater")),
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_detector(
    registry: Arc<PoolRegistry>,
    risk: Arc<RiskManager<SystemClock>>,
    provider: Option<RpcProvider>,
    submitter: Option<ExecutorSubmitter>,
    spatial: Option<SpatialParams>,
    paper_trading: bool,
    inclusion_tx: mpsc::Sender<PendingTx>,
    mut update_rx: mpsc::Receiver<PoolStateUpdate>,
    mut shutdown: ShutdownToken,
) -> JoinHandle<()> {
    tokio::spawn(
        async move {
            info!(
                submitter_wired = submitter.is_some(),
                provider_wired = provider.is_some(),
                spatial_active = spatial.is_some(),
                paper_trading,
                "detector up"
            );
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.wait() => {
                        info!("detector: shutdown received");
                        return;
                    }
                    msg = update_rx.recv() => {
                        let Some(update) = msg else {
                            info!("detector: update channel closed");
                            return;
                        };
                        let Some(params) = spatial.as_ref() else {
                            info!(
                                pool = ?update.pool,
                                version = update.state_version,
                                "passive mode (no spatial params)"
                            );
                            continue;
                        };
                        handle_update(
                            update,
                            &registry,
                            &risk,
                            provider.as_ref(),
                            submitter.as_ref(),
                            paper_trading,
                            &inclusion_tx,
                            params,
                        )
                        .await;
                    }
                }
            }
        }
        .instrument(info_span!("detector")),
    )
}

/// Handle one `PoolStateUpdate`: find candidate spatial arbitrages and gate
/// them through risk → simulation → submission.
#[allow(clippy::too_many_arguments)]
async fn handle_update(
    update: PoolStateUpdate,
    registry: &PoolRegistry,
    risk: &RiskManager<SystemClock>,
    provider: Option<&RpcProvider>,
    submitter: Option<&ExecutorSubmitter>,
    paper_trading: bool,
    inclusion_tx: &mpsc::Sender<PendingTx>,
    params: &SpatialParams,
) {
    let started = Instant::now();
    let Some((a, b)) = registry.pair_for(update.pool).await else {
        return;
    };

    let arcs = registry.pools_for_pair(a, b).await;
    if arcs.len() < 2 {
        return;
    }

    // Read-lock every pool sharing the pair, capture (SizedTrade, pool_metas)
    // pairs, then drop the locks before any I/O.
    let guards =
        futures::future::join_all(arcs.into_iter().map(tokio::sync::RwLock::read_owned)).await;
    let pool_refs: Vec<&dyn Pool> = guards
        .iter()
        .map(|g| {
            let b: &crate::registry::BoxedPool = g;
            let p: &(dyn Pool + Send + Sync) = b.as_ref();
            p as &dyn Pool
        })
        .collect();

    let opps = detect_spatial_opportunities(update.pool, &pool_refs, params);

    let detected: Vec<(SizedTrade, Vec<PoolMeta>)> = opps
        .into_iter()
        .filter_map(|sized| {
            let mut metas = Vec::with_capacity(sized.outcome.path.len());
            for hop in sized.outcome.path.hops() {
                let g = guards.iter().find(|g| g.meta().id == hop.pool)?;
                metas.push(g.meta().clone());
            }
            Some((sized, metas))
        })
        .collect();

    drop(guards);

    if detected.is_empty() {
        return;
    }
    info!(
        pool = ?update.pool,
        candidates = detected.len(),
        "detector found candidates"
    );

    for (sized, metas) in detected {
        process_sized(
            sized,
            metas,
            risk,
            provider,
            submitter,
            paper_trading,
            inclusion_tx,
        )
        .await;
    }

    histogram!("derrick_handle_update_duration_seconds").record(started.elapsed().as_secs_f64());
}

/// Gate one sized trade through the full pipeline:
///   risk → (sim → submit) → inclusion enqueue.
///
/// When `paper_trading` is true, simulation still runs but submit is
/// suppressed and the attempt is logged as `paper_traded`.
#[allow(clippy::too_many_arguments)]
async fn process_sized(
    sized: SizedTrade,
    pool_metas: Vec<PoolMeta>,
    risk: &RiskManager<SystemClock>,
    provider: Option<&RpcProvider>,
    submitter: Option<&ExecutorSubmitter>,
    paper_trading: bool,
    inclusion_tx: &mpsc::Sender<PendingTx>,
) {
    // Correlation id — threaded through every log line on this attempt so
    // operators can grep one attempt across detector/inclusion logs.
    let id = Uuid::new_v4();

    let expected_gross = sized.outcome.gross.abs();
    let min_profit = sized.outcome.net.abs();

    counter!("derrick_attempts_total", "status" => attempt_status::SIZED).increment(1);

    let prop = TradeProposal {
        token_in: sized.amount_in.token,
        amount_in: sized.amount_in.raw,
        expected_profit: sized.outcome.net,
    };
    if let Err(rejection) = risk.evaluate(&prop) {
        counter!("derrick_attempts_total", "status" => attempt_status::RISK_REJECTED).increment(1);
        info!(?id, reason = %rejection, "risk rejected");
        return;
    }

    let (Some(submitter), Some(provider)) = (submitter, provider) else {
        info!(
            ?id,
            "risk accepted; provider/submitter not configured — skipping submit"
        );
        return;
    };

    // Build the on-chain call sequence.
    let executor_addr = submitter.executor_address();
    let calls = match build_path_calls(&sized, &pool_metas, executor_addr) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, ?id, "build_path_calls failed");
            return;
        }
    };

    let token_in_felt = *sized.amount_in.token.as_felt();

    // Pre-submit simulation.
    let sim_started = Instant::now();
    let sim_result = simulate_execute(
        provider,
        submitter.client(),
        token_in_felt,
        min_profit,
        &calls,
        expected_gross,
    )
    .await;
    histogram!("derrick_simulate_duration_seconds").record(sim_started.elapsed().as_secs_f64());
    match sim_result {
        Ok(sim) => {
            info!(
                ?id,
                realized = %sim.realized_profit,
                divergence_bps = sim.divergence_bps,
                "simulation passed"
            );
        }
        Err(e) => {
            counter!(
                "derrick_attempts_total",
                "status" => attempt_status::SIMULATION_FAILED,
            )
            .increment(1);
            warn!(error = %e, ?id, "simulation failed");
            risk.record(TradeOutcome::SkippedSimulation {
                token: sized.amount_in.token,
            });
            return;
        }
    }

    // Paper-trading: simulation passed, stop here. Don't sign anything.
    if paper_trading {
        counter!("derrick_attempts_total", "status" => attempt_status::PAPER_TRADED).increment(1);
        info!(?id, "paper-trading mode — skipping on-chain submit");
        return;
    }

    // Sign + submit. Does NOT wait for inclusion — the inclusion watcher
    // polls the tx hash and closes the lifecycle.
    let submit_started = Instant::now();
    let submit_result = submitter.submit(token_in_felt, min_profit, &calls).await;
    histogram!("derrick_submit_duration_seconds").record(submit_started.elapsed().as_secs_f64());
    match submit_result {
        Ok(tx_hash) => {
            counter!("derrick_attempts_total", "status" => attempt_status::SUBMITTED).increment(1);
            info!(?id, tx_hash = %format!("{tx_hash:#x}"), "submitted");
            let pending = PendingTx {
                attempt_id: id,
                tx_hash,
                token: sized.amount_in.token,
                expected_gross,
            };
            if inclusion_tx.send(pending).await.is_err() {
                warn!(?id, "inclusion channel closed; tx_hash will not be tracked");
            }
        }
        Err(e) => {
            counter!(
                "derrick_attempts_total",
                "status" => attempt_status::SIMULATION_FAILED,
            )
            .increment(1);
            warn!(error = %e, ?id, "submit failed");
            match &e {
                ChainError::Reverted(_) => {
                    risk.record(TradeOutcome::Reverted {
                        token: sized.amount_in.token,
                        // No inclusion receipt yet — record zero gas. The
                        // inclusion-watcher path records the real fee.
                        gas_paid: domain::U256::zero(),
                    });
                }
                _ => {
                    risk.record(TradeOutcome::SkippedSimulation {
                        token: sized.amount_in.token,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::shutdown::Shutdown;
    use risk::{PerTokenLimits, RiskConfig};
    use std::collections::{HashMap, HashSet};

    fn dummy_risk() -> Arc<RiskManager<SystemClock>> {
        let cfg = RiskConfig {
            token_whitelist: HashSet::new(),
            per_token: HashMap::<domain::TokenId, PerTokenLimits>::new(),
            max_consecutive_failures: 5,
            circuit_breaker_pause_seconds: 60,
        };
        Arc::new(RiskManager::new(cfg, SystemClock))
    }

    #[tokio::test]
    async fn pipeline_tasks_exit_on_shutdown() {
        let shutdown = Shutdown::new();
        let cfg = PipelineConfig {
            registry: Arc::new(PoolRegistry::new()),
            risk: dummy_risk(),
            watcher_config: None,
            provider: None,
            submitter: None,
            spatial: None,
            paper_trading: false,
        };
        let handles = spawn(cfg, shutdown.token());

        tokio::task::yield_now().await;
        shutdown.broadcast();

        handles.watcher.await.unwrap();
        handles.state_updater.await.unwrap();
        handles.detector.await.unwrap();
        handles.inclusion.await.unwrap();
    }
}
