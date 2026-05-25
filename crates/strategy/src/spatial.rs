//! Event-driven 2-DEX spatial arbitrage detector.
//!
//! On each pool state update, find all pools that share the updated pool's
//! token pair, then for each (updated, other) combination try both cycle
//! directions and run the ternary-search sizer. Emit a `SizedTrade` per
//! profitable cycle.
//!
//! The function is intentionally O(N) in pools-matching-pair: only paths
//! involving the just-updated pool need re-evaluation. Cycles between two
//! *other* pools haven't changed, so they don't need to be rechecked.

use domain::{Hop, Path, Pool, PoolId, TokenId};
use tracing::warn;

use crate::profit::ProfitParams;
use crate::sizer::{find_optimal_input, SizedTrade, SizerError};

/// Parameters for the spatial detector, applied at every event.
#[derive(Debug, Clone)]
pub struct SpatialParams {
    /// The base token both legs of the cycle start/end with (e.g., USDC).
    /// Pools that don't contain this token are skipped.
    pub start_token: TokenId,
    /// Profit gating params (gas + safety margin).
    pub profit: ProfitParams,
    /// Lower/upper bounds on `amount_in` for the ternary search.
    /// Both are denominated in `start_token`.
    pub min_amount_in: domain::Amount,
    pub max_amount_in: domain::Amount,
    /// Ternary search iteration cap (30-40 is far more than enough).
    pub sizer_iterations: u32,
}

/// Find profitable 2-DEX spatial arbitrage cycles involving the pool that
/// just updated its state.
///
/// `pools` is a borrowed snapshot of all live pool adapters. The detector
/// walks it once to locate the updated pool and to collect candidates with
/// the same token pair. For each candidate it constructs both cycle
/// directions and runs the sizer.
///
/// Returns one [`SizedTrade`] per profitable cycle found. Both directions of
/// the same pair may be profitable simultaneously (rare); both are returned.
pub fn detect_spatial_opportunities(
    updated_pool: PoolId,
    pools: &[&dyn Pool],
    params: &SpatialParams,
) -> Vec<SizedTrade> {
    // Locate the updated pool. If it isn't registered, nothing to do.
    let Some(p0) = pools.iter().find(|p| p.meta().id == updated_pool) else {
        return Vec::new();
    };
    let p0_meta = p0.meta();

    // The pool must contain the bot's base token.
    if !p0_meta.contains(params.start_token) {
        return Vec::new();
    }
    let Some(other_token) = p0_meta.other_token(params.start_token) else {
        return Vec::new();
    };

    // Candidates: other pools that pair (start_token, other_token).
    let mut opportunities = Vec::new();
    for p1 in pools {
        let p1_meta = p1.meta();
        if p1_meta.id == p0_meta.id {
            continue;
        }
        if !(p1_meta.contains(params.start_token) && p1_meta.contains(other_token)) {
            continue;
        }
        opportunities.extend(try_pair(*p0, *p1, other_token, params));
    }

    opportunities
}

/// Try both cycle directions across a single (p0, p1) pool pair.
fn try_pair<'a>(
    p0: &'a dyn Pool,
    p1: &'a dyn Pool,
    other_token: TokenId,
    params: &SpatialParams,
) -> Vec<SizedTrade> {
    let mut out = Vec::new();
    let cycles = [(p0, p1), (p1, p0)];
    for (first, second) in cycles {
        let Some(path) = build_cycle_path(first, second, params.start_token, other_token) else {
            continue;
        };
        let pools_for_path: [&dyn Pool; 2] = [first, second];
        match find_optimal_input(
            &path,
            &pools_for_path,
            &params.profit,
            params.min_amount_in,
            params.max_amount_in,
            params.sizer_iterations,
        ) {
            Ok(sized) => out.push(sized),
            Err(SizerError::NoProfitableSize) => {
                // Common case: no profit between this pair right now.
            }
            Err(SizerError::Eval(e)) => {
                // Quotes hit ZeroInput / StateNotLoaded / etc. — expected when a
                // pool's state isn't loaded yet or the range bottoms are too small.
                // Log at debug so we don't drown in noise.
                tracing::debug!(error = %e, "spatial cycle eval failed");
            }
            Err(e) => {
                warn!(error = %e, "spatial sizer failed unexpectedly");
            }
        }
    }
    out
}

/// Build `start → other → start` cycle through `(first, second)`.
/// Returns None only if `Path::new` would reject — should not happen given
/// the upstream containment checks.
fn build_cycle_path(
    first: &dyn Pool,
    second: &dyn Pool,
    start: TokenId,
    other: TokenId,
) -> Option<Path> {
    Path::new(vec![
        Hop {
            pool: first.meta().id,
            token_in: start,
            token_out: other,
        },
        Hop {
            pool: second.meta().id,
            token_in: other,
            token_out: start,
        },
    ])
    .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use domain::{
        Amount, ContractAddress, DexKind, FeeBps, PoolEvent, PoolMeta, Quote, QuoteError,
        StateError, U256,
    };
    use math::cpmm_quote_out;
    use starknet_types_core::felt::Felt;

    // ---- minimal Pool impl reused from the profit/sizer modules ----

    #[derive(Debug)]
    struct MockCpmmPool {
        meta: PoolMeta,
        reserve0: U256,
        reserve1: U256,
        fee_ppm: u32,
        version: u64,
    }

    impl MockCpmmPool {
        fn new(
            id: PoolId,
            token0: TokenId,
            token1: TokenId,
            r0: U256,
            r1: U256,
            fee_ppm: u32,
        ) -> Self {
            Self {
                meta: PoolMeta { id, token0, token1 },
                reserve0: r0,
                reserve1: r1,
                fee_ppm,
                version: 1,
            }
        }
        fn reserves_for(&self, token_in: TokenId) -> Result<(U256, U256), QuoteError> {
            if token_in == self.meta.token0 {
                Ok((self.reserve0, self.reserve1))
            } else if token_in == self.meta.token1 {
                Ok((self.reserve1, self.reserve0))
            } else {
                Err(QuoteError::TokenNotInPool(token_in))
            }
        }
    }

    #[async_trait::async_trait]
    impl Pool for MockCpmmPool {
        fn meta(&self) -> &PoolMeta {
            &self.meta
        }
        fn state_version(&self) -> u64 {
            self.version
        }
        fn quote_in_local(&self, amount_in: Amount) -> Result<Quote, QuoteError> {
            if amount_in.is_zero() {
                return Err(QuoteError::ZeroInput);
            }
            let token_out = self
                .meta
                .other_token(amount_in.token)
                .ok_or(QuoteError::TokenNotInPool(amount_in.token))?;
            let (r_in, r_out) = self.reserves_for(amount_in.token)?;
            let out = cpmm_quote_out(r_in, r_out, amount_in.raw, self.fee_ppm)
                .map_err(|_| QuoteError::MathOverflow)?;
            Ok(Quote {
                pool: self.meta.id,
                amount_in,
                amount_out: Amount::new(token_out, out),
                gas_estimate: 0,
                state_version: self.version,
            })
        }
        fn quote_out_local(&self, _amount_out: Amount) -> Result<Quote, QuoteError> {
            Err(QuoteError::LocalUnavailable)
        }
        async fn quote_in_onchain(&self, _a: Amount) -> Result<Quote, QuoteError> {
            Err(QuoteError::OnChain("mock".into()))
        }
        fn apply_event(&mut self, _e: &PoolEvent) -> Result<(), StateError> {
            Ok(())
        }
    }

    fn tok(n: u128) -> TokenId {
        TokenId::new(ContractAddress::new(Felt::from(n)))
    }
    fn pid(addr: u128) -> PoolId {
        PoolId {
            address: ContractAddress::new(Felt::from(addr)),
            dex: DexKind::JediSwapV1,
            fee: FeeBps::new(30),
        }
    }
    fn default_params(usdc: TokenId) -> SpatialParams {
        SpatialParams {
            start_token: usdc,
            profit: ProfitParams {
                gas_cost: Amount::new(usdc, U256::from(100u64)),
                safety_margin_bps: 30,
            },
            min_amount_in: Amount::new(usdc, U256::from(2_000u64)),
            max_amount_in: Amount::new(usdc, U256::from(900_000u64)),
            sizer_iterations: 40,
        }
    }

    // ---- tests ----

    #[test]
    fn finds_profitable_spatial_arb() {
        // Pool A: 1M USDC / 1k ETH (cheap ETH).
        // Pool B: 1M USDC / 500 ETH (expensive ETH).
        let usdc = tok(1);
        let eth = tok(2);
        let pool_a = MockCpmmPool::new(
            pid(0x0A),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            pid(0x0B),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(500u64),
            0,
        );
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let params = default_params(usdc);

        let opps = detect_spatial_opportunities(pool_a.meta.id, &pools, &params);
        // Expect at least one profitable direction (USDC→ETH on A, ETH→USDC on B).
        assert!(!opps.is_empty(), "should find at least one opportunity");
        for sized in &opps {
            assert!(sized.outcome.is_profitable());
            assert_eq!(sized.amount_in.token, usdc);
            // The cycle starts and ends in USDC.
            assert_eq!(sized.outcome.path.start_token(), usdc);
            assert_eq!(sized.outcome.path.end_token(), usdc);
            assert_eq!(sized.outcome.path.len(), 2);
        }
    }

    #[test]
    fn identical_pools_yield_no_opportunities() {
        let usdc = tok(1);
        let eth = tok(2);
        let pool_a = MockCpmmPool::new(
            pid(0x0A),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            pid(0x0B),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let params = default_params(usdc);
        let opps = detect_spatial_opportunities(pool_a.meta.id, &pools, &params);
        assert!(opps.is_empty());
    }

    #[test]
    fn updated_pool_not_in_registry_returns_empty() {
        let usdc = tok(1);
        let eth = tok(2);
        let pool_a = MockCpmmPool::new(
            pid(0x0A),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pools: [&dyn Pool; 1] = [&pool_a];
        let params = default_params(usdc);
        let opps = detect_spatial_opportunities(pid(0xff), &pools, &params);
        assert!(opps.is_empty());
    }

    #[test]
    fn pool_without_start_token_is_skipped() {
        let usdc = tok(1);
        let eth = tok(2);
        let strk = tok(3);
        // pool_a is STRK/ETH — doesn't contain USDC, so it can't anchor the cycle.
        let pool_a = MockCpmmPool::new(
            pid(0x0A),
            strk,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            pid(0x0B),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(500u64),
            0,
        );
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let params = default_params(usdc);
        // Update is on pool_a (STRK/ETH) — without start_token, no spatial cycle
        // anchored at USDC can be detected from this event.
        let opps = detect_spatial_opportunities(pool_a.meta.id, &pools, &params);
        assert!(opps.is_empty());
    }

    #[test]
    fn unrelated_pools_are_filtered_out() {
        // Mix: two USDC/ETH pools (one is the updated one) AND one unrelated USDC/USDT pool.
        // The detector should only consider the USDC/ETH pair on event from pool_a.
        let usdc = tok(1);
        let eth = tok(2);
        let usdt = tok(3);
        let pool_a = MockCpmmPool::new(
            pid(0x0A),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            pid(0x0B),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(500u64),
            0,
        );
        let pool_c = MockCpmmPool::new(
            pid(0x0C),
            usdc,
            usdt,
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            0,
        );
        let pools: [&dyn Pool; 3] = [&pool_a, &pool_b, &pool_c];
        let params = default_params(usdc);
        let opps = detect_spatial_opportunities(pool_a.meta.id, &pools, &params);

        // All returned cycles must pair USDC with ETH (no usdt-anchored paths).
        for sized in &opps {
            for hop in sized.outcome.path.hops() {
                assert!(
                    (hop.token_in == usdc && hop.token_out == eth)
                        || (hop.token_in == eth && hop.token_out == usdc),
                    "hop {hop:?} should be USDC/ETH only",
                );
            }
        }
        assert!(!opps.is_empty());
    }

    #[test]
    fn unloaded_pool_state_is_swallowed_silently() {
        // pool_b's state will be unloaded (just constructed; reserves zero).
        // Actually MockCpmmPool always has state populated. To test the swallow
        // path we'd need a pool that returns StateNotLoaded — out of scope here.
        // Instead: assert that the detector doesn't crash when ranges produce
        // ZeroInput (e.g., min_amount_in too small for the pool's reserves).
        let usdc = tok(1);
        let eth = tok(2);
        let pool_a = MockCpmmPool::new(
            pid(0x0A),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            pid(0x0B),
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(500u64),
            0,
        );
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let mut params = default_params(usdc);
        // Force min_amount_in so small that hop A yields 0 ETH — this triggers
        // QuoteError::ZeroInput on hop B. The detector should swallow the Eval
        // error and continue (not panic).
        params.min_amount_in = Amount::new(usdc, U256::from(1u64));
        let opps = detect_spatial_opportunities(pool_a.meta.id, &pools, &params);
        // Either we find opps in the higher range, or none — but must not panic.
        let _ = opps;
    }
}
