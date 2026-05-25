use std::fmt;

use serde::{Deserialize, Serialize};
use starknet_types_core::felt::Felt;

use crate::token::{ContractAddress, TokenId};

/// Family of DEX integrations. Used for routing and quote-method dispatch.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
pub enum DexKind {
    #[serde(rename = "ekubo")]
    Ekubo,
    #[serde(rename = "jediswap_v1")]
    JediSwapV1,
    #[serde(rename = "jediswap_v2")]
    JediSwapV2,
    #[serde(rename = "myswap_v1")]
    MySwapV1,
    #[serde(rename = "myswap_v2")]
    MySwapV2,
    #[serde(rename = "tenkswap")]
    TenkSwap,
    #[serde(rename = "sithswap_stable")]
    SithSwapStable,
    #[serde(rename = "sithswap_volatile")]
    SithSwapVolatile,
    #[serde(rename = "haiko")]
    Haiko,
}

impl DexKind {
    /// CL DEXes cannot be quoted with x*y=k on raw reserves —
    /// callers must use active-tick simulation or an on-chain quote.
    pub const fn is_concentrated(self) -> bool {
        matches!(
            self,
            Self::Ekubo | Self::JediSwapV2 | Self::MySwapV2 | Self::Haiko
        )
    }

    pub const fn is_stable_curve(self) -> bool {
        matches!(self, Self::SithSwapStable)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Ekubo => "ekubo",
            Self::JediSwapV1 => "jediswap_v1",
            Self::JediSwapV2 => "jediswap_v2",
            Self::MySwapV1 => "myswap_v1",
            Self::MySwapV2 => "myswap_v2",
            Self::TenkSwap => "tenkswap",
            Self::SithSwapStable => "sithswap_stable",
            Self::SithSwapVolatile => "sithswap_volatile",
            Self::Haiko => "haiko",
        }
    }
}

/// Pool fee in basis points (1 bps = 0.01%).
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct FeeBps(u32);

impl FeeBps {
    pub const fn new(bps: u32) -> Self {
        Self(bps)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    /// Fee in parts-per-million, against a `1_000_000` denominator.
    ///
    /// Use for integer-only math:
    ///
    /// ```text
    /// amount_after_fee = amount * (1_000_000 - fee.ppm()) / 1_000_000
    /// ```
    pub const fn ppm(self) -> u32 {
        self.0 * 100
    }
}

/// Globally-unique pool identifier.
/// `fee` is duplicated alongside `address` for fast filtering without state access.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
pub struct PoolId {
    pub address: ContractAddress,
    pub dex: DexKind,
    pub fee: FeeBps,
}

impl fmt::Display for PoolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@{}/{}bps",
            self.dex.name(),
            self.address,
            self.fee.get()
        )
    }
}

/// Pool metadata: identifier plus the two tokens it pairs.
///
/// **Invariant**: `token0` and `token1` MUST match the order the pool's
/// on-chain contract uses internally — i.e., the results of the pool's
/// `token0()` / `token1()` view functions, NOT a lexicographic sort done by
/// an indexer. Adapters use this ordering to map `Sync(reserve0, reserve1)`
/// event data to the correct token sides; a misconstructed `PoolMeta` will
/// silently produce wrong quotes.
///
/// Pool state lives inside the per-adapter implementation, not here.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
pub struct PoolMeta {
    pub id: PoolId,
    pub token0: TokenId,
    pub token1: TokenId,
}

impl PoolMeta {
    pub fn other_token(&self, token: TokenId) -> Option<TokenId> {
        if token == self.token0 {
            Some(self.token1)
        } else if token == self.token1 {
            Some(self.token0)
        } else {
            None
        }
    }

    pub fn contains(&self, token: TokenId) -> bool {
        token == self.token0 || token == self.token1
    }
}

/// On-chain ordering of an event: `(block, tx_index, event_index)`.
/// Events must be applied to local pool state in this lexicographic order.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
pub struct EventMeta {
    pub block: u64,
    pub tx_index: u32,
    pub event_index: u32,
}

impl EventMeta {
    pub const fn ordering_key(&self) -> (u64, u32, u32) {
        (self.block, self.tx_index, self.event_index)
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PoolEventKind {
    Swap,
    Mint,
    Burn,
    /// Some v2-forks emit `Sync` after every reserve change with the new reserves.
    Sync,
}

impl PoolEventKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Swap => "swap",
            Self::Mint => "mint",
            Self::Burn => "burn",
            Self::Sync => "sync",
        }
    }
}

/// Raw pool event captured by `price_watcher`, ready for an adapter to decode.
/// `data` is DEX-specific calldata that the adapter parses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolEvent {
    pub pool: PoolId,
    pub meta: EventMeta,
    pub kind: PoolEventKind,
    pub data: Vec<Felt>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn tok(n: u128) -> TokenId {
        TokenId::new(ContractAddress::new(Felt::from(n)))
    }

    fn pid(addr: u128, dex: DexKind, fee: u32) -> PoolId {
        PoolId {
            address: ContractAddress::new(Felt::from(addr)),
            dex,
            fee: FeeBps::new(fee),
        }
    }

    #[test]
    fn meta_other_token_resolves() {
        let a = tok(1);
        let b = tok(2);
        let meta = PoolMeta {
            id: pid(99, DexKind::JediSwapV1, 30),
            token0: a,
            token1: b,
        };
        assert_eq!(meta.other_token(a), Some(b));
        assert_eq!(meta.other_token(b), Some(a));
        assert_eq!(meta.other_token(tok(3)), None);
        assert!(meta.contains(a));
        assert!(meta.contains(b));
        assert!(!meta.contains(tok(3)));
    }

    #[test]
    fn dex_kind_classification() {
        assert!(DexKind::Ekubo.is_concentrated());
        assert!(DexKind::JediSwapV2.is_concentrated());
        assert!(DexKind::MySwapV2.is_concentrated());
        assert!(DexKind::Haiko.is_concentrated());
        assert!(!DexKind::JediSwapV1.is_concentrated());
        assert!(!DexKind::MySwapV1.is_concentrated());
        assert!(!DexKind::TenkSwap.is_concentrated());
        assert!(!DexKind::SithSwapStable.is_concentrated());

        assert!(DexKind::SithSwapStable.is_stable_curve());
        assert!(!DexKind::SithSwapVolatile.is_stable_curve());
    }

    #[test]
    fn fee_bps_ppm_conversion() {
        assert_eq!(FeeBps::new(30).ppm(), 3000);
        assert_eq!(FeeBps::new(100).ppm(), 10000);
        assert_eq!(FeeBps::new(1).ppm(), 100);
    }

    #[test]
    fn event_ordering_key() {
        let e = PoolEvent {
            pool: pid(1, DexKind::JediSwapV1, 30),
            meta: EventMeta {
                block: 42,
                tx_index: 3,
                event_index: 7,
            },
            kind: PoolEventKind::Swap,
            data: vec![],
        };
        assert_eq!(e.meta.ordering_key(), (42, 3, 7));
    }

    #[test]
    fn pool_id_display() {
        let p = pid(0x1234, DexKind::Ekubo, 50);
        let s = format!("{p}");
        assert!(s.contains("ekubo"));
        assert!(s.contains("50bps"));
    }

    #[test]
    fn dex_kind_serde_renames() {
        assert_eq!(
            serde_json::to_string(&DexKind::JediSwapV1).unwrap(),
            "\"jediswap_v1\""
        );
        assert_eq!(serde_json::to_string(&DexKind::Ekubo).unwrap(), "\"ekubo\"");
        let parsed: DexKind = serde_json::from_str("\"sithswap_stable\"").unwrap();
        assert_eq!(parsed, DexKind::SithSwapStable);
    }
}
