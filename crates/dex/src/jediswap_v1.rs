//! `JediSwap` v1 adapter — a Uniswap v2 fork on Starknet.
//!
//! State model:
//!   * `reserve0` / `reserve1` are kept in sync with the on-chain reserves.
//!   * Updates come from `Sync(reserve0: u256, reserve1: u256)` events.
//!     v2-style contracts emit `Sync` after every reserve-changing call, so
//!     `Swap` / `Mint` / `Burn` are treated as observability signals and do
//!     not mutate the local snapshot. The next `Sync` carries the canonical
//!     post-event values.
//!
//! Event encoding:
//!   * Each Cairo `u256` is serialized as two felts: `low: u128`, `high: u128`.
//!   * Therefore a `Sync` event payload is exactly four felts.

use std::fmt;

use async_trait::async_trait;
use domain::{
    Amount, EventMeta, Pool, PoolEvent, PoolEventKind, PoolMeta, Quote, QuoteError, StateError,
    TokenId, U256,
};
use math::{cpmm_quote_in, cpmm_quote_out, MathError};
use starknet_types_core::felt::Felt;

use crate::quoter::SharedQuoter;

/// Adapter for a single `JediSwap` v1 pool.
pub struct JediSwapV1Pool {
    meta: PoolMeta,
    /// Pool fee in ppm against the `1_000_000` denominator
    /// (`JediSwap` v1 is a fixed 30 bps = `3_000` ppm pool).
    fee_ppm: u32,
    state: Option<State>,
    state_version: u64,
    quoter: SharedQuoter,
}

#[derive(Debug, Clone, Copy)]
struct State {
    reserve0: U256,
    reserve1: U256,
    /// The `(block, tx_index, event_index)` of the last applied Sync.
    /// Used for ordering checks and deduplication on reconnect.
    last_sync: EventMeta,
}

impl JediSwapV1Pool {
    /// Build an unloaded pool. The first applied `Sync` event seeds reserves.
    pub fn new(meta: PoolMeta, quoter: SharedQuoter) -> Self {
        let fee_ppm = meta.id.fee.ppm();
        Self {
            meta,
            fee_ppm,
            state: None,
            state_version: 0,
            quoter,
        }
    }

    /// Returns `(reserve_in, reserve_out)` ordered by the caller's `token_in`.
    fn reserves_for(&self, token_in: TokenId) -> Result<(U256, U256), QuoteError> {
        let state = self.state.as_ref().ok_or(QuoteError::StateNotLoaded)?;
        if token_in == self.meta.token0 {
            Ok((state.reserve0, state.reserve1))
        } else if token_in == self.meta.token1 {
            Ok((state.reserve1, state.reserve0))
        } else {
            Err(QuoteError::TokenNotInPool(token_in))
        }
    }
}

impl fmt::Debug for JediSwapV1Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JediSwapV1Pool")
            .field("id", &self.meta.id)
            .field("state_version", &self.state_version)
            .field("state_loaded", &self.state.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Pool for JediSwapV1Pool {
    fn meta(&self) -> &PoolMeta {
        &self.meta
    }

    fn state_version(&self) -> u64 {
        self.state_version
    }

    fn quote_in_local(&self, amount_in: Amount) -> Result<Quote, QuoteError> {
        if amount_in.is_zero() {
            return Err(QuoteError::ZeroInput);
        }
        let token_out = self
            .meta
            .other_token(amount_in.token)
            .ok_or(QuoteError::TokenNotInPool(amount_in.token))?;
        let (reserve_in, reserve_out) = self.reserves_for(amount_in.token)?;
        let amount_out_raw = cpmm_quote_out(reserve_in, reserve_out, amount_in.raw, self.fee_ppm)
            .map_err(map_math_err)?;
        Ok(Quote {
            pool: self.meta.id,
            amount_in,
            amount_out: Amount::new(token_out, amount_out_raw),
            gas_estimate: 0,
            state_version: self.state_version,
        })
    }

    fn quote_out_local(&self, amount_out: Amount) -> Result<Quote, QuoteError> {
        if amount_out.is_zero() {
            return Err(QuoteError::ZeroInput);
        }
        let token_in = self
            .meta
            .other_token(amount_out.token)
            .ok_or(QuoteError::TokenNotInPool(amount_out.token))?;
        let (reserve_in, reserve_out) = self.reserves_for(token_in)?;
        let amount_in_raw = cpmm_quote_in(reserve_in, reserve_out, amount_out.raw, self.fee_ppm)
            .map_err(map_math_err)?;
        Ok(Quote {
            pool: self.meta.id,
            amount_in: Amount::new(token_in, amount_in_raw),
            amount_out,
            gas_estimate: 0,
            state_version: self.state_version,
        })
    }

    async fn quote_in_onchain(&self, amount_in: Amount) -> Result<Quote, QuoteError> {
        if amount_in.is_zero() {
            return Err(QuoteError::ZeroInput);
        }
        let token_out = self
            .meta
            .other_token(amount_in.token)
            .ok_or(QuoteError::TokenNotInPool(amount_in.token))?;
        let amount_out_raw = self
            .quoter
            .quote_in(self.meta.id, amount_in.token, amount_in.raw)
            .await?;
        Ok(Quote {
            pool: self.meta.id,
            amount_in,
            amount_out: Amount::new(token_out, amount_out_raw),
            gas_estimate: 0,
            state_version: self.state_version,
        })
    }

    fn apply_event(&mut self, event: &PoolEvent) -> Result<(), StateError> {
        if event.pool != self.meta.id {
            return Err(StateError::WrongPool);
        }

        match event.kind {
            PoolEventKind::Sync => {
                if let Some(state) = &self.state {
                    let cur = state.last_sync.ordering_key();
                    let new = event.meta.ordering_key();
                    match new.cmp(&cur) {
                        std::cmp::Ordering::Less => {
                            return Err(StateError::OutOfOrder {
                                got: event.meta.block,
                                current: state.last_sync.block,
                            });
                        }
                        std::cmp::Ordering::Equal => {
                            return Err(StateError::Duplicate(format!(
                                "{}/{}/{}",
                                event.meta.block, event.meta.tx_index, event.meta.event_index
                            )));
                        }
                        std::cmp::Ordering::Greater => {}
                    }
                }
                let (r0, r1) = decode_sync(&event.data)?;
                self.state = Some(State {
                    reserve0: r0,
                    reserve1: r1,
                    last_sync: event.meta,
                });
                self.state_version = self.state_version.saturating_add(1);
                Ok(())
            }
            PoolEventKind::Swap | PoolEventKind::Mint | PoolEventKind::Burn => {
                // v2-fork semantics: any reserve change is followed by Sync;
                // we don't mutate local state from these events.
                Ok(())
            }
        }
    }
}

fn map_math_err(e: MathError) -> QuoteError {
    match e {
        // ZeroReserves can be hit on a loaded-but-empty pool (freshly deployed,
        // or fully drained). State IS loaded — surface this as InsufficientLiquidity
        // rather than misreporting as StateNotLoaded.
        MathError::ZeroReserves | MathError::InsufficientLiquidity => {
            QuoteError::InsufficientLiquidity
        }
        MathError::ZeroInput => QuoteError::ZeroInput,
        MathError::InvalidFee | MathError::Overflow => QuoteError::MathOverflow,
    }
}

/// Decode a `JediSwap` v1 `Sync(reserve0: u256, reserve1: u256)` event payload.
fn decode_sync(data: &[Felt]) -> Result<(U256, U256), StateError> {
    if data.len() != 4 {
        return Err(StateError::Malformed(format!(
            "expected 4 felts for Sync, got {}",
            data.len()
        )));
    }
    let r0 = felt_pair_to_u256(data[0], data[1])?;
    let r1 = felt_pair_to_u256(data[2], data[3])?;
    Ok((r0, r1))
}

/// Combine two felts (low: u128, high: u128) into a `U256`.
fn felt_pair_to_u256(low: Felt, high: Felt) -> Result<U256, StateError> {
    let low_u128 = u128::try_from(low)
        .map_err(|_| StateError::Malformed("u256 low half exceeds u128".into()))?;
    let high_u128 = u128::try_from(high)
        .map_err(|_| StateError::Malformed("u256 high half exceeds u128".into()))?;
    Ok((U256::from(high_u128) << 128) | U256::from(low_u128))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

    use super::*;
    use crate::quoter::OnChainQuoter;
    use domain::{ContractAddress, DexKind, FeeBps, PoolId};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // ---------- helpers ----------

    fn tok(n: u128) -> TokenId {
        TokenId::new(ContractAddress::new(Felt::from(n)))
    }

    fn pid(addr: u128) -> PoolId {
        PoolId {
            address: ContractAddress::new(Felt::from(addr)),
            dex: DexKind::JediSwapV1,
            fee: FeeBps::new(30), // 30 bps = 0.30% = 3000 ppm
        }
    }

    fn meta(token_a: TokenId, token_b: TokenId) -> PoolMeta {
        PoolMeta {
            id: pid(0xabcd),
            token0: token_a,
            token1: token_b,
        }
    }

    fn u256_to_felt_pair(n: U256) -> (Felt, Felt) {
        let low = n.low_u128();
        let high = (n >> 128).low_u128();
        (Felt::from(low), Felt::from(high))
    }

    fn sync_event(
        p: PoolId,
        block: u64,
        tx_index: u32,
        event_index: u32,
        r0: U256,
        r1: U256,
    ) -> PoolEvent {
        let (l0, h0) = u256_to_felt_pair(r0);
        let (l1, h1) = u256_to_felt_pair(r1);
        PoolEvent {
            pool: p,
            meta: EventMeta {
                block,
                tx_index,
                event_index,
            },
            kind: PoolEventKind::Sync,
            data: vec![l0, h0, l1, h1],
        }
    }

    struct MockQuoter {
        responses: Mutex<Vec<U256>>,
    }

    impl MockQuoter {
        fn new(mut responses: Vec<U256>) -> Arc<Self> {
            responses.reverse(); // pop() returns from the end → FIFO
            Arc::new(Self {
                responses: Mutex::new(responses),
            })
        }
    }

    #[async_trait]
    impl OnChainQuoter for MockQuoter {
        async fn quote_in(
            &self,
            _pool: PoolId,
            _token_in: TokenId,
            _amount_in: U256,
        ) -> Result<U256, QuoteError> {
            self.responses
                .lock()
                .await
                .pop()
                .ok_or_else(|| QuoteError::OnChain("MockQuoter exhausted".into()))
        }
    }

    // ---------- tests ----------

    #[test]
    fn quote_before_load_fails() {
        let usdc = tok(1);
        let eth = tok(2);
        let pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        let res = pool.quote_in_local(Amount::new(usdc, U256::from(100u64)));
        assert!(matches!(res, Err(QuoteError::StateNotLoaded)));
    }

    #[test]
    fn quote_with_token_not_in_pool_fails() {
        let usdc = tok(1);
        let eth = tok(2);
        let strk = tok(3);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        pool.apply_event(&sync_event(
            pool.meta.id,
            1,
            0,
            0,
            U256::from(1000u64),
            U256::from(1000u64),
        ))
        .unwrap();

        let res = pool.quote_in_local(Amount::new(strk, U256::from(100u64)));
        assert!(matches!(res, Err(QuoteError::TokenNotInPool(_))));
    }

    #[test]
    fn quote_zero_input_fails() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        pool.apply_event(&sync_event(
            pool.meta.id,
            1,
            0,
            0,
            U256::from(1000u64),
            U256::from(1000u64),
        ))
        .unwrap();
        let res = pool.quote_in_local(Amount::new(usdc, U256::zero()));
        assert!(matches!(res, Err(QuoteError::ZeroInput)));
    }

    #[test]
    fn canonical_quote_matches_uniswap_v2_formula() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        pool.apply_event(&sync_event(
            pool.meta.id,
            1,
            0,
            0,
            U256::from(1000u64),
            U256::from(1000u64),
        ))
        .unwrap();

        // 100 in @ (1000, 1000), fee 30 bps → 90 out (see math tests)
        let q = pool
            .quote_in_local(Amount::new(usdc, U256::from(100u64)))
            .unwrap();
        assert_eq!(q.amount_out.token, eth);
        assert_eq!(q.amount_out.raw, U256::from(90u64));
        assert_eq!(q.amount_in.raw, U256::from(100u64));
        assert_eq!(q.state_version, 1);
    }

    #[test]
    fn quote_in_then_quote_out_consistency() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        pool.apply_event(&sync_event(
            pool.meta.id,
            1,
            0,
            0,
            U256::from(1000u64),
            U256::from(1000u64),
        ))
        .unwrap();

        let q_in = pool
            .quote_in_local(Amount::new(usdc, U256::from(100u64)))
            .unwrap();
        let q_out = pool.quote_out_local(q_in.amount_out).unwrap();
        assert_eq!(q_out.amount_in.token, usdc);
        // ceil rounding may require <= original
        assert!(q_out.amount_in.raw <= U256::from(100u64));
    }

    #[test]
    fn sync_event_updates_reserves() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        pool.apply_event(&sync_event(
            pool.meta.id,
            1,
            0,
            0,
            U256::from(1000u64),
            U256::from(1000u64),
        ))
        .unwrap();
        assert_eq!(pool.state_version(), 1);

        pool.apply_event(&sync_event(
            pool.meta.id,
            2,
            0,
            0,
            U256::from(2000u64),
            U256::from(500u64),
        ))
        .unwrap();
        assert_eq!(pool.state_version(), 2);

        // After the new sync, quotes reflect new reserves.
        let q = pool
            .quote_in_local(Amount::new(usdc, U256::from(100u64)))
            .unwrap();
        assert_ne!(q.amount_out.raw, U256::from(90u64));
    }

    #[test]
    fn duplicate_event_rejected() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        let ev = sync_event(
            pool.meta.id,
            1,
            0,
            0,
            U256::from(1000u64),
            U256::from(1000u64),
        );
        pool.apply_event(&ev).unwrap();
        let res = pool.apply_event(&ev);
        assert!(matches!(res, Err(StateError::Duplicate(_))));
        assert_eq!(pool.state_version(), 1);
    }

    #[test]
    fn out_of_order_event_rejected() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        pool.apply_event(&sync_event(
            pool.meta.id,
            5,
            0,
            0,
            U256::from(1000u64),
            U256::from(1000u64),
        ))
        .unwrap();
        let res = pool.apply_event(&sync_event(
            pool.meta.id,
            3,
            0,
            0,
            U256::from(2000u64),
            U256::from(500u64),
        ));
        assert!(matches!(res, Err(StateError::OutOfOrder { .. })));
    }

    #[test]
    fn wrong_pool_rejected() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        let wrong_pid = PoolId {
            address: ContractAddress::new(Felt::from(0xdeadu64)),
            dex: DexKind::JediSwapV1,
            fee: FeeBps::new(30),
        };
        let ev = sync_event(wrong_pid, 1, 0, 0, U256::from(1000u64), U256::from(1000u64));
        assert!(matches!(pool.apply_event(&ev), Err(StateError::WrongPool)));
    }

    #[test]
    fn malformed_sync_event_rejected() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        let bad = PoolEvent {
            pool: pool.meta.id,
            meta: EventMeta {
                block: 1,
                tx_index: 0,
                event_index: 0,
            },
            kind: PoolEventKind::Sync,
            data: vec![Felt::from(1u64), Felt::from(2u64)], // 2 felts, need 4
        };
        assert!(matches!(
            pool.apply_event(&bad),
            Err(StateError::Malformed(_))
        ));
    }

    #[test]
    fn non_sync_events_dont_change_reserves() {
        let usdc = tok(1);
        let eth = tok(2);
        let mut pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![]));
        pool.apply_event(&sync_event(
            pool.meta.id,
            1,
            0,
            0,
            U256::from(1000u64),
            U256::from(1000u64),
        ))
        .unwrap();
        let version_before = pool.state_version();

        let swap_ev = PoolEvent {
            pool: pool.meta.id,
            meta: EventMeta {
                block: 2,
                tx_index: 0,
                event_index: 0,
            },
            kind: PoolEventKind::Swap,
            data: vec![],
        };
        pool.apply_event(&swap_ev).unwrap();
        assert_eq!(pool.state_version(), version_before);
        let q = pool
            .quote_in_local(Amount::new(usdc, U256::from(100u64)))
            .unwrap();
        assert_eq!(q.amount_out.raw, U256::from(90u64));
    }

    #[test]
    fn felt_pair_decoding_handles_large_u256() {
        // build n = (high << 128) | low where both halves use most of u128
        let low_u: u128 = (1u128 << 127) - 1;
        let high_u: u128 = (1u128 << 100) | 7;
        let n = (U256::from(high_u) << 128) | U256::from(low_u);
        let (l, h) = u256_to_felt_pair(n);
        let back = felt_pair_to_u256(l, h).unwrap();
        assert_eq!(back, n);
    }

    #[tokio::test]
    async fn quote_in_onchain_delegates_to_quoter() {
        let usdc = tok(1);
        let eth = tok(2);
        let pool = JediSwapV1Pool::new(meta(usdc, eth), MockQuoter::new(vec![U256::from(91u64)]));
        let q = pool
            .quote_in_onchain(Amount::new(usdc, U256::from(100u64)))
            .await
            .unwrap();
        assert_eq!(q.amount_out.token, eth);
        assert_eq!(q.amount_out.raw, U256::from(91u64));
    }
}
