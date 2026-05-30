//! clap CLI surface for `derrick-admin`.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "derrick-admin",
    version,
    about = "Admin CLI for the on-chain DerrickExecutor contract",
    long_about = "Read and modify DerrickExecutor state: whitelist (allow/disallow), withdraw \
                  funds, transfer ownership, batch setup from config. Write commands require \
                  OWNER_PRIVATE_KEY in env."
)]
pub struct Cli {
    /// Path to derrick TOML config.
    #[arg(long, default_value = "config/default.toml", env = "DERRICK_CONFIG")]
    pub config: String,

    /// Override RPC URL (otherwise taken from `[network].rpc_url`).
    #[arg(long, env = "DERRICK__NETWORK__RPC_URL")]
    pub rpc_url: Option<String>,

    /// Override executor contract address.
    #[arg(long, env = "DERRICK__EXECUTOR__CONTRACT_ADDRESS")]
    pub executor: Option<String>,

    /// Override owner account address.
    #[arg(long, env = "DERRICK__EXECUTOR__OWNER_ACCOUNT_ADDRESS")]
    pub owner: Option<String>,

    /// Override chain id (`SN_MAIN` / `SN_SEPOLIA` / raw hex).
    #[arg(long, env = "DERRICK__NETWORK__CHAIN_ID")]
    pub chain_id: Option<String>,

    /// After sending a write, do NOT block on inclusion.
    #[arg(long, global = true)]
    pub no_wait: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print contract state: owner, balances for every token in config.
    Status,

    /// Check whether a (target, selector) pair is whitelisted.
    IsAllowed(IsAllowedArgs),

    /// Whitelist a (target, selector). `selector` accepts either a name
    /// (`transfer`, `swap`, ...) or a raw `0x...` felt.
    AllowTarget(TargetArgs),

    /// Remove a (target, selector) from the whitelist.
    DisallowTarget(TargetArgs),

    /// Withdraw `amount` of `token` from the executor to `to`.
    Withdraw(WithdrawArgs),

    /// Two-step caveat: this is a SINGLE-STEP transfer in the current Cairo
    /// contract. Make absolutely sure `new_owner` controls a working signer.
    TransferOwnership(TransferOwnershipArgs),

    /// Batch-whitelist every token (with `transfer`) and every pool (with the
    /// DEX-appropriate selector) from the config file. Single atomic tx.
    Setup(SetupArgs),
}

#[derive(Args, Debug)]
pub struct IsAllowedArgs {
    /// Target contract address (hex felt).
    #[arg(long)]
    pub target: String,
    /// Selector — name (`transfer`, `swap`) or raw hex felt.
    #[arg(long)]
    pub selector: String,
}

#[derive(Args, Debug)]
pub struct TargetArgs {
    /// Target contract address (hex felt).
    #[arg(long)]
    pub target: String,
    /// Selector — name (`transfer`, `swap`) or raw hex felt.
    #[arg(long)]
    pub selector: String,
}

#[derive(Args, Debug)]
pub struct WithdrawArgs {
    /// Token contract address (hex felt) — or a symbol present in config (`USDC`, `ETH`, ...).
    #[arg(long)]
    pub token: String,
    /// Recipient address (hex felt).
    #[arg(long)]
    pub to: String,
    /// Amount in raw token units (decimal). NOT human-decimal: pass `1000000`
    /// for 1 USDC, not `1.0`. This is the same convention `[risk]` limits use.
    #[arg(long)]
    pub amount: String,
}

#[derive(Args, Debug)]
pub struct TransferOwnershipArgs {
    /// New owner address (hex felt). Must be a deployed Starknet account.
    #[arg(long)]
    pub new_owner: String,
    /// Required confirmation. Pass `--yes-i-mean-it` to skip the interactive prompt.
    #[arg(long)]
    pub yes_i_mean_it: bool,
}

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Don't actually send — just print the planned calls.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}
