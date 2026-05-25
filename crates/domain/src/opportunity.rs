use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CoreError;
use crate::pool::PoolId;
use crate::token::TokenId;

/// A single swap step through one pool.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Hop {
    pub pool: PoolId,
    pub token_in: TokenId,
    pub token_out: TokenId,
}

/// A continuous chain of hops, validated on construction:
///   * non-empty
///   * `hop[i].token_out == hop[i+1].token_in`
///   * every hop has `token_in != token_out`
///
/// Whether the path forms a closed loop (arbitrage opportunity) is checked
/// separately via `is_cycle()`. A non-cycle path can still be valid for
/// quoting individual routes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Path {
    hops: Vec<Hop>,
}

impl Path {
    pub fn new(hops: Vec<Hop>) -> Result<Self, CoreError> {
        if hops.is_empty() {
            return Err(CoreError::EmptyPath);
        }
        for h in &hops {
            if h.token_in == h.token_out {
                return Err(CoreError::TrivialHop);
            }
        }
        for w in hops.windows(2) {
            if w[0].token_out != w[1].token_in {
                return Err(CoreError::DiscontinuousPath);
            }
        }
        Ok(Self { hops })
    }

    pub fn hops(&self) -> &[Hop] {
        &self.hops
    }

    pub fn len(&self) -> usize {
        self.hops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hops.is_empty()
    }

    pub fn start_token(&self) -> TokenId {
        self.hops[0].token_in
    }

    pub fn end_token(&self) -> TokenId {
        self.hops[self.hops.len() - 1].token_out
    }

    pub fn is_cycle(&self) -> bool {
        self.start_token() == self.end_token()
    }
}

/// A potentially-profitable arbitrage opportunity prior to sizing.
/// Carries a path and the wall-clock time of detection for latency tracking.
#[derive(Clone, Debug)]
pub struct Opportunity {
    pub id: Uuid,
    pub path: Path,
    /// Unix epoch milliseconds at the moment the detector emitted this.
    pub detected_at_ms: u64,
}

impl Opportunity {
    pub fn new(path: Path, detected_at_ms: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            path,
            detected_at_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::pool::{DexKind, FeeBps, PoolId};
    use crate::token::ContractAddress;
    use starknet_types_core::felt::Felt;

    fn tok(n: u128) -> TokenId {
        TokenId::new(ContractAddress::new(Felt::from(n)))
    }

    fn pid(addr: u128, dex: DexKind) -> PoolId {
        PoolId {
            address: ContractAddress::new(Felt::from(addr)),
            dex,
            fee: FeeBps::new(30),
        }
    }

    #[test]
    fn spatial_two_dex_path() {
        let usdc = tok(1);
        let eth = tok(2);
        let p = Path::new(vec![
            Hop {
                pool: pid(10, DexKind::JediSwapV1),
                token_in: usdc,
                token_out: eth,
            },
            Hop {
                pool: pid(11, DexKind::MySwapV1),
                token_in: eth,
                token_out: usdc,
            },
        ])
        .unwrap();
        assert!(p.is_cycle());
        assert_eq!(p.start_token(), usdc);
        assert_eq!(p.end_token(), usdc);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn triangular_path_is_cycle() {
        let usdc = tok(1);
        let eth = tok(2);
        let strk = tok(3);
        let p = Path::new(vec![
            Hop {
                pool: pid(10, DexKind::JediSwapV1),
                token_in: usdc,
                token_out: eth,
            },
            Hop {
                pool: pid(11, DexKind::Ekubo),
                token_in: eth,
                token_out: strk,
            },
            Hop {
                pool: pid(12, DexKind::MySwapV1),
                token_in: strk,
                token_out: usdc,
            },
        ])
        .unwrap();
        assert!(p.is_cycle());
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn discontinuous_path_rejected() {
        let usdc = tok(1);
        let eth = tok(2);
        let strk = tok(3);
        let res = Path::new(vec![
            Hop {
                pool: pid(10, DexKind::JediSwapV1),
                token_in: usdc,
                token_out: eth,
            },
            Hop {
                pool: pid(11, DexKind::MySwapV1),
                token_in: strk,
                token_out: usdc,
            },
        ]);
        assert!(matches!(res, Err(CoreError::DiscontinuousPath)));
    }

    #[test]
    fn empty_path_rejected() {
        assert!(matches!(Path::new(vec![]), Err(CoreError::EmptyPath)));
    }

    #[test]
    fn trivial_hop_rejected() {
        let res = Path::new(vec![Hop {
            pool: pid(10, DexKind::JediSwapV1),
            token_in: tok(1),
            token_out: tok(1),
        }]);
        assert!(matches!(res, Err(CoreError::TrivialHop)));
    }

    #[test]
    fn non_cycle_path_is_valid_but_not_cycle() {
        let usdc = tok(1);
        let eth = tok(2);
        let p = Path::new(vec![Hop {
            pool: pid(10, DexKind::JediSwapV1),
            token_in: usdc,
            token_out: eth,
        }])
        .unwrap();
        assert!(!p.is_cycle());
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn opportunity_has_unique_uuids() {
        let p = Path::new(vec![
            Hop {
                pool: pid(10, DexKind::JediSwapV1),
                token_in: tok(1),
                token_out: tok(2),
            },
            Hop {
                pool: pid(11, DexKind::MySwapV1),
                token_in: tok(2),
                token_out: tok(1),
            },
        ])
        .unwrap();
        let o1 = Opportunity::new(p.clone(), 1_700_000_000_000);
        let o2 = Opportunity::new(p, 1_700_000_000_000);
        assert_ne!(o1.id, o2.id);
    }
}
