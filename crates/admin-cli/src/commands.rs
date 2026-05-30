//! Command implementations.
//!
//! Each `run_*` function takes parsed CLI args + a `Context` (RPC URL,
//! resolved addresses, chain id, config), runs the operation, and returns
//! `Result<()>`. Read commands only need a `ReadClient`; write commands
//! lazily build a `WriteClient` from env.

use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::{anyhow, bail, Context as _, Result};
use bot::config::{AppConfig, TokenConfig};
use chain::{TxStatus, SWAP_SELECTOR, TRANSFER_SELECTOR};
use primitive_types::U256;
use starknet::macros::selector;
use starknet_types_core::felt::Felt;

use crate::cli::{
    IsAllowedArgs, SetupArgs, TargetArgs, TransferOwnershipArgs, WithdrawArgs,
};
use crate::client::{sel, ReadClient, WriteClient};

/// Resolved CLI context — flags merged with config file values.
pub struct Context {
    pub rpc_url: String,
    pub executor: Felt,
    pub owner: Felt,
    pub chain_id: Felt,
    pub config: AppConfig,
    pub no_wait: bool,
}

// ─── read commands ───────────────────────────────────────────────────────

pub async fn run_status(ctx: &Context) -> Result<()> {
    let r = ReadClient::new(&ctx.rpc_url, ctx.executor)?;

    let owner = r.owner().await.context("read owner")?;

    println!("Executor: {:#x}", ctx.executor);
    println!("  owner (on-chain):  {owner:#x}");
    println!("  owner (configured): {:#x}", ctx.owner);
    if owner != ctx.owner {
        println!("  ⚠️  on-chain owner != configured owner — check your config");
    }
    println!();

    if ctx.config.tokens.is_empty() {
        println!("No [[tokens]] in config — nothing to query balances for.");
        return Ok(());
    }

    println!("Balances:");
    for t in &ctx.config.tokens {
        let token_addr = match Felt::from_hex(&t.address) {
            Ok(f) => f,
            Err(e) => {
                println!("  {:6} [{}]  invalid hex address: {e}", t.symbol, t.address);
                continue;
            }
        };
        match r.balance_of(token_addr, ctx.executor).await {
            Ok(bal) => {
                let pretty = format_amount(bal, t.decimals);
                println!(
                    "  {:6} {:>22}  raw={}  ({} decimals)",
                    t.symbol, pretty, bal, t.decimals
                );
            }
            Err(e) => println!("  {:6} balance_of failed: {e}", t.symbol),
        }
    }
    Ok(())
}

pub async fn run_is_allowed(ctx: &Context, args: &IsAllowedArgs) -> Result<()> {
    let r = ReadClient::new(&ctx.rpc_url, ctx.executor)?;
    let target = parse_felt(&args.target).context("--target")?;
    let selector = resolve_selector(&args.selector).context("--selector")?;
    let allowed = r.is_target_allowed(target, selector).await?;
    println!(
        "{:#x} / {:#x}: {}",
        target,
        selector,
        if allowed { "ALLOWED" } else { "not allowed" }
    );
    Ok(())
}

// ─── write commands ──────────────────────────────────────────────────────

pub async fn run_allow_target(ctx: &Context, args: &TargetArgs) -> Result<()> {
    let target = parse_felt(&args.target).context("--target")?;
    let selector = resolve_selector(&args.selector).context("--selector")?;
    let w = write_client(ctx)?;
    let tx = w.allow_target(target, selector).await?;
    println!(
        "Sent allow_target({target:#x}, {selector:#x}) tx: {tx:#x}"
    );
    await_inclusion(&w, ctx, tx).await
}

pub async fn run_disallow_target(ctx: &Context, args: &TargetArgs) -> Result<()> {
    let target = parse_felt(&args.target).context("--target")?;
    let selector = resolve_selector(&args.selector).context("--selector")?;
    let w = write_client(ctx)?;
    let tx = w.disallow_target(target, selector).await?;
    println!(
        "Sent disallow_target({target:#x}, {selector:#x}) tx: {tx:#x}"
    );
    await_inclusion(&w, ctx, tx).await
}

pub async fn run_withdraw(ctx: &Context, args: &WithdrawArgs) -> Result<()> {
    let token = resolve_token(&args.token, &ctx.config.tokens).context("--token")?;
    let to = parse_felt(&args.to).context("--to")?;
    let amount = U256::from_dec_str(&args.amount).map_err(|e| anyhow!("--amount: {e}"))?;
    let w = write_client(ctx)?;
    let tx = w.withdraw(token, to, amount).await?;
    println!(
        "Sent withdraw(token={token:#x}, to={to:#x}, amount={amount}) tx: {tx:#x}"
    );
    await_inclusion(&w, ctx, tx).await
}

pub async fn run_transfer_ownership(
    ctx: &Context,
    args: &TransferOwnershipArgs,
) -> Result<()> {
    let new_owner = parse_felt(&args.new_owner).context("--new-owner")?;
    if !args.yes_i_mean_it {
        bail!(
            "transfer-ownership is irreversible and the contract has no two-step accept. \
             Pass --yes-i-mean-it after verifying that {new_owner:#x} is a deployed account you control."
        );
    }
    let w = write_client(ctx)?;
    let tx = w.transfer_ownership(new_owner).await?;
    println!(
        "Sent transfer_ownership({new_owner:#x}) tx: {tx:#x}"
    );
    await_inclusion(&w, ctx, tx).await
}

// ─── setup command (batch) ───────────────────────────────────────────────

pub async fn run_setup(ctx: &Context, args: &SetupArgs) -> Result<()> {
    let plan = plan_setup(&ctx.config)?;
    if plan.is_empty() {
        println!("Nothing to do — no tokens or pools in config.");
        return Ok(());
    }

    println!("Planned allow_target calls ({}):", plan.len());
    for (i, (kind, target, sel_name, sel_felt)) in plan.iter().enumerate() {
        println!(
            "  {:>2}. {:>5}  target={:#x}  selector={} ({:#x})",
            i + 1,
            kind,
            target,
            sel_name,
            sel_felt
        );
    }
    println!();

    if args.dry_run {
        println!("--dry-run set; not sending.");
        return Ok(());
    }

    if !args.yes && !confirm("Send as single atomic invoke_v3? [y/N] ")? {
        println!("Aborted.");
        return Ok(());
    }

    let calls: Vec<(Felt, Vec<Felt>)> = plan
        .iter()
        .map(|(_, target, _, sel_felt)| (sel::ALLOW_TARGET, vec![*target, *sel_felt]))
        .collect();

    let w = write_client(ctx)?;
    let tx = w.invoke_many(calls).await?;
    println!("Sent batch tx: {tx:#x}");
    await_inclusion(&w, ctx, tx).await
}

/// Build the (kind, `target_addr`, `selector_name`, `selector_felt`) list to whitelist.
fn plan_setup(
    cfg: &AppConfig,
) -> Result<Vec<(&'static str, Felt, &'static str, Felt)>> {
    let mut out = Vec::new();

    // Tokens — allow `transfer` on each. The bot's first-hop transfer
    // (calls.rs::build_erc20_transfer) triggers a whitelist check on
    // (token_addr, transfer_selector).
    for t in &cfg.tokens {
        let addr = Felt::from_hex(&t.address)
            .map_err(|e| anyhow!("token {} address '{}': {e}", t.symbol, t.address))?;
        out.push(("token", addr, "transfer", TRANSFER_SELECTOR));
    }

    // Pools — allow the DEX-appropriate swap selector. For now only `swap`
    // is wired (matches calls.rs::build_uniswap_v2_swap and the Cairo
    // Uniswap-v2 fork convention). New DEX kinds need their own selector.
    for p in &cfg.pools {
        let addr = Felt::from_hex(&p.address)
            .map_err(|e| anyhow!("pool {} address '{}': {e}", p.dex, p.address))?;
        let (sel_name, sel_felt) = pool_selector_for_dex(&p.dex)?;
        out.push(("pool", addr, sel_name, sel_felt));
    }

    Ok(out)
}

fn pool_selector_for_dex(dex: &str) -> Result<(&'static str, Felt)> {
    match dex {
        // Uniswap-v2 fork family: single `swap(amount0, amount1, to, data)` entry point.
        "jediswap_v1" | "myswap_v1" | "tenkswap" => Ok(("swap", SWAP_SELECTOR)),
        // Other DEX kinds need bespoke executor adapters first; whitelisting
        // them here is meaningless until calls.rs knows their shape.
        other => bail!(
            "setup: dex '{other}' has no executor adapter yet — extend calls.rs first, \
             then add it here"
        ),
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

fn write_client(ctx: &Context) -> Result<WriteClient> {
    WriteClient::from_env(&ctx.rpc_url, ctx.owner, ctx.executor, ctx.chain_id)
}

async fn await_inclusion(w: &WriteClient, ctx: &Context, tx: Felt) -> Result<()> {
    if ctx.no_wait {
        println!("--no-wait set; not blocking on inclusion.");
        return Ok(());
    }
    println!("Waiting for inclusion…");
    match w.wait_for_inclusion(tx).await? {
        TxStatus::Succeeded { actual_fee, .. } => {
            println!("✓ included; actual_fee={actual_fee} (raw)");
            Ok(())
        }
        TxStatus::Reverted { reason, actual_fee } => {
            bail!(
                "tx reverted on-chain: {reason}\n  (you still paid actual_fee={actual_fee} raw)"
            );
        }
        TxStatus::Pending | TxStatus::NotFound => {
            bail!("polling exited without a terminal status — should not happen")
        }
    }
}

fn parse_felt(s: &str) -> Result<Felt> {
    Felt::from_hex(s.trim()).map_err(|e| anyhow!("invalid hex felt '{s}': {e}"))
}

/// Resolve a selector name (`transfer`, `swap`, `approve`, `balance_of`,
/// plus the executor's own admin selectors) or fall through to raw hex.
fn resolve_selector(s: &str) -> Result<Felt> {
    let s = s.trim();
    Ok(match s {
        // ERC20 / DEX
        "transfer" => TRANSFER_SELECTOR,
        "swap" => SWAP_SELECTOR,
        "approve" => selector!("approve"),
        "balance_of" => selector!("balance_of"),
        // Executor admin (mostly for inspection / dry-runs)
        "execute" => selector!("execute"),
        "allow_target" => sel::ALLOW_TARGET,
        "disallow_target" => sel::DISALLOW_TARGET,
        "withdraw" => sel::WITHDRAW,
        // Raw hex passthrough
        _ if s.starts_with("0x") => parse_felt(s)?,
        other => bail!(
            "unknown selector name '{other}' — pass a hex felt (`0x...`) or one of: \
             transfer, swap, approve, balance_of, execute, allow_target, \
             disallow_target, withdraw"
        ),
    })
}

/// Resolve a token by symbol (from config) or raw hex.
fn resolve_token(s: &str, tokens: &[TokenConfig]) -> Result<Felt> {
    if s.starts_with("0x") {
        return parse_felt(s);
    }
    let by_symbol: HashMap<&str, &str> = tokens
        .iter()
        .map(|t| (t.symbol.as_str(), t.address.as_str()))
        .collect();
    let hex = by_symbol.get(s).ok_or_else(|| {
        anyhow!(
            "no token with symbol '{s}' in config; pass a hex address instead. \
             Available: {:?}",
            by_symbol.keys().collect::<Vec<_>>()
        )
    })?;
    parse_felt(hex)
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(matches!(buf.trim(), "y" | "Y" | "yes" | "YES"))
}

/// Pretty-print a raw token amount with the given number of decimals.
/// Doesn't allocate big strings; just splits at the decimal boundary.
fn format_amount(raw: U256, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    let s = raw.to_string();
    let d = decimals as usize;
    if s.len() <= d {
        let zeros = "0".repeat(d - s.len());
        format!("0.{zeros}{s}")
    } else {
        let cut = s.len() - d;
        format!("{}.{}", &s[..cut], &s[cut..])
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn format_amount_handles_small() {
        assert_eq!(format_amount(U256::from(1_u64), 6), "0.000001");
        assert_eq!(format_amount(U256::from(1_000_000_u64), 6), "1.000000");
        assert_eq!(format_amount(U256::from(1_234_567_u64), 6), "1.234567");
    }

    #[test]
    fn format_amount_handles_zero_decimals() {
        assert_eq!(format_amount(U256::from(42_u64), 0), "42");
    }

    #[test]
    fn resolve_selector_named() {
        assert_eq!(resolve_selector("transfer").unwrap(), TRANSFER_SELECTOR);
        assert_eq!(resolve_selector("swap").unwrap(), SWAP_SELECTOR);
        assert_eq!(resolve_selector("allow_target").unwrap(), sel::ALLOW_TARGET);
    }

    #[test]
    fn resolve_selector_raw_hex() {
        let f = resolve_selector("0x1234").unwrap();
        assert_eq!(f, Felt::from_hex("0x1234").unwrap());
    }

    #[test]
    fn resolve_selector_unknown_rejected() {
        assert!(resolve_selector("definitely_not_a_selector").is_err());
    }

    #[test]
    fn resolve_token_by_symbol() {
        let tokens = vec![TokenConfig {
            symbol: "USDC".into(),
            address: "0x53c91253".into(),
            decimals: 6,
            risk: None,
        }];
        let f = resolve_token("USDC", &tokens).unwrap();
        assert_eq!(f, Felt::from_hex("0x53c91253").unwrap());
    }

    #[test]
    fn resolve_token_raw_hex() {
        let f = resolve_token("0xabcd", &[]).unwrap();
        assert_eq!(f, Felt::from_hex("0xabcd").unwrap());
    }

    #[test]
    fn resolve_token_unknown_symbol_rejected() {
        let r = resolve_token("UNKNOWN", &[]);
        assert!(r.is_err());
    }

    #[test]
    fn plan_setup_empty_when_no_tokens_pools() {
        let cfg = sample_empty_config();
        let plan = plan_setup(&cfg).unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_setup_lists_tokens_then_pools() {
        use bot::config::PoolConfig;
        let mut cfg = sample_empty_config();
        cfg.tokens = vec![TokenConfig {
            symbol: "USDC".into(),
            address: "0x01".into(),
            decimals: 6,
            risk: None,
        }];
        cfg.pools = vec![PoolConfig {
            dex: "jediswap_v1".into(),
            address: "0x0a".into(),
            token0: "0x01".into(),
            token1: "0x02".into(),
            fee_bps: 30,
        }];
        let plan = plan_setup(&cfg).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, "token");
        assert_eq!(plan[0].3, TRANSFER_SELECTOR);
        assert_eq!(plan[1].0, "pool");
        assert_eq!(plan[1].3, SWAP_SELECTOR);
    }

    #[test]
    fn plan_setup_rejects_unsupported_dex() {
        use bot::config::PoolConfig;
        let mut cfg = sample_empty_config();
        cfg.pools = vec![PoolConfig {
            dex: "ekubo".into(),
            address: "0x0a".into(),
            token0: "0x01".into(),
            token1: "0x02".into(),
            fee_bps: 100,
        }];
        let r = plan_setup(&cfg);
        assert!(r.is_err());
    }

    fn sample_empty_config() -> AppConfig {
        use bot::config::{
            ExecutorConfig, NetworkConfig, ObservabilityConfig, RiskConfig, StrategyConfig,
        };
        AppConfig {
            network: NetworkConfig {
                rpc_url: "x".into(),
                ws_url: None,
                chain_id: "SN_MAIN".into(),
            },
            observability: ObservabilityConfig {
                log_level: "info".into(),
                metrics_bind: "127.0.0.1:9090".into(),
            },
            executor: ExecutorConfig {
                contract_address: "0x0".into(),
                owner_account_address: "0x0".into(),
                chain_id: "SN_MAIN".into(),
                paper_trading: false,
            },
            risk: RiskConfig {
                max_consecutive_failures: 5,
                circuit_breaker_pause_seconds: 600,
            },
            strategy: StrategyConfig {
                safety_margin_bps: 30,
                sizer_iterations: 40,
            },
            tokens: vec![],
            pools: vec![],
            spatial: None,
        }
    }
}
