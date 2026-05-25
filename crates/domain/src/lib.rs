//! Core types and traits shared across the derrick workspace.
//!
//! This crate is deliberately I/O-free: no async runtime, no network, no DB.
//! Everything here is data types, validated constructors, and trait shapes.

pub mod amount;
pub mod error;
pub mod opportunity;
pub mod pool;
pub mod quote;
pub mod token;
pub mod traits;

pub use amount::{Amount, SignedAmount};
pub use error::{AmountError, CoreError, QuoteError, StateError};
pub use opportunity::{Hop, Opportunity, Path};
pub use pool::{DexKind, EventMeta, FeeBps, PoolEvent, PoolEventKind, PoolId, PoolMeta};
pub use quote::Quote;
pub use token::{ContractAddress, Decimals, Symbol, Token, TokenId};
pub use traits::Pool;

pub use primitive_types::U256;
pub use starknet_types_core::felt::Felt;
