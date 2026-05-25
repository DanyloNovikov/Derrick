use async_trait::async_trait;

use crate::amount::Amount;
use crate::error::{QuoteError, StateError};
use crate::pool::{PoolEvent, PoolMeta};
use crate::quote::Quote;

/// Adapter for a single liquidity pool. Implementations live in `dex`.
///
/// Implementations hold a local snapshot of pool state updated via `apply_event`.
/// Quotes have two flavors:
///   * `quote_*_local`  — runs against the snapshot, NO network I/O. Hot path.
///   * `quote_in_onchain` — calls the pool contract; used for final verification
///     immediately before submitting a transaction.
///
/// For concentrated-liquidity pools, the `_local` methods MAY return
/// `QuoteError::LocalUnavailable` if tick state has not been loaded — callers
/// must then fall back to `quote_in_onchain`.
#[async_trait]
pub trait Pool: Send + Sync + std::fmt::Debug {
    fn meta(&self) -> &PoolMeta;

    /// Monotonic state version. Increments on every successfully-applied event.
    /// Quotes carry this so stale snapshots are detectable downstream.
    fn state_version(&self) -> u64;

    /// Quote using the cached snapshot. Must not perform I/O.
    fn quote_in_local(&self, amount_in: Amount) -> Result<Quote, QuoteError>;

    /// Inverse: how much `amount_in` is required to obtain `amount_out`?
    /// Same constraints as `quote_in_local`.
    fn quote_out_local(&self, amount_out: Amount) -> Result<Quote, QuoteError>;

    /// Quote via on-chain contract call. May be slow.
    async fn quote_in_onchain(&self, amount_in: Amount) -> Result<Quote, QuoteError>;

    /// Apply an event from `price_watcher`. Bumps `state_version` on success.
    /// Idempotent on duplicates (returns `StateError::Duplicate`, state unchanged).
    fn apply_event(&mut self, event: &PoolEvent) -> Result<(), StateError>;
}
