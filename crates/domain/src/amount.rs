use std::fmt;

use primitive_types::U256;
use serde::{Deserialize, Serialize};

use crate::error::AmountError;
use crate::token::TokenId;

/// A raw on-chain amount tagged with the token it belongs to.
///
/// Arithmetic between two `Amount`s is only valid when the tokens match.
/// `checked_add` / `checked_sub` return `AmountError::TokenMismatch` otherwise.
/// This prevents the most common decimals-confusion bug at the type level.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Amount {
    pub token: TokenId,
    pub raw: U256,
}

impl Amount {
    pub const fn new(token: TokenId, raw: U256) -> Self {
        Self { token, raw }
    }

    pub fn zero(token: TokenId) -> Self {
        Self {
            token,
            raw: U256::zero(),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.raw.is_zero()
    }

    pub fn checked_add(self, other: Self) -> Result<Self, AmountError> {
        self.require_same_token(other)?;
        let raw = self
            .raw
            .checked_add(other.raw)
            .ok_or(AmountError::Overflow)?;
        Ok(Self {
            token: self.token,
            raw,
        })
    }

    pub fn checked_sub(self, other: Self) -> Result<Self, AmountError> {
        self.require_same_token(other)?;
        let raw = self
            .raw
            .checked_sub(other.raw)
            .ok_or(AmountError::Underflow)?;
        Ok(Self {
            token: self.token,
            raw,
        })
    }

    pub fn checked_mul_u256(self, factor: U256) -> Result<Self, AmountError> {
        let raw = self.raw.checked_mul(factor).ok_or(AmountError::Overflow)?;
        Ok(Self {
            token: self.token,
            raw,
        })
    }

    pub fn checked_div_u256(self, divisor: U256) -> Result<Self, AmountError> {
        if divisor.is_zero() {
            return Err(AmountError::DivisionByZero);
        }
        let raw = self.raw / divisor;
        Ok(Self {
            token: self.token,
            raw,
        })
    }

    /// True iff `self.raw >= other.raw`. Tokens must match.
    pub fn at_least(self, other: Self) -> Result<bool, AmountError> {
        self.require_same_token(other)?;
        Ok(self.raw >= other.raw)
    }

    /// Total ordering on `raw`. Tokens must match.
    pub fn cmp_same_token(self, other: Self) -> Result<std::cmp::Ordering, AmountError> {
        self.require_same_token(other)?;
        Ok(self.raw.cmp(&other.raw))
    }

    fn require_same_token(self, other: Self) -> Result<(), AmountError> {
        if self.token == other.token {
            Ok(())
        } else {
            Err(AmountError::TokenMismatch {
                lhs: self.token,
                rhs: other.token,
            })
        }
    }
}

impl fmt::Debug for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Amount({} of {})", self.raw, self.token)
    }
}

/// A signed token amount. Useful for representing profit/loss where the value
/// can go negative.
///
/// Like [`Amount`], arithmetic between two `SignedAmount`s requires matching
/// tokens — `TokenMismatch` otherwise.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SignedAmount {
    token: TokenId,
    abs: U256,
    negative: bool,
}

impl SignedAmount {
    pub const fn zero(token: TokenId) -> Self {
        Self {
            token,
            abs: U256::zero(),
            negative: false,
        }
    }

    /// Construct a non-negative signed amount.
    pub const fn positive(token: TokenId, abs: U256) -> Self {
        Self {
            token,
            abs,
            negative: false,
        }
    }

    /// Construct a non-positive signed amount. Passing `U256::zero()` yields `+0`.
    pub fn negative(token: TokenId, abs: U256) -> Self {
        let negative = !abs.is_zero();
        Self {
            token,
            abs,
            negative,
        }
    }

    pub const fn token(&self) -> TokenId {
        self.token
    }

    pub const fn abs(&self) -> U256 {
        self.abs
    }

    pub const fn is_negative(&self) -> bool {
        self.negative
    }

    pub fn is_positive(&self) -> bool {
        !self.negative && !self.abs.is_zero()
    }

    pub fn is_zero(&self) -> bool {
        self.abs.is_zero()
    }

    /// `minuend - subtrahend`, signed.
    pub fn from_diff(minuend: Amount, subtrahend: Amount) -> Result<Self, AmountError> {
        if minuend.token != subtrahend.token {
            return Err(AmountError::TokenMismatch {
                lhs: minuend.token,
                rhs: subtrahend.token,
            });
        }
        if minuend.raw >= subtrahend.raw {
            Ok(Self::positive(minuend.token, minuend.raw - subtrahend.raw))
        } else {
            Ok(Self::negative(minuend.token, subtrahend.raw - minuend.raw))
        }
    }

    /// `self - amount` where `amount` is unsigned. Tokens must match.
    pub fn checked_sub_amount(self, amount: Amount) -> Result<Self, AmountError> {
        if self.token != amount.token {
            return Err(AmountError::TokenMismatch {
                lhs: self.token,
                rhs: amount.token,
            });
        }
        if self.negative {
            // (-self.abs) - amount = -(self.abs + amount)
            let new_abs = self
                .abs
                .checked_add(amount.raw)
                .ok_or(AmountError::Overflow)?;
            Ok(Self::negative(self.token, new_abs))
        } else if self.abs >= amount.raw {
            Ok(Self::positive(self.token, self.abs - amount.raw))
        } else {
            Ok(Self::negative(self.token, amount.raw - self.abs))
        }
    }

    /// Total ordering on signed values; negatives sort below positives.
    pub fn cmp_same_token(self, other: Self) -> Result<std::cmp::Ordering, AmountError> {
        if self.token != other.token {
            return Err(AmountError::TokenMismatch {
                lhs: self.token,
                rhs: other.token,
            });
        }
        let ord = match (self.is_negative(), other.is_negative()) {
            (false, false) => self.abs.cmp(&other.abs),
            (true, true) => other.abs.cmp(&self.abs),
            (false, true) => std::cmp::Ordering::Greater,
            (true, false) => std::cmp::Ordering::Less,
        };
        Ok(ord)
    }
}

impl fmt::Debug for SignedAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.negative { "-" } else { "+" };
        write!(f, "SignedAmount({sign}{} of {})", self.abs, self.token)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::token::ContractAddress;
    use starknet_types_core::felt::Felt;

    fn tok(n: u128) -> TokenId {
        TokenId::new(ContractAddress::new(Felt::from(n)))
    }

    #[test]
    fn checked_add_same_token() {
        let a = Amount::new(tok(1), U256::from(100u64));
        let b = Amount::new(tok(1), U256::from(50u64));
        let sum = a.checked_add(b).unwrap();
        assert_eq!(sum.raw, U256::from(150u64));
        assert_eq!(sum.token, tok(1));
    }

    #[test]
    fn checked_add_different_tokens_fails() {
        let a = Amount::new(tok(1), U256::from(100u64));
        let b = Amount::new(tok(2), U256::from(50u64));
        assert!(matches!(
            a.checked_add(b),
            Err(AmountError::TokenMismatch { .. })
        ));
    }

    #[test]
    fn checked_sub_underflow() {
        let a = Amount::new(tok(1), U256::from(10u64));
        let b = Amount::new(tok(1), U256::from(50u64));
        assert!(matches!(a.checked_sub(b), Err(AmountError::Underflow)));
    }

    #[test]
    fn checked_add_overflow() {
        let a = Amount::new(tok(1), U256::MAX);
        let b = Amount::new(tok(1), U256::from(1u64));
        assert!(matches!(a.checked_add(b), Err(AmountError::Overflow)));
    }

    #[test]
    fn checked_div_by_zero() {
        let a = Amount::new(tok(1), U256::from(100u64));
        assert!(matches!(
            a.checked_div_u256(U256::zero()),
            Err(AmountError::DivisionByZero)
        ));
    }

    #[test]
    fn at_least_compares_values() {
        let a = Amount::new(tok(1), U256::from(100u64));
        let b = Amount::new(tok(1), U256::from(100u64));
        let c = Amount::new(tok(1), U256::from(99u64));
        assert!(a.at_least(b).unwrap());
        assert!(a.at_least(c).unwrap());
        assert!(!c.at_least(a).unwrap());
    }

    #[test]
    fn at_least_rejects_token_mismatch() {
        let a = Amount::new(tok(1), U256::from(100u64));
        let b = Amount::new(tok(2), U256::from(50u64));
        assert!(matches!(
            a.at_least(b),
            Err(AmountError::TokenMismatch { .. })
        ));
    }

    #[test]
    fn is_zero_works() {
        assert!(Amount::zero(tok(1)).is_zero());
        assert!(!Amount::new(tok(1), U256::from(1u64)).is_zero());
    }

    // ---------- SignedAmount ----------

    #[test]
    fn signed_zero_is_not_positive_or_negative() {
        let z = SignedAmount::zero(tok(1));
        assert!(z.is_zero());
        assert!(!z.is_positive());
        assert!(!z.is_negative());
    }

    #[test]
    fn signed_negative_of_zero_is_zero() {
        let z = SignedAmount::negative(tok(1), U256::zero());
        assert!(z.is_zero());
        assert!(!z.is_negative());
    }

    #[test]
    fn signed_from_diff_positive() {
        let a = Amount::new(tok(1), U256::from(150u64));
        let b = Amount::new(tok(1), U256::from(100u64));
        let s = SignedAmount::from_diff(a, b).unwrap();
        assert!(s.is_positive());
        assert_eq!(s.abs(), U256::from(50u64));
    }

    #[test]
    fn signed_from_diff_negative() {
        let a = Amount::new(tok(1), U256::from(100u64));
        let b = Amount::new(tok(1), U256::from(150u64));
        let s = SignedAmount::from_diff(a, b).unwrap();
        assert!(s.is_negative());
        assert_eq!(s.abs(), U256::from(50u64));
    }

    #[test]
    fn signed_from_diff_rejects_token_mismatch() {
        let a = Amount::new(tok(1), U256::from(100u64));
        let b = Amount::new(tok(2), U256::from(50u64));
        assert!(matches!(
            SignedAmount::from_diff(a, b),
            Err(AmountError::TokenMismatch { .. })
        ));
    }

    #[test]
    fn signed_checked_sub_amount_positive_remains_positive() {
        let s = SignedAmount::positive(tok(1), U256::from(100u64));
        let a = Amount::new(tok(1), U256::from(30u64));
        let r = s.checked_sub_amount(a).unwrap();
        assert!(r.is_positive());
        assert_eq!(r.abs(), U256::from(70u64));
    }

    #[test]
    fn signed_checked_sub_amount_positive_flips_negative() {
        let s = SignedAmount::positive(tok(1), U256::from(30u64));
        let a = Amount::new(tok(1), U256::from(100u64));
        let r = s.checked_sub_amount(a).unwrap();
        assert!(r.is_negative());
        assert_eq!(r.abs(), U256::from(70u64));
    }

    #[test]
    fn signed_checked_sub_amount_negative_grows() {
        let s = SignedAmount::negative(tok(1), U256::from(30u64));
        let a = Amount::new(tok(1), U256::from(20u64));
        let r = s.checked_sub_amount(a).unwrap();
        assert!(r.is_negative());
        assert_eq!(r.abs(), U256::from(50u64));
    }

    #[test]
    fn signed_cmp_same_token_orders_correctly() {
        use std::cmp::Ordering;
        let pos = SignedAmount::positive(tok(1), U256::from(5u64));
        let neg = SignedAmount::negative(tok(1), U256::from(3u64));
        let zero = SignedAmount::zero(tok(1));
        assert_eq!(pos.cmp_same_token(neg).unwrap(), Ordering::Greater);
        assert_eq!(neg.cmp_same_token(pos).unwrap(), Ordering::Less);
        assert_eq!(pos.cmp_same_token(zero).unwrap(), Ordering::Greater);
        assert_eq!(neg.cmp_same_token(zero).unwrap(), Ordering::Less);

        // both negative: more-negative is smaller
        let more_neg = SignedAmount::negative(tok(1), U256::from(10u64));
        assert_eq!(more_neg.cmp_same_token(neg).unwrap(), Ordering::Less);
        // both positive: bigger abs is greater
        let more_pos = SignedAmount::positive(tok(1), U256::from(10u64));
        assert_eq!(more_pos.cmp_same_token(pos).unwrap(), Ordering::Greater);
    }
}
