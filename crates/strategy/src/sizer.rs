//! Ternary search over `amount_in` to find the optimal trade size.
//!
//! `NetProfit(amount_in)` for an arbitrage cycle is unimodal (one peak): too
//! small fails to cover gas, too large is eaten by price impact. Ternary
//! search converges fast (≈30 iterations is overkill for any realistic range).
//!
//! The signal is [`SignedAmount`] net profit — so the search converges even
//! through negative-profit territory, which matters when the lower endpoint
//! is unprofitable and the peak is to the right.

use domain::{Amount, Path, Pool, SignedAmount, U256};
use thiserror::Error;

use crate::profit::{evaluate_path, EvalError, PathOutcome, ProfitParams};

#[derive(Debug)]
pub struct SizedTrade {
    pub amount_in: Amount,
    pub outcome: PathOutcome,
    pub iterations: u32,
}

#[derive(Debug, Error)]
pub enum SizerError {
    #[error("min_in.token != max_in.token")]
    RangeTokenMismatch,

    #[error("min_in.raw ({min}) >= max_in.raw ({max})")]
    RangeEmpty { min: U256, max: U256 },

    #[error("ternary search converged but the result is not profitable")]
    NoProfitableSize,

    #[error("path evaluation failed: {0}")]
    Eval(#[from] EvalError),
}

/// Ternary-search `[min_in, max_in]` for the `amount_in` that maximizes net
/// profit on `path`. Returns the best [`PathOutcome`] if profitable, else
/// [`SizerError::NoProfitableSize`].
///
/// `iterations` caps the search depth — 30 is far more than required for any
/// reasonable range. The search exits early once `high - low < 3` (smaller
/// ranges can't be subdivided with integer math).
pub fn find_optimal_input(
    path: &Path,
    pools: &[&dyn Pool],
    params: &ProfitParams,
    min_in: Amount,
    max_in: Amount,
    iterations: u32,
) -> Result<SizedTrade, SizerError> {
    if min_in.token != max_in.token {
        return Err(SizerError::RangeTokenMismatch);
    }
    if min_in.raw >= max_in.raw {
        return Err(SizerError::RangeEmpty {
            min: min_in.raw,
            max: max_in.raw,
        });
    }

    let token = min_in.token;
    let mut lo = min_in.raw;
    let mut hi = max_in.raw;
    let three = U256::from(3u64);
    let mut iters_done = 0u32;

    // Track best (amount_in, outcome) seen across all evaluations. The final
    // window's midpoint is only one candidate — picking the loop-best preserves
    // information that the integer-discretized window would otherwise discard.
    let mut best: Option<(Amount, PathOutcome)> = None;

    for _ in 0..iterations {
        let span = hi - lo;
        if span < three {
            break;
        }
        let third = span / three;
        let m1_raw = lo + third;
        let m2_raw = hi - third;

        let m1 = Amount::new(token, m1_raw);
        let m2 = Amount::new(token, m2_raw);

        let o1 = evaluate_path(path, m1, pools, params)?;
        let o2 = evaluate_path(path, m2, pools, params)?;

        // Cache the nets before moving outcomes into `consider`.
        let o1_net = o1.net;
        let o2_net = o2.net;

        consider(&mut best, m1, o1);
        consider(&mut best, m2, o2);

        match signed_cmp(o1_net, o2_net) {
            std::cmp::Ordering::Less => {
                // f(m1) < f(m2): peak is to the right of m1.
                lo = m1_raw;
            }
            std::cmp::Ordering::Greater | std::cmp::Ordering::Equal => {
                // f(m1) >= f(m2): peak is to the left of m2.
                hi = m2_raw;
            }
        }
        iters_done += 1;
    }

    // Final midpoint as one more candidate — covers the case where iterations=0
    // or the loop never ran (span<3 initially).
    let mid_raw = lo + (hi - lo) / U256::from(2u64);
    let mid = Amount::new(token, mid_raw);
    let mid_outcome = evaluate_path(path, mid, pools, params)?;
    consider(&mut best, mid, mid_outcome);

    let (best_amount, best_outcome) = best.ok_or(SizerError::NoProfitableSize)?;
    if !best_outcome.is_profitable() {
        return Err(SizerError::NoProfitableSize);
    }
    Ok(SizedTrade {
        amount_in: best_amount,
        outcome: best_outcome,
        iterations: iters_done,
    })
}

/// Update `best` if `outcome` has a strictly greater net than the current best.
fn consider(best: &mut Option<(Amount, PathOutcome)>, amount: Amount, outcome: PathOutcome) {
    let take = match best {
        None => true,
        Some((_, b)) => signed_cmp(outcome.net, b.net) == std::cmp::Ordering::Greater,
    };
    if take {
        *best = Some((amount, outcome));
    }
}

/// Compare two signed nets. Tokens are guaranteed to match by `evaluate_path`'s
/// `StartTokenMismatch` / `GasTokenMismatch` guards: every `PathOutcome`'s `net`
/// is in `path.start_token`, which equals `min_in.token` by the upfront range
/// check. A `TokenMismatch` here would indicate a bug in `evaluate_path` or a
/// new entry point that skips the guards — surface it loudly rather than
/// silently corrupting the search.
#[allow(clippy::expect_used)]
fn signed_cmp(a: SignedAmount, b: SignedAmount) -> std::cmp::Ordering {
    a.cmp_same_token(b)
        .expect("evaluate_path invariant: nets share path.start_token")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::profit::ProfitParams;
    use domain::{
        Amount, ContractAddress, DexKind, FeeBps, PoolEvent, PoolId, PoolMeta, Quote, QuoteError,
        StateError, TokenId,
    };
    use math::cpmm_quote_out;
    use starknet_types_core::felt::Felt;

    // Same MockCpmmPool as in profit::tests, copied to keep the modules independent.
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
    fn cycle_path(usdc: TokenId, eth: TokenId, p_a: PoolId, p_b: PoolId) -> Path {
        Path::new(vec![
            domain::Hop {
                pool: p_a,
                token_in: usdc,
                token_out: eth,
            },
            domain::Hop {
                pool: p_b,
                token_in: eth,
                token_out: usdc,
            },
        ])
        .unwrap()
    }

    #[test]
    fn finds_peak_for_two_dex_arbitrage() {
        // Same setup as profit::tests::profitable_two_dex_arbitrage:
        // Pool A: 1M USDC, 1k ETH (cheap ETH).
        // Pool B: 1M USDC, 500 ETH (expensive ETH).
        let usdc = tok(1);
        let eth = tok(2);
        let p_a = pid(0x0A);
        let p_b = pid(0x0B);
        let pool_a = MockCpmmPool::new(
            p_a,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            p_b,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(500u64),
            0,
        );
        let path = cycle_path(usdc, eth, p_a, p_b);
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(100u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];

        let result = find_optimal_input(
            &path,
            &pools,
            &params,
            // amount_in below ~1112 USDC quotes to 0 ETH on hop A — outside the
            // useful range. Start at 2_000 so every probe yields a valid quote.
            Amount::new(usdc, U256::from(2_000u64)),
            Amount::new(usdc, U256::from(900_000u64)),
            40,
        )
        .unwrap();

        // 1) The peak must be profitable.
        assert!(
            result.outcome.is_profitable(),
            "outcome={:?}",
            result.outcome
        );

        // 2) The peak's net must dominate several reference points across the range.
        let peak_net = result.outcome.net;
        for sample_raw in [2_000u64, 10_000, 50_000, 100_000, 500_000, 900_000] {
            let sample = Amount::new(usdc, U256::from(sample_raw));
            let s = evaluate_path(&path, sample, &pools, &params).unwrap();
            assert!(
                peak_net.cmp_same_token(s.net).unwrap() != std::cmp::Ordering::Less,
                "peak {peak_net:?} should be >= net at {sample_raw} ({:?})",
                s.net
            );
        }

        // 3) The peak is not pinned to a boundary.
        assert!(result.amount_in.raw > U256::from(2_000u64));
        assert!(result.amount_in.raw < U256::from(900_000u64));
    }

    #[test]
    fn no_profitable_size_when_pools_are_balanced() {
        // Identical pools → no arbitrage; gas + safety make every input unprofitable.
        let usdc = tok(1);
        let eth = tok(2);
        let p_a = pid(0x0A);
        let p_b = pid(0x0B);
        let pool_a = MockCpmmPool::new(
            p_a,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            p_b,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let path = cycle_path(usdc, eth, p_a, p_b);
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(100u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];

        let result = find_optimal_input(
            &path,
            &pools,
            &params,
            Amount::new(usdc, U256::from(1u64)),
            Amount::new(usdc, U256::from(1_000_000u64)),
            40,
        );
        assert!(matches!(result, Err(SizerError::NoProfitableSize)));
    }

    #[test]
    fn rejects_empty_range() {
        let usdc = tok(1);
        let eth = tok(2);
        let p_a = pid(0x0A);
        let p_b = pid(0x0B);
        let pool_a = MockCpmmPool::new(
            p_a,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            p_b,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(500u64),
            0,
        );
        let path = cycle_path(usdc, eth, p_a, p_b);
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(100u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let r = find_optimal_input(
            &path,
            &pools,
            &params,
            Amount::new(usdc, U256::from(100u64)),
            Amount::new(usdc, U256::from(100u64)),
            40,
        );
        assert!(matches!(r, Err(SizerError::RangeEmpty { .. })));
    }

    #[test]
    fn rejects_range_token_mismatch() {
        let usdc = tok(1);
        let eth = tok(2);
        let p_a = pid(0x0A);
        let p_b = pid(0x0B);
        let pool_a = MockCpmmPool::new(
            p_a,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let pool_b = MockCpmmPool::new(
            p_b,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(500u64),
            0,
        );
        let path = cycle_path(usdc, eth, p_a, p_b);
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(100u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let r = find_optimal_input(
            &path,
            &pools,
            &params,
            Amount::new(usdc, U256::from(1u64)),
            Amount::new(eth, U256::from(1_000u64)),
            40,
        );
        assert!(matches!(r, Err(SizerError::RangeTokenMismatch)));
    }
}
