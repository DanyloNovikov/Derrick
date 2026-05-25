//! Shared, async-safe registry of pool adapters.
//!
//! The watcher writes (calling `apply_event` to mutate adapter state); the
//! detector reads (calling `quote_in_local` against the latest snapshot).
//! Per-pool `tokio::sync::RwLock`s let many concurrent readers proceed while
//! a writer holds exclusive access only briefly during state updates.
//!
//! A secondary `(TokenId, TokenId) → set of PoolId` index lets the detector
//! find all pools matching a given token pair in O(1) without scanning.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use domain::{Pool, PoolEvent, PoolId, StateError, TokenId};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("pool {0:?} is not registered")]
    NotFound(PoolId),

    #[error(transparent)]
    State(#[from] StateError),
}

/// Boxed trait object alias to keep signatures readable.
pub type BoxedPool = Box<dyn Pool + Send + Sync>;

#[derive(Default)]
struct Inner {
    pools: HashMap<PoolId, Arc<RwLock<BoxedPool>>>,
    by_pair: HashMap<(TokenId, TokenId), HashSet<PoolId>>,
    /// Reverse lookup: id → canonical pair, used so we can drop a pool from
    /// `by_pair` without re-reading its meta (which would require a write
    /// lock on the pool's `RwLock`).
    pair_of: HashMap<PoolId, (TokenId, TokenId)>,
}

#[derive(Default)]
pub struct PoolRegistry {
    inner: RwLock<Inner>,
}

impl std::fmt::Debug for PoolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolRegistry").finish_non_exhaustive()
    }
}

fn canonical_pair(a: TokenId, b: TokenId) -> (TokenId, TokenId) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

impl PoolRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
        }
    }

    /// Insert a pool. Replaces any existing entry with the same id.
    pub async fn add(&self, pool: BoxedPool) {
        let meta = pool.meta().clone();
        let pair = canonical_pair(meta.token0, meta.token1);
        let mut inner = self.inner.write().await;

        // If this id is already registered, scrub the old pair index entry
        // before re-inserting so we don't leave a dangling reference.
        if let Some(old_pair) = inner.pair_of.get(&meta.id).copied() {
            if let Some(set) = inner.by_pair.get_mut(&old_pair) {
                set.remove(&meta.id);
                if set.is_empty() {
                    inner.by_pair.remove(&old_pair);
                }
            }
        }

        inner.pools.insert(meta.id, Arc::new(RwLock::new(pool)));
        inner.by_pair.entry(pair).or_default().insert(meta.id);
        inner.pair_of.insert(meta.id, pair);
    }

    pub async fn contains(&self, id: PoolId) -> bool {
        self.inner.read().await.pools.contains_key(&id)
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.pools.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.pools.is_empty()
    }

    /// Return the canonical `(token, token)` pair this pool trades, if known.
    pub async fn pair_for(&self, id: PoolId) -> Option<(TokenId, TokenId)> {
        self.inner.read().await.pair_of.get(&id).copied()
    }

    /// All pools that pair `(a, b)` (order-insensitive), returned as
    /// reference-counted handles. Caller is expected to `read_owned()` each
    /// for the duration of a quote / detection pass.
    pub async fn pools_for_pair(&self, a: TokenId, b: TokenId) -> Vec<Arc<RwLock<BoxedPool>>> {
        let pair = canonical_pair(a, b);
        let inner = self.inner.read().await;
        let Some(ids) = inner.by_pair.get(&pair) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| inner.pools.get(id).cloned())
            .collect()
    }

    /// Apply an event to the relevant pool. Returns the pool's new
    /// `state_version` on success.
    pub async fn apply_event(&self, event: &PoolEvent) -> Result<u64, RegistryError> {
        let pool_arc = {
            let inner = self.inner.read().await;
            inner.pools.get(&event.pool).cloned()
        };
        let Some(pool_arc) = pool_arc else {
            return Err(RegistryError::NotFound(event.pool));
        };
        let mut guard = pool_arc.write().await;
        guard.apply_event(event)?;
        Ok(guard.state_version())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use async_trait::async_trait;
    use domain::{
        Amount, ContractAddress, DexKind, EventMeta, FeeBps, Felt, PoolEventKind, PoolMeta, Quote,
        QuoteError,
    };

    #[derive(Debug)]
    struct DummyPool {
        meta: PoolMeta,
        version: u64,
    }

    impl DummyPool {
        fn new(id: PoolId, t0: TokenId, t1: TokenId) -> Self {
            Self {
                meta: PoolMeta {
                    id,
                    token0: t0,
                    token1: t1,
                },
                version: 0,
            }
        }
    }

    #[async_trait]
    impl Pool for DummyPool {
        fn meta(&self) -> &PoolMeta {
            &self.meta
        }
        fn state_version(&self) -> u64 {
            self.version
        }
        fn quote_in_local(&self, _a: Amount) -> Result<Quote, QuoteError> {
            Err(QuoteError::StateNotLoaded)
        }
        fn quote_out_local(&self, _a: Amount) -> Result<Quote, QuoteError> {
            Err(QuoteError::StateNotLoaded)
        }
        async fn quote_in_onchain(&self, _a: Amount) -> Result<Quote, QuoteError> {
            Err(QuoteError::LocalUnavailable)
        }
        fn apply_event(&mut self, event: &PoolEvent) -> Result<(), StateError> {
            if event.pool != self.meta.id {
                return Err(StateError::WrongPool);
            }
            self.version += 1;
            Ok(())
        }
    }

    fn pid(addr: u64) -> PoolId {
        PoolId {
            address: ContractAddress::new(Felt::from(addr)),
            dex: DexKind::JediSwapV1,
            fee: FeeBps::new(30),
        }
    }

    fn tok(n: u64) -> TokenId {
        TokenId::new(ContractAddress::new(Felt::from(n)))
    }

    #[tokio::test]
    async fn add_then_contains() {
        let reg = PoolRegistry::new();
        assert!(reg.is_empty().await);
        let id = pid(1);
        reg.add(Box::new(DummyPool::new(id, tok(10), tok(20))))
            .await;
        assert!(reg.contains(id).await);
        assert_eq!(reg.len().await, 1);
    }

    #[tokio::test]
    async fn apply_event_unknown_pool_errors() {
        let reg = PoolRegistry::new();
        let ev = PoolEvent {
            pool: pid(99),
            meta: EventMeta {
                block: 1,
                tx_index: 0,
                event_index: 0,
            },
            kind: PoolEventKind::Sync,
            data: vec![],
        };
        let r = reg.apply_event(&ev).await;
        assert!(matches!(r, Err(RegistryError::NotFound(_))));
    }

    #[tokio::test]
    async fn apply_event_bumps_state_version() {
        let reg = PoolRegistry::new();
        let id = pid(1);
        reg.add(Box::new(DummyPool::new(id, tok(10), tok(20))))
            .await;
        let ev = PoolEvent {
            pool: id,
            meta: EventMeta {
                block: 1,
                tx_index: 0,
                event_index: 0,
            },
            kind: PoolEventKind::Sync,
            data: vec![],
        };
        let v1 = reg.apply_event(&ev).await.unwrap();
        let v2 = reg.apply_event(&ev).await.unwrap();
        assert_eq!(v1, 1);
        assert_eq!(v2, 2);
    }

    #[tokio::test]
    async fn pair_for_returns_canonical_pair() {
        let reg = PoolRegistry::new();
        let usdc = tok(1);
        let eth = tok(2);
        let id = pid(10);
        reg.add(Box::new(DummyPool::new(id, eth, usdc))) // order intentionally flipped
            .await;
        // Canonical pair is (usdc, eth) since usdc < eth.
        assert_eq!(reg.pair_for(id).await, Some((usdc, eth)));
    }

    #[tokio::test]
    async fn pools_for_pair_finds_all_orientations() {
        let reg = PoolRegistry::new();
        let usdc = tok(1);
        let eth = tok(2);
        let strk = tok(3);
        reg.add(Box::new(DummyPool::new(pid(10), usdc, eth))).await;
        reg.add(Box::new(DummyPool::new(pid(11), eth, usdc))) // same pair, opposite order
            .await;
        reg.add(Box::new(DummyPool::new(pid(12), usdc, strk))).await;

        let matches = reg.pools_for_pair(usdc, eth).await;
        assert_eq!(matches.len(), 2);

        let strk_matches = reg.pools_for_pair(usdc, strk).await;
        assert_eq!(strk_matches.len(), 1);

        let none = reg.pools_for_pair(eth, strk).await;
        assert_eq!(none.len(), 0);
    }

    #[tokio::test]
    async fn replace_same_id_does_not_double_index() {
        let reg = PoolRegistry::new();
        let usdc = tok(1);
        let eth = tok(2);
        let strk = tok(3);
        let id = pid(10);
        reg.add(Box::new(DummyPool::new(id, usdc, eth))).await;
        // Replace with a different pair: should drop from by_pair[(usdc, eth)]
        // and add to by_pair[(usdc, strk)].
        reg.add(Box::new(DummyPool::new(id, usdc, strk))).await;
        assert_eq!(reg.pools_for_pair(usdc, eth).await.len(), 0);
        assert_eq!(reg.pools_for_pair(usdc, strk).await.len(), 1);
        assert_eq!(reg.len().await, 1);
    }
}
