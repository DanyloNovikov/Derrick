//! derrick-admin — operational CLI for the on-chain `DerrickExecutor`.

mod cli;
mod client;
mod commands;

use anyhow::{anyhow, Context as _, Result};
use clap::Parser;
use starknet_types_core::felt::Felt;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};
use crate::commands::Context;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    let cfg = bot::config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config))?;

    let ctx = build_context(&cli, cfg)?;

    match &cli.command {
        Command::Status => commands::run_status(&ctx).await,
        Command::IsAllowed(args) => commands::run_is_allowed(&ctx, args).await,
        Command::AllowTarget(args) => commands::run_allow_target(&ctx, args).await,
        Command::DisallowTarget(args) => commands::run_disallow_target(&ctx, args).await,
        Command::Withdraw(args) => commands::run_withdraw(&ctx, args).await,
        Command::TransferOwnership(args) => commands::run_transfer_ownership(&ctx, args).await,
        Command::Setup(args) => commands::run_setup(&ctx, args).await,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn build_context(cli: &Cli, cfg: bot::config::AppConfig) -> Result<Context> {
    let rpc_url = cli
        .rpc_url
        .clone()
        .unwrap_or_else(|| cfg.network.rpc_url.clone());

    let executor_hex = cli
        .executor
        .clone()
        .unwrap_or_else(|| cfg.executor.contract_address.clone());
    if executor_hex == "0x0" || executor_hex.is_empty() {
        anyhow::bail!(
            "executor address is placeholder; set --executor or [executor].contract_address"
        );
    }

    let owner_hex = cli
        .owner
        .clone()
        .unwrap_or_else(|| cfg.executor.owner_account_address.clone());
    if owner_hex == "0x0" || owner_hex.is_empty() {
        anyhow::bail!(
            "owner address is placeholder; set --owner or [executor].owner_account_address"
        );
    }

    let chain_id_str = cli
        .chain_id
        .clone()
        .unwrap_or_else(|| cfg.executor.chain_id.clone());

    let executor = Felt::from_hex(&executor_hex)
        .map_err(|e| anyhow!("executor address '{executor_hex}': {e}"))?;
    let owner = Felt::from_hex(&owner_hex)
        .map_err(|e| anyhow!("owner address '{owner_hex}': {e}"))?;
    let chain_id = parse_chain_id(&chain_id_str)?;

    Ok(Context {
        rpc_url,
        executor,
        owner,
        chain_id,
        config: cfg,
        no_wait: cli.no_wait,
    })
}

/// Resolve `"SN_MAIN"` / `"SN_SEPOLIA"` / raw hex felt to a chain id `Felt`.
/// Mirrors `bot::wiring::parse_chain_id` — kept local to avoid pulling that
/// module's full dep tree.
fn parse_chain_id(s: &str) -> Result<Felt> {
    match s {
        "SN_MAIN" => Felt::from_hex("0x534e5f4d41494e").map_err(|e| anyhow!("{e}")),
        "SN_SEPOLIA" => Felt::from_hex("0x534e5f5345504f4c4941").map_err(|e| anyhow!("{e}")),
        _ => Felt::from_hex(s).map_err(|e| anyhow!("invalid chain_id '{s}': {e}")),
    }
}
