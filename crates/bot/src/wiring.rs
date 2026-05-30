//! Build runtime components from `AppConfig`.
//!
//! Lives separately from `main.rs` so the parsing/conversion logic is
//! unit-testable without spinning up the full binary.

use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chain::{ExecutorSubmitter, PoolEventSelectors, PoolSubscription, RpcProvider, WatcherConfig};
use dex::{build_pool, NoopQuoter, SharedQuoter};
use domain::{
    Amount, ContractAddress, Decimals, DexKind, FeeBps, Felt, PoolId, PoolMeta, Symbol, Token,
    TokenId, U256,
};
use risk::{PerTokenLimits, RiskConfig as RiskCfg};
use starknet::macros::selector;
use std::collections::{HashMap, HashSet};
use strategy::{ProfitParams, SpatialParams};
use tracing::warn;

use crate::config::{AppConfig, PoolConfig, RiskLimitsConfig, SpatialConfig, TokenConfig};
use crate::registry::PoolRegistry;

const RECONNECT_INITIAL_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 30_000;

/// Parsed pool entry ready for registration + WS subscription.
pub struct ParsedPool {
    pub meta: PoolMeta,
    pub selectors: Option<PoolEventSelectors>,
}

/// Convert each `PoolConfig` into a `ParsedPool` (or log + skip on parse error).
pub fn parse_pools(pool_cfgs: &[PoolConfig]) -> Vec<ParsedPool> {
    let mut out = Vec::with_capacity(pool_cfgs.len());
    for pc in pool_cfgs {
        match parse_one_pool(pc) {
            Ok(parsed) => out.push(parsed),
            Err(e) => warn!(error = %e, dex = %pc.dex, addr = %pc.address, "skipping invalid pool"),
        }
    }
    out
}

fn parse_one_pool(pc: &PoolConfig) -> Result<ParsedPool> {
    let dex = parse_dex_kind(&pc.dex)?;
    let address = parse_address(&pc.address).context("pool address")?;
    let token0 = parse_token(&pc.token0).context("token0")?;
    let token1 = parse_token(&pc.token1).context("token1")?;
    if token0 == token1 {
        bail!("token0 and token1 must differ");
    }
    let meta = PoolMeta {
        id: PoolId {
            address,
            dex,
            fee: FeeBps::new(pc.fee_bps),
        },
        token0,
        token1,
    };
    let selectors = event_selectors_for(dex);
    Ok(ParsedPool { meta, selectors })
}

fn parse_dex_kind(s: &str) -> Result<DexKind> {
    Ok(match s {
        "ekubo" => DexKind::Ekubo,
        "jediswap_v1" => DexKind::JediSwapV1,
        "jediswap_v2" => DexKind::JediSwapV2,
        "myswap_v1" => DexKind::MySwapV1,
        "myswap_v2" => DexKind::MySwapV2,
        "tenkswap" => DexKind::TenkSwap,
        "sithswap_stable" => DexKind::SithSwapStable,
        "sithswap_volatile" => DexKind::SithSwapVolatile,
        "haiko" => DexKind::Haiko,
        other => bail!("unknown dex kind: {other}"),
    })
}

fn parse_address(hex: &str) -> Result<ContractAddress> {
    let felt = Felt::from_hex(hex).map_err(|e| anyhow!("invalid hex: {e}"))?;
    Ok(ContractAddress::new(felt))
}

fn parse_token(hex: &str) -> Result<TokenId> {
    Ok(TokenId::new(parse_address(hex)?))
}

fn parse_u256_dec(s: &str) -> Result<U256> {
    U256::from_dec_str(s).map_err(|e| anyhow!("invalid decimal U256 '{s}': {e}"))
}

/// Per-DEX event selectors. Returns None for kinds whose event shape we
/// haven't wired into [`PoolEventSelectors`] yet.
fn event_selectors_for(dex: DexKind) -> Option<PoolEventSelectors> {
    match dex {
        // Uniswap v2 fork — Sync/Swap/Mint/Burn.
        DexKind::JediSwapV1 | DexKind::MySwapV1 | DexKind::TenkSwap => Some(PoolEventSelectors {
            sync: selector!("Sync"),
            swap: selector!("Swap"),
            mint: selector!("Mint"),
            burn: selector!("Burn"),
        }),
        _ => None,
    }
}

/// Register parsed pools into the registry. Uses [`NoopQuoter`] for the
/// on-chain fallback (CPMM adapters work off Sync-cached reserves).
pub async fn register_pools(registry: &PoolRegistry, parsed: &[ParsedPool]) -> usize {
    let quoter: SharedQuoter = Arc::new(NoopQuoter);
    let mut registered = 0;
    for p in parsed {
        if let Some(pool) = build_pool(p.meta.clone(), quoter.clone()) {
            registry.add(pool).await;
            registered += 1;
        } else {
            warn!(?p.meta.id, "no adapter for this DEX kind; pool skipped");
        }
    }
    registered
}

/// Collect WS subscriptions for the pools whose DEX kind has known selectors.
pub fn build_subscriptions(parsed: &[ParsedPool]) -> Vec<PoolSubscription> {
    parsed
        .iter()
        .filter_map(|p| {
            p.selectors.as_ref().map(|sel| PoolSubscription {
                pool: p.meta.id,
                selectors: *sel,
            })
        })
        .collect()
}

/// Build a `WatcherConfig` only if both a WS URL and at least one subscription
/// are present. Otherwise the bot runs without on-chain event ingestion.
pub fn build_watcher_config(
    ws_url: Option<&str>,
    subscriptions: Vec<PoolSubscription>,
) -> Option<WatcherConfig> {
    let url = ws_url?.to_string();
    if subscriptions.is_empty() {
        return None;
    }
    Some(WatcherConfig {
        ws_url: url,
        subscriptions,
        reconnect_initial_delay_ms: RECONNECT_INITIAL_MS,
        reconnect_max_delay_ms: RECONNECT_MAX_MS,
    })
}

/// Translate the TOML `[spatial]` block into runtime `SpatialParams`.
pub fn build_spatial_params(sp: &SpatialConfig) -> Result<SpatialParams> {
    let token = parse_token(&sp.start_token).context("spatial.start_token")?;
    let gas_cost = parse_u256_dec(&sp.gas_cost).context("spatial.gas_cost")?;
    let min_amount_in = parse_u256_dec(&sp.min_amount_in).context("spatial.min_amount_in")?;
    let max_amount_in = parse_u256_dec(&sp.max_amount_in).context("spatial.max_amount_in")?;
    Ok(SpatialParams {
        start_token: token,
        profit: ProfitParams {
            gas_cost: Amount::new(token, gas_cost),
            safety_margin_bps: sp.safety_margin_bps,
        },
        min_amount_in: Amount::new(token, min_amount_in),
        max_amount_in: Amount::new(token, max_amount_in),
        sizer_iterations: sp.sizer_iterations,
    })
}

/// Resolve `"SN_MAIN"` / `"SN_SEPOLIA"` / raw hex felt to a chain id `Felt`.
pub fn parse_chain_id(s: &str) -> Result<Felt> {
    match s {
        // ASCII bytes of the literal — hardcoded to avoid pulling chain-id
        // constants from the starknet crate just for this.
        "SN_MAIN" => Felt::from_hex("0x534e5f4d41494e").map_err(|e| anyhow!("{e}")),
        "SN_SEPOLIA" => Felt::from_hex("0x534e5f5345504f4c4941").map_err(|e| anyhow!("{e}")),
        _ => Felt::from_hex(s).map_err(|e| anyhow!("invalid chain_id '{s}': {e}")),
    }
}

/// Build an [`ExecutorSubmitter`] if (a) `OWNER_PRIVATE_KEY` env var is
/// set, (b) executor contract address is not the placeholder `0x0`, and
/// (c) all hex values parse. Returns `Ok(None)` (with a warn) when any
/// precondition fails — the bot continues without a submitter.
///
/// `OWNER_PRIVATE_KEY` is the private key of the Oracle wallet — the same
/// wallet that owns the on-chain `DerrickExecutor` contract (the only address
/// the contract's `execute()` accepts as caller).
pub fn build_submitter(
    rpc_url: &str,
    contract_address: &str,
    owner_address: &str,
    chain_id: &str,
) -> Result<Option<ExecutorSubmitter>> {
    let Ok(private_key_hex) = std::env::var("OWNER_PRIVATE_KEY") else {
        warn!("OWNER_PRIVATE_KEY not set; submitter disabled");
        return Ok(None);
    };
    if contract_address == "0x0" || contract_address.is_empty() {
        warn!("executor.contract_address is placeholder; submitter disabled");
        return Ok(None);
    }
    let executor_addr =
        Felt::from_hex(contract_address).map_err(|e| anyhow!("executor.contract_address: {e}"))?;
    let owner_addr = Felt::from_hex(owner_address)
        .map_err(|e| anyhow!("executor.owner_account_address: {e}"))?;
    let private_key = Felt::from_hex(&private_key_hex)
        .map_err(|_| anyhow!("OWNER_PRIVATE_KEY is not a valid hex felt"))?;
    let chain = parse_chain_id(chain_id)?;
    let s = ExecutorSubmitter::new(rpc_url, owner_addr, private_key, executor_addr, chain)?;
    Ok(Some(s))
}

/// One token with its metadata and (optional) per-token risk limits.
#[derive(Debug, Clone)]
pub struct ParsedToken {
    pub id: TokenId,
    pub token: Token,
    pub risk: Option<PerTokenLimits>,
}

/// Parse `[[tokens]]` entries. Bad entries are logged and skipped.
/// Later duplicate-address entries silently override earlier ones — last
/// wins (so an env override file can refine a base file).
pub fn parse_tokens(token_cfgs: &[TokenConfig]) -> Vec<ParsedToken> {
    let mut out: HashMap<TokenId, ParsedToken> = HashMap::with_capacity(token_cfgs.len());
    for tc in token_cfgs {
        match parse_one_token(tc) {
            Ok(parsed) => {
                out.insert(parsed.id, parsed);
            }
            Err(e) => warn!(
                error = %e,
                symbol = %tc.symbol,
                addr = %tc.address,
                "skipping invalid token"
            ),
        }
    }
    out.into_values().collect()
}

fn parse_one_token(tc: &TokenConfig) -> Result<ParsedToken> {
    let id = parse_token(&tc.address).context("token address")?;
    let symbol = Symbol::new(tc.symbol.clone())
        .map_err(|e| anyhow!("invalid token symbol '{}': {e}", tc.symbol))?;
    let token = Token {
        id,
        symbol,
        decimals: Decimals::new(tc.decimals),
    };
    let risk = tc
        .risk
        .as_ref()
        .map(parse_risk_limits)
        .transpose()
        .context("risk limits")?;
    Ok(ParsedToken { id, token, risk })
}

fn parse_risk_limits(rc: &RiskLimitsConfig) -> Result<PerTokenLimits> {
    Ok(PerTokenLimits {
        max_position: parse_u256_dec(&rc.max_position).context("max_position")?,
        min_profit: parse_u256_dec(&rc.min_profit).context("min_profit")?,
        daily_max_loss: parse_u256_dec(&rc.daily_max_loss).context("daily_max_loss")?,
    })
}

/// Build the production `risk::RiskConfig` from the bot config plus
/// parsed tokens. `token_whitelist` includes every successfully-parsed token;
/// `per_token` includes those that also specified risk limits. Tokens
/// without limits stay whitelisted but rejected at evaluate-time
/// (`NoLimitsConfigured`) — surface a config bug loudly.
pub fn build_risk_config(cfg: &AppConfig, parsed_tokens: &[ParsedToken]) -> RiskCfg {
    let token_whitelist: HashSet<TokenId> = parsed_tokens.iter().map(|t| t.id).collect();
    let per_token: HashMap<TokenId, PerTokenLimits> = parsed_tokens
        .iter()
        .filter_map(|t| t.risk.as_ref().map(|r| (t.id, r.clone())))
        .collect();
    RiskCfg {
        token_whitelist,
        per_token,
        max_consecutive_failures: cfg.risk.max_consecutive_failures,
        circuit_breaker_pause_seconds: cfg.risk.circuit_breaker_pause_seconds,
    }
}

/// Construct an `RpcProvider`. `Url::parse` failures are propagated as
/// `Err`; for the bot we treat any RPC URL trouble as a config bug.
pub fn build_provider(rpc_url: &str) -> Result<RpcProvider> {
    RpcProvider::new(rpc_url).map_err(|e| anyhow!("rpc provider: {e}"))
}

// Trait-impl-compatibility shim — Felt::from_str ergonomics live elsewhere;
// we never call FromStr here so silence the unused-import lint if it fires.
#[allow(dead_code)]
fn _phantom_use_fromstr() {
    let _ = <Felt as FromStr>::from_str;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::config::PoolConfig;

    #[test]
    fn parses_jediswap_v1_pool() {
        let pc = PoolConfig {
            dex: "jediswap_v1".into(),
            address: "0x0a".into(),
            token0: "0x01".into(),
            token1: "0x02".into(),
            fee_bps: 30,
        };
        let parsed = parse_pools(&[pc]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].meta.id.dex, DexKind::JediSwapV1);
        assert_eq!(parsed[0].meta.id.fee.get(), 30);
        assert!(parsed[0].selectors.is_some());
    }

    #[test]
    fn unknown_dex_is_skipped() {
        let pc = PoolConfig {
            dex: "bogus_dex".into(),
            address: "0x0a".into(),
            token0: "0x01".into(),
            token1: "0x02".into(),
            fee_bps: 30,
        };
        let parsed = parse_pools(&[pc]);
        assert!(parsed.is_empty());
    }

    #[test]
    fn ekubo_pool_parses_but_has_no_selectors() {
        let pc = PoolConfig {
            dex: "ekubo".into(),
            address: "0x0a".into(),
            token0: "0x01".into(),
            token1: "0x02".into(),
            fee_bps: 100,
        };
        let parsed = parse_pools(&[pc]);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].selectors.is_none()); // Sync/Swap shape not v2-fork
    }

    #[test]
    fn equal_tokens_rejected() {
        let pc = PoolConfig {
            dex: "jediswap_v1".into(),
            address: "0x0a".into(),
            token0: "0x01".into(),
            token1: "0x01".into(),
            fee_bps: 30,
        };
        let parsed = parse_pools(&[pc]);
        assert!(parsed.is_empty());
    }

    #[test]
    fn build_spatial_params_round_trips_numbers() {
        let sc = SpatialConfig {
            start_token: "0x01".into(),
            gas_cost: "100000".into(),
            safety_margin_bps: 30,
            min_amount_in: "10000000".into(),
            max_amount_in: "10000000000".into(),
            sizer_iterations: 40,
        };
        let sp = build_spatial_params(&sc).unwrap();
        assert_eq!(sp.profit.gas_cost.raw, U256::from(100_000u64));
        assert_eq!(sp.profit.safety_margin_bps, 30);
        assert_eq!(sp.min_amount_in.raw, U256::from(10_000_000u64));
        assert_eq!(sp.max_amount_in.raw, U256::from(10_000_000_000u64));
        assert_eq!(sp.sizer_iterations, 40);
    }

    #[test]
    fn parse_chain_id_named() {
        let m = parse_chain_id("SN_MAIN").unwrap();
        let m_hex = format!("{m:#x}");
        assert!(m_hex.ends_with("4d41494e"), "got {m_hex}");
    }

    #[test]
    fn parse_chain_id_raw_hex() {
        let v = parse_chain_id("0xdead").unwrap();
        assert_eq!(format!("{v:#x}"), "0xdead");
    }

    #[test]
    fn build_subscriptions_filters_out_unsupported() {
        let parsed = parse_pools(&[
            PoolConfig {
                dex: "jediswap_v1".into(),
                address: "0x0a".into(),
                token0: "0x01".into(),
                token1: "0x02".into(),
                fee_bps: 30,
            },
            PoolConfig {
                dex: "ekubo".into(),
                address: "0x0b".into(),
                token0: "0x01".into(),
                token1: "0x02".into(),
                fee_bps: 30,
            },
        ]);
        let subs = build_subscriptions(&parsed);
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn watcher_config_requires_ws_url_and_subs() {
        assert!(build_watcher_config(None, vec![]).is_none());
        assert!(build_watcher_config(Some("ws://x"), vec![]).is_none());
    }

    #[test]
    fn parses_token_with_risk() {
        let tc = TokenConfig {
            symbol: "USDC".into(),
            address: "0x01".into(),
            decimals: 6,
            risk: Some(RiskLimitsConfig {
                max_position: "10000000000".into(),
                min_profit: "1000000".into(),
                daily_max_loss: "100000000".into(),
            }),
        };
        let out = parse_tokens(&[tc]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].token.symbol.as_str(), "USDC");
        assert_eq!(out[0].token.decimals.get(), 6);
        let limits = out[0].risk.as_ref().unwrap();
        assert_eq!(limits.max_position, U256::from(10_000_000_000_u64));
        assert_eq!(limits.min_profit, U256::from(1_000_000_u64));
        assert_eq!(limits.daily_max_loss, U256::from(100_000_000_u64));
    }

    #[test]
    fn parses_token_without_risk() {
        let tc = TokenConfig {
            symbol: "ETH".into(),
            address: "0x02".into(),
            decimals: 18,
            risk: None,
        };
        let out = parse_tokens(&[tc]);
        assert_eq!(out.len(), 1);
        assert!(out[0].risk.is_none());
    }

    #[test]
    fn invalid_symbol_skipped() {
        let tc = TokenConfig {
            symbol: "BAD SYMBOL".into(), // space → rejected by Symbol::new
            address: "0x01".into(),
            decimals: 6,
            risk: None,
        };
        let out = parse_tokens(&[tc]);
        assert!(out.is_empty());
    }

    #[test]
    fn duplicate_address_last_wins() {
        let a = TokenConfig {
            symbol: "USDC".into(),
            address: "0x01".into(),
            decimals: 6,
            risk: None,
        };
        let b = TokenConfig {
            symbol: "USDC".into(),
            address: "0x01".into(),
            decimals: 6,
            risk: Some(RiskLimitsConfig {
                max_position: "1".into(),
                min_profit: "1".into(),
                daily_max_loss: "1".into(),
            }),
        };
        let out = parse_tokens(&[a, b]);
        assert_eq!(out.len(), 1);
        assert!(out[0].risk.is_some(), "second entry's risk should win");
    }

    #[test]
    fn build_risk_config_populates_whitelist_and_limits() {
        let cfg = AppConfig {
            network: crate::config::NetworkConfig {
                rpc_url: "x".into(),
                ws_url: None,
                chain_id: "SN_MAIN".into(),
            },
            observability: crate::config::ObservabilityConfig {
                log_level: "info".into(),
                metrics_bind: "127.0.0.1:9090".into(),
            },
            executor: crate::config::ExecutorConfig {
                contract_address: "0x0".into(),
                owner_account_address: "0x0".into(),
                chain_id: "SN_MAIN".into(),
                paper_trading: false,
            },
            risk: crate::config::RiskConfig {
                max_consecutive_failures: 7,
                circuit_breaker_pause_seconds: 42,
            },
            strategy: crate::config::StrategyConfig {
                safety_margin_bps: 30,
                sizer_iterations: 40,
            },
            tokens: vec![],
            pools: vec![],
            spatial: None,
        };
        let parsed = vec![
            ParsedToken {
                id: TokenId::new(ContractAddress::new(Felt::from(1u64))),
                token: Token {
                    id: TokenId::new(ContractAddress::new(Felt::from(1u64))),
                    symbol: Symbol::new("USDC").unwrap(),
                    decimals: Decimals::new(6),
                },
                risk: Some(PerTokenLimits {
                    max_position: U256::from(1000u64),
                    min_profit: U256::from(10u64),
                    daily_max_loss: U256::from(100u64),
                }),
            },
            ParsedToken {
                id: TokenId::new(ContractAddress::new(Felt::from(2u64))),
                token: Token {
                    id: TokenId::new(ContractAddress::new(Felt::from(2u64))),
                    symbol: Symbol::new("ETH").unwrap(),
                    decimals: Decimals::new(18),
                },
                risk: None, // whitelisted but no limits
            },
        ];
        let rc = build_risk_config(&cfg, &parsed);
        assert_eq!(rc.token_whitelist.len(), 2);
        assert_eq!(rc.per_token.len(), 1); // only USDC has limits
        assert_eq!(rc.max_consecutive_failures, 7);
        assert_eq!(rc.circuit_breaker_pause_seconds, 42);
    }

    #[tokio::test]
    async fn register_pools_inserts_supported_only() {
        let parsed = parse_pools(&[
            PoolConfig {
                dex: "jediswap_v1".into(),
                address: "0x0a".into(),
                token0: "0x01".into(),
                token1: "0x02".into(),
                fee_bps: 30,
            },
            PoolConfig {
                dex: "ekubo".into(),
                address: "0x0b".into(),
                token0: "0x01".into(),
                token1: "0x02".into(),
                fee_bps: 30,
            },
        ]);
        let reg = PoolRegistry::new();
        let n = register_pools(&reg, &parsed).await;
        assert_eq!(n, 1);
        assert_eq!(reg.len().await, 1);
    }
}
