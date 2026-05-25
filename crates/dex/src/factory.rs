//! Build a boxed `Pool` adapter from metadata + a shared on-chain quoter.
//!
//! Lives here so wiring code (e.g., `bot::main`) doesn't need to know
//! which adapter struct each `DexKind` maps to. Returns `None` for DEX kinds
//! we haven't implemented yet.

use domain::{DexKind, Pool, PoolMeta};

use crate::jediswap_v1::JediSwapV1Pool;
use crate::quoter::SharedQuoter;

/// Trait-object pool. Matches the shape `bot::registry` expects.
pub type BoxedPool = Box<dyn Pool + Send + Sync>;

/// Build the right adapter for `meta.id.dex`. Returns `None` for unsupported
/// kinds — callers should log and skip.
pub fn build_pool(meta: PoolMeta, quoter: SharedQuoter) -> Option<BoxedPool> {
    match meta.id.dex {
        DexKind::JediSwapV1 => Some(Box::new(JediSwapV1Pool::new(meta, quoter))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::quoter::NoopQuoter;
    use domain::{ContractAddress, FeeBps, Felt, PoolId, TokenId};
    use std::sync::Arc;

    fn meta(dex: DexKind) -> PoolMeta {
        let usdc = TokenId::new(ContractAddress::new(Felt::from(1u64)));
        let eth = TokenId::new(ContractAddress::new(Felt::from(2u64)));
        PoolMeta {
            id: PoolId {
                address: ContractAddress::new(Felt::from(0xaa_u64)),
                dex,
                fee: FeeBps::new(30),
            },
            token0: usdc,
            token1: eth,
        }
    }

    #[test]
    fn builds_jediswap_v1() {
        let q: SharedQuoter = Arc::new(NoopQuoter);
        let p = build_pool(meta(DexKind::JediSwapV1), q);
        assert!(p.is_some());
    }

    #[test]
    fn unsupported_dex_returns_none() {
        let q: SharedQuoter = Arc::new(NoopQuoter);
        let p = build_pool(meta(DexKind::Ekubo), q);
        assert!(p.is_none());
    }
}
