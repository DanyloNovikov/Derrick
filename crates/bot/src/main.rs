use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing::{error, info, warn};

use bot::{
    config, observability,
    pipeline::{self, PipelineConfig},
    registry::PoolRegistry,
    shutdown::Shutdown,
    wiring,
};
use ledger::Ledger;
use risk::{RiskManager, SystemClock};

#[derive(Parser, Debug)]
#[command(
    name = "derrick",
    about = "Starknet arbitrage bot",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long, env = "DERRICK_CONFIG", default_value = "config/default.toml")]
    config: String,
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // linear bot setup reads more clearly inline than split across helpers
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let cfg = config::load(&cli.config)
        .with_context(|| format!("failed to load config from {}", cli.config))?;
    observability::init_observability(&cfg.observability)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = cli.config,
        "derrick starting"
    );

    let ledger = Ledger::connect(&cfg.database.url)
        .await
        .with_context(|| format!("connect to {}", cfg.database.url))?;
    ledger
        .run_migrations()
        .await
        .context("apply ledger migrations")?;
    info!("ledger connected and migrated");

    // Pool registry — populated from [[pools]] config entries.
    let registry = Arc::new(PoolRegistry::new());
    let parsed_pools = wiring::parse_pools(&cfg.pools);
    let registered = wiring::register_pools(&registry, &parsed_pools).await;
    info!(
        configured = cfg.pools.len(),
        registered, "pool registry ready"
    );

    // Token registry — feeds the risk manager's whitelist + per-token limits.
    let parsed_tokens = wiring::parse_tokens(&cfg.tokens);
    info!(
        configured = cfg.tokens.len(),
        whitelisted = parsed_tokens.len(),
        with_limits = parsed_tokens.iter().filter(|t| t.risk.is_some()).count(),
        "token registry ready"
    );

    // Risk manager.
    let risk_config = wiring::build_risk_config(&cfg, &parsed_tokens);
    let risk = Arc::new(RiskManager::new(risk_config, SystemClock));
    info!("risk manager initialized");

    // RPC provider.
    let provider = match wiring::build_provider(&cfg.network.rpc_url) {
        Ok(p) => {
            info!(url = %cfg.network.rpc_url, "rpc provider ready");
            Some(p)
        }
        Err(e) => {
            warn!(error = %e, "rpc provider unavailable; on-chain reads disabled");
            None
        }
    };

    // Submitter — requires OPERATOR_PRIVATE_KEY env + non-placeholder addrs.
    let submitter = match wiring::build_submitter(
        &cfg.network.rpc_url,
        &cfg.executor.contract_address,
        &cfg.executor.operator_account_address,
        &cfg.executor.chain_id,
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "submitter not constructed; on-chain submission disabled");
            None
        }
    };
    info!(submitter_wired = submitter.is_some(), "submitter status");

    // WS watcher config — only built when both ws_url and subscriptions exist.
    let subscriptions = wiring::build_subscriptions(&parsed_pools);
    let watcher_config = wiring::build_watcher_config(cfg.network.ws_url.as_deref(), subscriptions);
    info!(watcher_wired = watcher_config.is_some(), "watcher status");

    // Spatial-arb params (Optional). None → passive mode.
    let spatial = if let Some(s) = cfg.spatial.as_ref() {
        match wiring::build_spatial_params(s) {
            Ok(p) => {
                info!("spatial params loaded — detector active");
                Some(p)
            }
            Err(e) => {
                warn!(error = %e, "failed to parse spatial config; passive mode");
                None
            }
        }
    } else {
        info!("no spatial config; detector in passive mode");
        None
    };

    let pipeline_config = PipelineConfig {
        registry,
        risk,
        ledger,
        watcher_config,
        provider,
        submitter,
        spatial,
        paper_trading: cfg.executor.paper_trading,
    };

    let shutdown = Shutdown::new();
    let handles = pipeline::spawn(pipeline_config, shutdown.token());

    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("ctrl-c received, broadcasting shutdown"),
        Err(e) => error!(error = %e, "signal handler failed, broadcasting shutdown anyway"),
    }
    shutdown.broadcast();

    if let Err(e) = handles.watcher.await {
        error!(error = %e, "watcher join failed");
    }
    if let Err(e) = handles.state_updater.await {
        error!(error = %e, "state_updater join failed");
    }
    if let Err(e) = handles.detector.await {
        error!(error = %e, "detector join failed");
    }
    if let Err(e) = handles.inclusion.await {
        error!(error = %e, "inclusion join failed");
    }

    info!("derrick stopped");
    Ok(())
}
