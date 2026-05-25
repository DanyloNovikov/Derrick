use crate::amount::Amount;
use crate::pool::PoolId;

/// Result of asking a pool: "if I put in X, how much Y do I get out?".
///
/// `state_version` is the monotonic version of the pool state at quote time.
/// Downstream code that compounds quotes (multi-hop path simulation) compares
/// versions to detect stale snapshots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Quote {
    pub pool: PoolId,
    pub amount_in: Amount,
    pub amount_out: Amount,
    /// Adapter-reported gas estimate for executing this swap, in fri (STRK base units).
    /// `0` if the adapter cannot estimate without an on-chain call.
    pub gas_estimate: u64,
    pub state_version: u64,
}
