use std::fmt;

use serde::{Deserialize, Serialize};
use starknet_types_core::felt::Felt;

use crate::error::CoreError;

/// Starknet contract address — a 252-bit field element.
/// Newtype to keep contract addresses distinct from arbitrary `Felt` values.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContractAddress(Felt);

impl ContractAddress {
    pub const fn new(f: Felt) -> Self {
        Self(f)
    }

    pub const fn as_felt(&self) -> &Felt {
        &self.0
    }
}

impl fmt::Display for ContractAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl fmt::Debug for ContractAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContractAddress({self})")
    }
}

/// Identifies a token uniquely on Starknet by its ERC-20 contract address.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenId(ContractAddress);

impl TokenId {
    pub const fn new(addr: ContractAddress) -> Self {
        Self(addr)
    }

    pub const fn address(&self) -> ContractAddress {
        self.0
    }

    pub const fn as_felt(&self) -> &Felt {
        self.0.as_felt()
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenId({})", self.0)
    }
}

/// ERC-20 decimals. Wrapper prevents accidental int casts in money math.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct Decimals(u8);

impl Decimals {
    pub const fn new(d: u8) -> Self {
        Self(d)
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Short, validated token symbol. ASCII alphanumeric, dash, or underscore;
/// 1-16 chars.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(s: impl Into<String>) -> Result<Self, CoreError> {
        let s = s.into();
        if s.is_empty() || s.len() > 16 {
            return Err(CoreError::InvalidSymbol(s));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(CoreError::InvalidSymbol(s));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Token metadata; canonical source of symbol and decimals across the bot.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
pub struct Token {
    pub id: TokenId,
    pub symbol: Symbol,
    pub decimals: Decimals,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn valid_symbols() {
        assert!(Symbol::new("USDC").is_ok());
        assert!(Symbol::new("ETH").is_ok());
        assert!(Symbol::new("WBTC").is_ok());
        assert!(Symbol::new("STRK-USD").is_ok());
        assert!(Symbol::new("a_b_c_d").is_ok());
    }

    #[test]
    fn rejects_empty_symbol() {
        assert!(matches!(Symbol::new(""), Err(CoreError::InvalidSymbol(_))));
    }

    #[test]
    fn rejects_too_long_symbol() {
        assert!(matches!(
            Symbol::new("THIS_IS_WAY_TOO_LONG"),
            Err(CoreError::InvalidSymbol(_))
        ));
    }

    #[test]
    fn rejects_invalid_chars() {
        assert!(matches!(
            Symbol::new("US DC"),
            Err(CoreError::InvalidSymbol(_))
        ));
        assert!(matches!(
            Symbol::new("US$DC"),
            Err(CoreError::InvalidSymbol(_))
        ));
        assert!(matches!(
            Symbol::new("USD/C"),
            Err(CoreError::InvalidSymbol(_))
        ));
    }

    #[test]
    fn contract_address_display_is_hex() {
        let addr = ContractAddress::new(Felt::from(0xabcd_u64));
        let s = format!("{addr}");
        assert!(s.starts_with("0x"));
        assert!(s.to_lowercase().contains("abcd"));
    }

    #[test]
    fn token_id_serde_roundtrip() {
        let id = TokenId::new(ContractAddress::new(Felt::from(0x1234_u64)));
        let json = serde_json::to_string(&id).unwrap();
        let back: TokenId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}
