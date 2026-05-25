//! Tracing + Prometheus exporter initialization.

use anyhow::Context;
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;
use tracing_subscriber::EnvFilter;

use crate::config::ObservabilityConfig;

/// Initialize `tracing` (JSON output, env-filter from config) and start a
/// Prometheus HTTP exporter on `metrics_bind`. Returns once both are ready.
///
/// Must be called exactly once per process. Calling twice will fail the
/// `tracing_subscriber::set_global_default`.
pub fn init_observability(cfg: &ObservabilityConfig) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(&cfg.log_level)
        .with_context(|| format!("invalid log_level filter: {}", cfg.log_level))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .with_target(true)
        .with_current_span(true)
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing init failed: {e}"))?;

    let addr: SocketAddr = cfg
        .metrics_bind
        .parse()
        .with_context(|| format!("invalid metrics_bind: {}", cfg.metrics_bind))?;
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .install()
        .context("prometheus exporter failed to install")?;

    tracing::info!(addr = %addr, "prometheus exporter ready");
    Ok(())
}
