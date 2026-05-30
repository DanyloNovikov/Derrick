//! Starknet RPC/WS bridge for derrick.
//!
//! For Step 6 the focus is the [`executor`] module — a pure-function client
//! that builds calldata for the Cairo `DerrickExecutor` contract. The [`provider`]
//! module exposes a thin trait that the bot's main wiring will plug into
//! `starknet-rs` in a later step; tests can mock it.

pub mod error;
pub mod executor;
pub mod provider;
pub mod rpc;
pub mod selectors;
pub mod simulator;
pub mod submitter;
pub mod watcher;

pub use error::ChainError;
pub use executor::{ExecutorCall, ExecutorClient};
pub use provider::{BlockTarget, EventLog, Provider, ProviderCall, TxStatus};
pub use rpc::RpcProvider;
pub use selectors::{
    APPROVE_SELECTOR, BALANCE_OF_SELECTOR, EXECUTE_SELECTOR, SWAP_SELECTOR, TRANSFER_SELECTOR,
};
pub use simulator::{simulate_execute, SimulationResult, MAX_DIVERGENCE_BPS};
pub use submitter::ExecutorSubmitter;
pub use watcher::{
    decode_event, PoolEventSelectors, PoolSubscription, WatcherConfig, WatcherError, WsWatcher,
};
