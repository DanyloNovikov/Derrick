//! DEX adapters implementing `domain::Pool`.
//!
//! Each adapter holds a local snapshot of pool state and answers quote
//! questions either from the snapshot (`quote_*_local`) or via an injected
//! on-chain quoter (`quote_in_onchain`). The chain-layer implementation of
//! [`OnChainQuoter`] lives in `chain`.

pub mod factory;
pub mod jediswap_v1;
pub mod quoter;

pub use factory::{build_pool, BoxedPool};
pub use jediswap_v1::JediSwapV1Pool;
pub use quoter::{NoopQuoter, OnChainQuoter, SharedQuoter};
