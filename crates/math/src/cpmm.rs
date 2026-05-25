//! Constant-product (Uniswap v2 family) market-maker math.
//!
//! Formula:
//!
//! ```text
//! amount_out = (Rout * Δin * (D - φ)) / (Rin * D + Δin * (D - φ))
//! ```
//!
//! where `D = FEE_DENOM = 1_000_000` and `φ = fee_ppm`.
//!
//! Internally promotes operands to `U512` and uses checked arithmetic; both
//! halves of the formula are bounded by `2^532` in the worst case, which
//! exceeds `U512`, so every multiplication and addition is `checked_*` and
//! returns `MathError::Overflow` on saturation. The final result is checked
//! to fit back into `U256`.

use primitive_types::{U256, U512};
use thiserror::Error;

/// Denominator for fee arithmetic. Fees are expressed in parts-per-million.
pub const FEE_DENOM: u32 = 1_000_000;

#[derive(Debug, Clone, Copy, Error, Eq, PartialEq)]
pub enum MathError {
    #[error("pool has zero reserve(s)")]
    ZeroReserves,

    #[error("input amount is zero")]
    ZeroInput,

    #[error("fee_ppm exceeds the 1_000_000 denominator")]
    InvalidFee,

    #[error("insufficient liquidity (requested output >= reserve_out)")]
    InsufficientLiquidity,

    #[error("intermediate computation overflowed U256")]
    Overflow,
}

/// "How much do I get out for this much in?". Returns floored `amount_out`.
///
/// Floor rounding favors the pool (matches Uniswap v2's `getAmountOut`).
pub fn cpmm_quote_out(
    reserve_in: U256,
    reserve_out: U256,
    amount_in: U256,
    fee_ppm: u32,
) -> Result<U256, MathError> {
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err(MathError::ZeroReserves);
    }
    if amount_in.is_zero() {
        return Err(MathError::ZeroInput);
    }
    if fee_ppm >= FEE_DENOM {
        return Err(MathError::InvalidFee);
    }

    let fee_complement = u64::from(FEE_DENOM - fee_ppm);
    let amount_in_after_fee = U512::from(amount_in)
        .checked_mul(U512::from(fee_complement))
        .ok_or(MathError::Overflow)?;
    let numerator = amount_in_after_fee
        .checked_mul(U512::from(reserve_out))
        .ok_or(MathError::Overflow)?;
    let reserve_in_scaled = U512::from(reserve_in)
        .checked_mul(U512::from(u64::from(FEE_DENOM)))
        .ok_or(MathError::Overflow)?;
    let denominator = reserve_in_scaled
        .checked_add(amount_in_after_fee)
        .ok_or(MathError::Overflow)?;

    let result = numerator / denominator;
    U256::try_from(result).map_err(|_| MathError::Overflow)
}

/// "How much do I need to put in to receive exactly this much out?".
///
/// Returns `floor(n / d) + 1`, matching Uniswap v2's `getAmountIn` exactly.
/// The unconditional `+1` is Uniswap's defensive ceiling — at the rare
/// exact-divisibility boundary, an `(n + d - 1) / d` formula would return one
/// wei less than the on-chain contract requires, causing the swap to revert
/// the K-invariant check. Always over-quoting by at most one wei is the
/// safe choice — it favors the pool and prevents reverts.
pub fn cpmm_quote_in(
    reserve_in: U256,
    reserve_out: U256,
    amount_out: U256,
    fee_ppm: u32,
) -> Result<U256, MathError> {
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err(MathError::ZeroReserves);
    }
    if amount_out.is_zero() {
        return Err(MathError::ZeroInput);
    }
    if fee_ppm >= FEE_DENOM {
        return Err(MathError::InvalidFee);
    }
    if amount_out >= reserve_out {
        return Err(MathError::InsufficientLiquidity);
    }

    let fee_complement = u64::from(FEE_DENOM - fee_ppm);
    let r_in_times_out = U512::from(reserve_in)
        .checked_mul(U512::from(amount_out))
        .ok_or(MathError::Overflow)?;
    let numerator = r_in_times_out
        .checked_mul(U512::from(u64::from(FEE_DENOM)))
        .ok_or(MathError::Overflow)?;
    let denominator = (U512::from(reserve_out) - U512::from(amount_out))
        .checked_mul(U512::from(fee_complement))
        .ok_or(MathError::Overflow)?;

    let result = (numerator / denominator)
        .checked_add(U512::from(1u64))
        .ok_or(MathError::Overflow)?;
    U256::try_from(result).map_err(|_| MathError::Overflow)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn canonical_uniswap_v2_quote() {
        // r_in=1000, r_out=1000, amount_in=100, fee=3000 ppm (0.3%)
        // amount_in_after_fee = 100 * 997_000 = 99_700_000
        // numerator           = 99_700_000 * 1000 = 99_700_000_000
        // denominator         = 1000 * 1_000_000 + 99_700_000 = 1_099_700_000
        // amount_out          = floor(99_700_000_000 / 1_099_700_000) = 90
        let out = cpmm_quote_out(
            U256::from(1000u64),
            U256::from(1000u64),
            U256::from(100u64),
            3000,
        )
        .unwrap();
        assert_eq!(out, U256::from(90u64));
    }

    #[test]
    fn quote_in_for_known_output_matches_uniswap_plus_one() {
        // Uniswap v2 getAmountIn: floor(numerator / denominator) + 1.
        // For amount_out=90 with reserves (1000,1000) and fee 3000 ppm:
        // numerator   = 1000 * 90 * 1_000_000        = 90_000_000_000
        // denominator = (1000 - 90) * 997_000        = 907_270_000
        // floor(n / d) = 99
        // result       = 99 + 1                       = 100
        let req_in = cpmm_quote_in(
            U256::from(1000u64),
            U256::from(1000u64),
            U256::from(90u64),
            3000,
        )
        .unwrap();
        assert_eq!(req_in, U256::from(100u64));
    }

    #[test]
    fn quote_in_unconditional_plus_one_at_exact_divisibility() {
        // Construct a case where n is exactly divisible by d, so the +1 fires
        // and our result is one wei greater than naive ceil would produce.
        // r_in=2, r_out=10, amount_out=5, fee=0 → fee_complement = 1_000_000
        // numerator   = 2 * 5 * 1_000_000 = 10_000_000
        // denominator = (10 - 5) * 1_000_000 = 5_000_000
        // floor(n/d) = 2 (exact); result = 2 + 1 = 3
        let req = cpmm_quote_in(U256::from(2u64), U256::from(10u64), U256::from(5u64), 0).unwrap();
        assert_eq!(req, U256::from(3u64));
    }

    #[test]
    fn zero_fee_matches_no_fee_formula() {
        // With fee=0, amount_in_after_fee == amount_in.
        // amount_out = r_out * amount_in / (r_in + amount_in) = 1000*100/1100 = 90 (floor)
        let out = cpmm_quote_out(
            U256::from(1000u64),
            U256::from(1000u64),
            U256::from(100u64),
            0,
        )
        .unwrap();
        assert_eq!(out, U256::from(90u64));
    }

    #[test]
    fn high_fee_at_boundary() {
        // 999_999 ppm = 99.9999% fee. Still allowed.
        let out = cpmm_quote_out(
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            999_999,
        )
        .unwrap();
        // amount_in_after_fee = 1_000_000 * 1 = 1_000_000
        // numerator = 1_000_000 * 1_000_000 = 1e12
        // denominator = 1_000_000 * 1_000_000 + 1_000_000 = 1_000_001_000_000
        // out = floor(1e12 / 1_000_001_000_000) = 0
        assert_eq!(out, U256::zero());
    }

    #[test]
    fn zero_reserves_in_rejected() {
        let r = cpmm_quote_out(U256::zero(), U256::from(1000u64), U256::from(10u64), 3000);
        assert_eq!(r, Err(MathError::ZeroReserves));
    }

    #[test]
    fn zero_reserves_out_rejected() {
        let r = cpmm_quote_out(U256::from(1000u64), U256::zero(), U256::from(10u64), 3000);
        assert_eq!(r, Err(MathError::ZeroReserves));
    }

    #[test]
    fn zero_input_rejected_out() {
        let r = cpmm_quote_out(U256::from(1000u64), U256::from(1000u64), U256::zero(), 3000);
        assert_eq!(r, Err(MathError::ZeroInput));
    }

    #[test]
    fn zero_input_rejected_in() {
        let r = cpmm_quote_in(U256::from(1000u64), U256::from(1000u64), U256::zero(), 3000);
        assert_eq!(r, Err(MathError::ZeroInput));
    }

    #[test]
    fn invalid_fee_rejected_out() {
        let r = cpmm_quote_out(
            U256::from(1000u64),
            U256::from(1000u64),
            U256::from(10u64),
            FEE_DENOM,
        );
        assert_eq!(r, Err(MathError::InvalidFee));
    }

    #[test]
    fn invalid_fee_rejected_in() {
        let r = cpmm_quote_in(
            U256::from(1000u64),
            U256::from(1000u64),
            U256::from(10u64),
            FEE_DENOM,
        );
        assert_eq!(r, Err(MathError::InvalidFee));
    }

    #[test]
    fn insufficient_liquidity_in_inverse() {
        let r = cpmm_quote_in(
            U256::from(1000u64),
            U256::from(1000u64),
            U256::from(1000u64),
            3000,
        );
        assert_eq!(r, Err(MathError::InsufficientLiquidity));
    }

    #[test]
    fn handles_pathological_reserves() {
        // Reserves near U256::MAX / 2 should not overflow due to U512 intermediates.
        let big = U256::MAX / 2;
        let in_ = U256::from(1_000_000_u64);
        let out = cpmm_quote_out(big, big, in_, 3000).unwrap();
        assert!(out > U256::zero());
        assert!(out <= big);
    }

    /// Property: `cpmm_quote_in(cpmm_quote_out(X))` must produce `X' <= X + 1`.
    /// Reason: floor in `cpmm_quote_out` plus Uniswap's `+1` in `cpmm_quote_in`
    /// means the recovered
    /// input is always <= the original (with possible equality).
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn quote_roundtrip_recovers_no_more_than_input(
            r_in_seed  in 1_000_000u64..u64::MAX / 4,
            r_out_seed in 1_000_000u64..u64::MAX / 4,
            amount_in  in 1u64..1_000_000_000u64,
            fee_ppm    in 0u32..50_000u32,
        ) {
            let r_in  = U256::from(r_in_seed);
            let r_out = U256::from(r_out_seed);
            let a_in  = U256::from(amount_in);

            let a_out = cpmm_quote_out(r_in, r_out, a_in, fee_ppm).unwrap();
            prop_assume!(!a_out.is_zero() && a_out < r_out);

            let required_in = cpmm_quote_in(r_in, r_out, a_out, fee_ppm).unwrap();
            // Uniswap-style +1 means required_in can be up to a_in + 1.
            let upper_bound = a_in.saturating_add(U256::from(1u64));
            prop_assert!(
                required_in <= upper_bound,
                "required_in={required_in}, a_in={a_in}"
            );
        }

        /// Monotonicity: larger amount_in → larger or equal amount_out.
        #[test]
        fn quote_out_monotone_in_input(
            r_in_seed  in 1_000_000u64..u64::MAX / 4,
            r_out_seed in 1_000_000u64..u64::MAX / 4,
            a1 in 1u64..1_000_000u64,
            delta in 1u64..1_000_000u64,
            fee_ppm in 0u32..50_000u32,
        ) {
            let r_in  = U256::from(r_in_seed);
            let r_out = U256::from(r_out_seed);
            let q1 = cpmm_quote_out(r_in, r_out, U256::from(a1), fee_ppm).unwrap();
            let q2 = cpmm_quote_out(r_in, r_out, U256::from(a1 + delta), fee_ppm).unwrap();
            prop_assert!(q2 >= q1);
        }
    }
}
