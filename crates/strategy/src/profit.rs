//! Profit evaluation for a candidate arbitrage path.
//!
//! The formula (per project `critical_rules`):
//!
//! ```text
//! safety_margin = max(2 * gas_cost, amount_in * safety_margin_bps / 10_000)
//! net_profit    = amount_out - amount_in - gas_cost - safety_margin
//! ```
//!
//! Net is computed as a [`SignedAmount`] so unprofitable inputs are first-class
//! (the sizer's ternary search needs a numeric signal even in negative territory).

use domain::{
    Amount, AmountError, CoreError, Path, Pool, Quote, QuoteError, SignedAmount, TokenId, U256,
};
use thiserror::Error;

/// Knobs that govern profitability gating, applied to every evaluation.
#[derive(Debug, Clone, Copy)]
pub struct ProfitParams {
    /// Estimated total gas cost of executing the path, denominated in the
    /// path's start token. Sourced from a gas oracle upstream.
    pub gas_cost: Amount,
    /// Safety margin in basis points (1 bps = 0.01%), applied against
    /// `amount_in`. The actual margin used is `max(2 * gas_cost, bps * amount_in / 10_000)`.
    ///
    /// Note: the bps term floors to zero for `amount_in * bps < 10_000` (e.g.,
    /// `amount_in = 5, bps = 30 → 0`). Below that threshold, `2 * gas_cost`
    /// dominates, which is the intended behavior — gas is the binding floor
    /// for tiny trades.
    pub safety_margin_bps: u32,
}

/// Full result of evaluating a path at a specific `amount_in`. Always returned
/// when quoting succeeds — both profitable and unprofitable cases land here.
#[derive(Debug, Clone)]
pub struct PathOutcome {
    /// The path that produced this outcome. Carried alongside the quotes so
    /// downstream (simulator, executor) can reconstruct the trade without
    /// tracking it separately.
    pub path: Path,
    pub amount_in: Amount,
    pub amount_out: Amount,
    pub gas_cost: Amount,
    pub safety_margin: Amount,
    pub hop_quotes: Vec<Quote>,
    /// State versions of each pool snapshot used. Lets downstream detect
    /// staleness if the world changes between detection and submission.
    pub state_versions: Vec<u64>,
    /// `amount_out - amount_in`. Positive when the round-trip ended up.
    pub gross: SignedAmount,
    /// `gross - gas_cost - safety_margin`. The signal the sizer optimizes.
    pub net: SignedAmount,
}

impl PathOutcome {
    pub fn is_profitable(&self) -> bool {
        self.net.is_positive()
    }
}

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("path is not a closed cycle (start_token != end_token)")]
    PathNotCycle,

    #[error("pools.len() ({pools}) != path.hops().len() ({hops})")]
    PoolsHopMismatch { pools: usize, hops: usize },

    #[error("pool at index {index} has id {found:?}, hop expected {expected:?}")]
    PoolIdMismatch {
        index: usize,
        expected: domain::PoolId,
        found: domain::PoolId,
    },

    #[error("amount_in.token {amount_in:?} != path.start_token {start:?}")]
    StartTokenMismatch { amount_in: TokenId, start: TokenId },

    #[error("gas_cost.token {gas:?} != amount_in.token {amount_in:?}")]
    GasTokenMismatch { gas: TokenId, amount_in: TokenId },

    #[error("at hop {index}: quote returned amount_out token {got:?}, expected {expected:?}")]
    HopOutTokenMismatch {
        index: usize,
        expected: TokenId,
        got: TokenId,
    },

    #[error(transparent)]
    Quote(#[from] QuoteError),

    #[error(transparent)]
    Amount(#[from] AmountError),

    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Walk `path` starting from `amount_in`, quoting each hop via `quote_in_local`,
/// then compute the profit outcome.
///
/// `pools[i]` must correspond to `path.hops()[i]`. The function returns an
/// outcome whether or not the path is profitable; callers decide what to do.
pub fn evaluate_path(
    path: &Path,
    amount_in: Amount,
    pools: &[&dyn Pool],
    params: &ProfitParams,
) -> Result<PathOutcome, EvalError> {
    let hops = path.hops();
    if pools.len() != hops.len() {
        return Err(EvalError::PoolsHopMismatch {
            pools: pools.len(),
            hops: hops.len(),
        });
    }
    if !path.is_cycle() {
        return Err(EvalError::PathNotCycle);
    }
    if amount_in.token != path.start_token() {
        return Err(EvalError::StartTokenMismatch {
            amount_in: amount_in.token,
            start: path.start_token(),
        });
    }
    if params.gas_cost.token != amount_in.token {
        return Err(EvalError::GasTokenMismatch {
            gas: params.gas_cost.token,
            amount_in: amount_in.token,
        });
    }

    let mut hop_quotes = Vec::with_capacity(hops.len());
    let mut state_versions = Vec::with_capacity(hops.len());
    let mut current = amount_in;

    for (i, hop) in hops.iter().enumerate() {
        let pool = pools[i];
        let pool_id = pool.meta().id;
        if pool_id != hop.pool {
            return Err(EvalError::PoolIdMismatch {
                index: i,
                expected: hop.pool,
                found: pool_id,
            });
        }
        let quote = pool.quote_in_local(current)?;
        if quote.amount_out.token != hop.token_out {
            return Err(EvalError::HopOutTokenMismatch {
                index: i,
                expected: hop.token_out,
                got: quote.amount_out.token,
            });
        }
        state_versions.push(quote.state_version);
        current = quote.amount_out;
        hop_quotes.push(quote);
    }

    let amount_out = current;
    let gross = SignedAmount::from_diff(amount_out, amount_in)?;
    let safety_margin = compute_safety_margin(amount_in, params)?;
    let net = gross
        .checked_sub_amount(params.gas_cost)?
        .checked_sub_amount(safety_margin)?;

    Ok(PathOutcome {
        path: path.clone(),
        amount_in,
        amount_out,
        gas_cost: params.gas_cost,
        safety_margin,
        hop_quotes,
        state_versions,
        gross,
        net,
    })
}

/// `safety_margin = max(2 * gas_cost, amount_in * bps / 10_000)`.
fn compute_safety_margin(amount_in: Amount, params: &ProfitParams) -> Result<Amount, EvalError> {
    let gas_2x = params
        .gas_cost
        .raw
        .checked_mul(U256::from(2u64))
        .ok_or(AmountError::Overflow)?;
    let bps_raw = amount_in
        .raw
        .checked_mul(U256::from(u64::from(params.safety_margin_bps)))
        .ok_or(AmountError::Overflow)?
        / U256::from(10_000u64);
    let chosen = gas_2x.max(bps_raw);
    Ok(Amount::new(amount_in.token, chosen))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

    use super::*;
    use domain::{ContractAddress, DexKind, FeeBps, PoolEvent, PoolId, PoolMeta, StateError};
    use math::cpmm_quote_out;
    use starknet_types_core::felt::Felt;

    // ---- minimal Pool impl for testing (CPMM, no events, no state-loading flow) ----

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

    // ---- tests ----

    #[test]
    fn profitable_two_dex_arbitrage() {
        // Pool A is cheap on ETH (1000 ETH / 1M USDC → 1000 USDC/ETH).
        // Pool B is expensive on ETH (500 ETH / 1M USDC → 2000 USDC/ETH).
        // Buy ETH on A, sell on B. Use zero fee for crisp arithmetic.
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
        let amount_in = Amount::new(usdc, U256::from(10_000u64));
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(100u64)),
            safety_margin_bps: 30,
        };

        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let out = evaluate_path(&path, amount_in, &pools, &params).unwrap();

        // Hop A (USDC=1M, ETH=1k, fee=0, in=10_000 USDC):
        //   numerator = 10_000 * 1_000_000 * 1_000 = 1e13
        //   denominator = 1_000_000 * 1_000_000 + 10_000 * 1_000_000 = 1_010_000_000_000
        //   out = floor(1e13 / 1.01e12) = 9 ETH
        // Hop B (USDC=1M, ETH=500, fee=0, in=9 ETH):
        //   numerator = 9 * 1_000_000 * 1_000_000 = 9e12
        //   denominator = 500 * 1_000_000 + 9 * 1_000_000 = 509_000_000
        //   out = floor(9e12 / 509_000_000)
        //   = floor(9_000_000_000_000 / 509_000_000) = 17_681 USDC
        assert_eq!(out.amount_out.raw, U256::from(17_681u64));
        assert!(out.gross.is_positive());
        assert_eq!(out.gross.abs(), U256::from(7_681u64)); // 17_681 - 10_000

        // safety_margin = max(2*100, 10_000 * 30 / 10_000) = max(200, 30) = 200
        assert_eq!(out.safety_margin.raw, U256::from(200u64));

        // net = 7681 - 100 (gas) - 200 (safety) = 7381
        assert!(out.is_profitable());
        assert_eq!(out.net.abs(), U256::from(7_381u64));
        assert!(out.net.is_positive());
    }

    #[test]
    fn unprofitable_when_gross_below_costs() {
        // Pools nearly identical → tiny spread; pure backflow makes the round-trip
        // net-negative even before gas. amount_in must be large enough that
        // hop A yields non-zero ETH (price impact is the only reason there's a
        // round-trip loss at all in a CPMM with fee=0).
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
            U256::from(999u64),
            0,
        );
        let path = cycle_path(usdc, eth, p_a, p_b);
        let amount_in = Amount::new(usdc, U256::from(10_000u64));
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(50u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let out = evaluate_path(&path, amount_in, &pools, &params).unwrap();
        // Hop A: 10_000 USDC → 9 ETH (price impact).
        // Hop B: 9 ETH → 8_928 USDC (pool B has 1 ETH less than A — minimal spread
        // doesn't recover the price impact).
        // Gross = -1072. Net = -1072 - 50 - 100 = -1222.
        assert!(!out.is_profitable(), "{out:?}");
        assert!(out.net.is_negative());
        assert!(out.gross.is_negative());
    }

    #[test]
    fn safety_margin_uses_max_of_2x_gas_and_bps() {
        // Case 1: 2*gas dominates (high gas, small amount_in)
        let usdc = tok(1);
        let amount_in = Amount::new(usdc, U256::from(1_000u64));
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(500u64)),
            safety_margin_bps: 30, // 30 bps of 1000 = 3
        };
        let m = compute_safety_margin(amount_in, &params).unwrap();
        assert_eq!(m.raw, U256::from(1_000u64)); // 2 * 500

        // Case 2: bps dominates (low gas, large amount_in)
        let amount_in = Amount::new(usdc, U256::from(1_000_000u64));
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(10u64)),
            safety_margin_bps: 30, // 30 bps of 1_000_000 = 3_000
        };
        let m = compute_safety_margin(amount_in, &params).unwrap();
        assert_eq!(m.raw, U256::from(3_000u64)); // > 2 * 10
    }

    #[test]
    fn rejects_non_cycle_path() {
        let usdc = tok(1);
        let eth = tok(2);
        let p_a = pid(0x0A);
        let pool_a = MockCpmmPool::new(
            p_a,
            usdc,
            eth,
            U256::from(1_000_000u64),
            U256::from(1_000u64),
            0,
        );
        let path = Path::new(vec![domain::Hop {
            pool: p_a,
            token_in: usdc,
            token_out: eth,
        }])
        .unwrap();
        let amount_in = Amount::new(usdc, U256::from(100u64));
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(10u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 1] = [&pool_a];
        let r = evaluate_path(&path, amount_in, &pools, &params);
        assert!(matches!(r, Err(EvalError::PathNotCycle)));
    }

    #[test]
    fn rejects_pools_hop_mismatch() {
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
        let path = cycle_path(usdc, eth, p_a, p_b);
        let amount_in = Amount::new(usdc, U256::from(100u64));
        let params = ProfitParams {
            gas_cost: Amount::new(usdc, U256::from(10u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 1] = [&pool_a];
        let r = evaluate_path(&path, amount_in, &pools, &params);
        assert!(matches!(r, Err(EvalError::PoolsHopMismatch { .. })));
    }

    #[test]
    fn rejects_start_token_mismatch() {
        let usdc = tok(1);
        let eth = tok(2);
        let strk = tok(3);
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
        // Pass strk as amount_in — doesn't match the path's start token (usdc).
        let amount_in = Amount::new(strk, U256::from(100u64));
        let params = ProfitParams {
            gas_cost: Amount::new(strk, U256::from(10u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let r = evaluate_path(&path, amount_in, &pools, &params);
        assert!(matches!(r, Err(EvalError::StartTokenMismatch { .. })));
    }

    #[test]
    fn rejects_gas_token_mismatch() {
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
        let amount_in = Amount::new(usdc, U256::from(100u64));
        // gas in eth — different token from amount_in
        let params = ProfitParams {
            gas_cost: Amount::new(eth, U256::from(1u64)),
            safety_margin_bps: 30,
        };
        let pools: [&dyn Pool; 2] = [&pool_a, &pool_b];
        let r = evaluate_path(&path, amount_in, &pools, &params);
        assert!(matches!(r, Err(EvalError::GasTokenMismatch { .. })));
    }
}
