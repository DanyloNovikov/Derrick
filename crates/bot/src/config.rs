//! TOML-driven config loading.
//!
//! Sources, in order of precedence (later overrides earlier):
//!   1. The TOML file passed on the CLI (default: `config/default.toml`).
//!   2. Environment variables prefixed `DERRICK__` with `__` as the level
//!      separator (e.g., `DERRICK__DATABASE__URL`).

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub network: NetworkConfig,
    pub database: DatabaseConfig,
    pub observability: ObservabilityConfig,
    pub executor: ExecutorConfig,
    pub risk: RiskConfig,
    pub strategy: StrategyConfig,

    /// Token registry. Each entry whitelists the token for trading and
    /// optionally carries per-token risk limits.
    #[serde(default)]
    pub tokens: Vec<TokenConfig>,

    /// Pool registrations. Empty → registry stays empty → detector idle.
    #[serde(default)]
    pub pools: Vec<PoolConfig>,

    /// Spatial-arb parameters. `None` → pipeline runs in passive mode
    /// (only logs `PoolStateUpdate`, doesn't act).
    pub spatial: Option<SpatialConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    pub rpc_url: String,
    pub ws_url: Option<String>,
    pub chain_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig {
    /// Env-filter directive for `tracing-subscriber` (e.g.,
    /// `info,derrick=debug`).
    pub log_level: String,
    /// Where the Prometheus exporter binds. `host:port`.
    pub metrics_bind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutorConfig {
    /// Address of the on-chain `ArbExecutor` contract.
    pub contract_address: String,
    /// Operator account address. The private key is read from the
    /// `OPERATOR_PRIVATE_KEY` env var, never from config.
    pub operator_account_address: String,
    /// Chain id: `"SN_MAIN"`, `"SN_SEPOLIA"`, or a raw hex felt.
    #[serde(default = "default_chain_id")]
    pub chain_id: String,
    /// When true, simulation runs as usual but submit is suppressed; the
    /// attempt is recorded with status `paper_traded`. Use for pre-launch
    /// shadow runs against a live RPC.
    #[serde(default)]
    pub paper_trading: bool,
}

fn default_chain_id() -> String {
    "SN_MAIN".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    /// DEX kind: `snake_case` string matching `DexKind` (e.g., `"jediswap_v1"`).
    pub dex: String,
    /// Pool contract address as hex felt.
    pub address: String,
    /// Token0 address (must match pool's on-chain `token0()` to keep the
    /// `Sync(reserve0, reserve1)` event decoded into the right slots).
    pub token0: String,
    pub token1: String,
    /// Pool fee in basis points (1 bps = 0.01%).
    pub fee_bps: u32,
}

/// One tradeable token: metadata + (optional) per-token risk limits.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenConfig {
    /// Display symbol. Must validate via `Symbol::new` (1-16 ASCII alphanum +
    /// `-`/`_`).
    pub symbol: String,
    /// Token contract address as hex felt.
    pub address: String,
    pub decimals: u8,
    /// Risk limits scoped to this token. Without it, the token is
    /// whitelisted but `RiskManager::evaluate` will reject every proposal
    /// with `NoLimitsConfigured`. Strict by design — production safety.
    pub risk: Option<RiskLimitsConfig>,
}

/// Per-token risk limits. All amounts are raw token units (decimal string).
#[derive(Debug, Clone, Deserialize)]
pub struct RiskLimitsConfig {
    pub max_position: String,
    pub min_profit: String,
    pub daily_max_loss: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpatialConfig {
    /// Token the bot starts and ends each spatial cycle in (e.g., USDC).
    pub start_token: String,
    /// Estimated gas cost per trade, in raw `start_token` units (decimal).
    pub gas_cost: String,
    /// Bps of `amount_in` to use as the safety margin floor.
    pub safety_margin_bps: u32,
    /// Lower bound of the ternary search, in raw `start_token` units (decimal).
    pub min_amount_in: String,
    /// Upper bound of the ternary search.
    pub max_amount_in: String,
    /// Iteration cap for the ternary search.
    pub sizer_iterations: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub max_consecutive_failures: u32,
    pub circuit_breaker_pause_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig {
    pub safety_margin_bps: u32,
    pub sizer_iterations: u32,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config crate error: {0}")]
    Config(#[from] config::ConfigError),
}

/// Load and deserialize the config from the given path.
pub fn load(path: &str) -> Result<AppConfig, ConfigError> {
    let cfg = config::Config::builder()
        .add_source(config::File::with_name(path))
        .add_source(
            config::Environment::with_prefix("DERRICK")
                .separator("__")
                .try_parsing(true),
        )
        .build()?;
    let app: AppConfig = cfg.try_deserialize()?;
    Ok(app)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_temp_config(body: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "derrick-test-{}.toml",
            uuid::Uuid::new_v4().simple()
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn loads_full_config_from_toml() {
        let body = r#"
[network]
rpc_url = "https://example/rpc"
ws_url = "wss://example/ws"
chain_id = "SN_MAIN"

[database]
url = "postgres://x"

[observability]
log_level = "info,derrick=debug"
metrics_bind = "127.0.0.1:9090"

[executor]
contract_address = "0xdead"
operator_account_address = "0xbeef"

[risk]
max_consecutive_failures = 5
circuit_breaker_pause_seconds = 600

[strategy]
safety_margin_bps = 30
sizer_iterations = 40
        "#;
        let p = write_temp_config(body);
        let cfg = load(p.to_str().unwrap()).unwrap();
        std::fs::remove_file(p).ok();

        assert_eq!(cfg.network.rpc_url, "https://example/rpc");
        assert_eq!(cfg.network.ws_url.as_deref(), Some("wss://example/ws"));
        assert_eq!(cfg.network.chain_id, "SN_MAIN");
        assert_eq!(cfg.database.url, "postgres://x");
        assert_eq!(cfg.observability.log_level, "info,derrick=debug");
        assert_eq!(cfg.observability.metrics_bind, "127.0.0.1:9090");
        assert_eq!(cfg.executor.contract_address, "0xdead");
        assert_eq!(cfg.executor.operator_account_address, "0xbeef");
        assert_eq!(cfg.risk.max_consecutive_failures, 5);
        assert_eq!(cfg.risk.circuit_breaker_pause_seconds, 600);
        assert_eq!(cfg.strategy.safety_margin_bps, 30);
        assert_eq!(cfg.strategy.sizer_iterations, 40);
    }

    #[test]
    fn rejects_missing_required_field() {
        // [network] block missing → load fails
        let body = r#"
[database]
url = "x"

[observability]
log_level = "info"
metrics_bind = "127.0.0.1:9090"

[executor]
contract_address = "0xdead"
operator_account_address = "0xbeef"

[risk]
max_consecutive_failures = 5
circuit_breaker_pause_seconds = 600

[strategy]
safety_margin_bps = 30
sizer_iterations = 40
        "#;
        let p = write_temp_config(body);
        let res = load(p.to_str().unwrap());
        std::fs::remove_file(p).ok();
        assert!(res.is_err());
    }
}
