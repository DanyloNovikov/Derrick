//! Minimal Starknet provider abstraction.
//!
//! The trait is intentionally narrow: only the operations derrick needs.
//! The production implementation wraps `starknet-rs`'s `JsonRpcClient`;
//! tests can mock it.
//!
//! Step 6 ships the trait shape only. The RPC-backed implementation lands in
//! the chain-integration step once the bot can be exercised against devnet.

use async_trait::async_trait;
use primitive_types::U256;
use starknet_types_core::felt::Felt;

use crate::error::ChainError;

/// A single contract call (read-only or write).
#[derive(Clone, Debug)]
pub struct ProviderCall {
    pub to: Felt,
    pub selector: Felt,
    pub calldata: Vec<Felt>,
}

/// Which block the request should target.
///
/// `Pending` is the right choice for pre-submit simulation per project
/// `critical_rules`: it reflects in-flight transactions that `Latest` does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockTarget {
    Latest,
    Pending,
}

/// One event emitted on chain. Mirror of starknet-rs's `Event` but kept in
/// our own type so the trait stays decoupled from the underlying client.
#[derive(Debug, Clone)]
pub struct EventLog {
    pub from_address: Felt,
    pub keys: Vec<Felt>,
    pub data: Vec<Felt>,
}

/// Terminal-or-transient state of a transaction the bot has submitted.
#[derive(Debug, Clone)]
pub enum TxStatus {
    /// Node has not seen the hash yet — keep polling.
    NotFound,
    /// Hash known but not yet included in an accepted block — keep polling.
    Pending,
    /// Included and the contract returned successfully. `events` includes
    /// every event emitted by the transaction.
    Succeeded {
        actual_fee: U256,
        events: Vec<EventLog>,
    },
    /// Included but reverted on-chain. `reason` is the node-reported revert
    /// message; `actual_fee` is still owed by the operator.
    Reverted { reason: String, actual_fee: U256 },
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Read-only call. Used for `quote_in_onchain` and pre-submit simulation.
    async fn call(&self, call: ProviderCall, block: BlockTarget) -> Result<Vec<Felt>, ChainError>;

    /// Current nonce for the operator account.
    async fn get_nonce(&self, account: Felt, block: BlockTarget) -> Result<Felt, ChainError>;

    /// Poll the status of a submitted transaction.
    async fn get_tx_status(&self, tx_hash: Felt) -> Result<TxStatus, ChainError>;
}
