//! Translate a sized arbitrage trade into a list of [`ExecutorCall`]s.
//!
//! Step 10.2 supports `JediSwap` v1 (Uniswap v2 fork): for the first hop the
//! input token is `transfer`ed from the executor's balance directly to the
//! pool; the pool's `swap` then sends its output to the next pool (or to the
//! executor for the final hop).
//!
//! Layout for a 2-hop spatial cycle (USDC → ETH → USDC via pools A and B):
//!
//! ```text
//! [
//!   IERC20(USDC).transfer(A,        amount_in),
//!   A.swap(0, eth_out, B,           empty_data),
//!   B.swap(usdc_out, 0, executor,   empty_data),
//! ]
//! ```
//!
//! Other DEX kinds (concentrated liquidity Ekubo, etc.) require different
//! call shapes and are not in this step; the builder returns
//! [`BuildError::UnsupportedDex`].

use chain::{ExecutorCall, SWAP_SELECTOR, TRANSFER_SELECTOR};
use domain::{DexKind, Felt, PoolMeta, Quote, U256};
use strategy::SizedTrade;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("unsupported DEX kind: {0:?}")]
    UnsupportedDex(DexKind),

    #[error(
        "hop/meta count mismatch: path has {path_hops} hops, pool_metas has {pool_metas} entries"
    )]
    HopCountMismatch { path_hops: usize, pool_metas: usize },

    #[error("token at hop {hop} is not in the pool's pair")]
    TokenNotInPool { hop: usize },
}

/// Build the on-chain call sequence to execute `sized`.
///
/// `pool_metas[i]` MUST be the meta of the pool referenced by
/// `sized.outcome.path.hops()[i]`. The caller (the detector) snapshots metas
/// from the registry guards before dropping the read locks.
///
/// `executor_address` is the destination for the FINAL hop's output (the
/// `ArbExecutor`'s own address — so the post-swap balance increase shows up
/// against `executor.balance_of(self)`).
pub fn build_path_calls(
    sized: &SizedTrade,
    pool_metas: &[PoolMeta],
    executor_address: Felt,
) -> Result<Vec<ExecutorCall>, BuildError> {
    let hops = sized.outcome.path.hops();
    let quotes = &sized.outcome.hop_quotes;
    if hops.len() != pool_metas.len() {
        return Err(BuildError::HopCountMismatch {
            path_hops: hops.len(),
            pool_metas: pool_metas.len(),
        });
    }

    let mut calls = Vec::with_capacity(hops.len() + 1);

    for i in 0..hops.len() {
        let hop = &hops[i];
        let quote = &quotes[i];
        let meta = &pool_metas[i];

        let recipient_felt = if i + 1 < hops.len() {
            *hops[i + 1].pool.address.as_felt()
        } else {
            executor_address
        };

        match hop.pool.dex {
            DexKind::JediSwapV1 => {
                if i == 0 {
                    calls.push(build_erc20_transfer(
                        *hop.token_in.as_felt(),
                        *hop.pool.address.as_felt(),
                        quote.amount_in.raw,
                    ));
                }
                calls.push(build_uniswap_v2_swap(meta, quote, recipient_felt, i)?);
            }
            other => return Err(BuildError::UnsupportedDex(other)),
        }
    }

    Ok(calls)
}

fn build_erc20_transfer(token: Felt, recipient: Felt, amount: U256) -> ExecutorCall {
    let (lo, hi) = u256_split(amount);
    ExecutorCall {
        to: token,
        selector: TRANSFER_SELECTOR,
        calldata: vec![recipient, Felt::from(lo), Felt::from(hi)],
    }
}

/// Uniswap v2 `swap(amount0_out, amount1_out, to, data)`. Data is empty for
/// straight swaps (no flash callback).
fn build_uniswap_v2_swap(
    meta: &PoolMeta,
    quote: &Quote,
    recipient: Felt,
    hop_index: usize,
) -> Result<ExecutorCall, BuildError> {
    let (amount0_out, amount1_out) = if quote.amount_in.token == meta.token0 {
        (U256::zero(), quote.amount_out.raw)
    } else if quote.amount_in.token == meta.token1 {
        (quote.amount_out.raw, U256::zero())
    } else {
        return Err(BuildError::TokenNotInPool { hop: hop_index });
    };
    let (lo0, hi0) = u256_split(amount0_out);
    let (lo1, hi1) = u256_split(amount1_out);
    Ok(ExecutorCall {
        to: *meta.id.address.as_felt(),
        selector: SWAP_SELECTOR,
        calldata: vec![
            Felt::from(lo0),
            Felt::from(hi0),
            Felt::from(lo1),
            Felt::from(hi1),
            recipient,
            Felt::from(0u64), // empty Span<felt252> data
        ],
    })
}

fn u256_split(n: U256) -> (u128, u128) {
    let low = n.low_u128();
    let high = (n >> 128).low_u128();
    (low, high)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use domain::{Amount, ContractAddress, FeeBps, Felt, Hop, Path, PoolId, SignedAmount, TokenId};
    use strategy::PathOutcome;

    fn tok(n: u64) -> TokenId {
        TokenId::new(ContractAddress::new(Felt::from(n)))
    }

    fn pid(addr: u64, dex: DexKind) -> PoolId {
        PoolId {
            address: ContractAddress::new(Felt::from(addr)),
            dex,
            fee: FeeBps::new(30),
        }
    }

    fn synth_sized() -> SizedTrade {
        let usdc = tok(1);
        let eth = tok(2);
        let p_a = pid(0x0a, DexKind::JediSwapV1);
        let p_b = pid(0x0b, DexKind::JediSwapV1);
        let path = Path::new(vec![
            Hop {
                pool: p_a,
                token_in: usdc,
                token_out: eth,
            },
            Hop {
                pool: p_b,
                token_in: eth,
                token_out: usdc,
            },
        ])
        .unwrap();

        let q1 = Quote {
            pool: p_a,
            amount_in: Amount::new(usdc, U256::from(10_000u64)),
            amount_out: Amount::new(eth, U256::from(9u64)),
            gas_estimate: 0,
            state_version: 1,
        };
        let q2 = Quote {
            pool: p_b,
            amount_in: Amount::new(eth, U256::from(9u64)),
            amount_out: Amount::new(usdc, U256::from(17_681u64)),
            gas_estimate: 0,
            state_version: 1,
        };
        let outcome = PathOutcome {
            path,
            amount_in: Amount::new(usdc, U256::from(10_000u64)),
            amount_out: Amount::new(usdc, U256::from(17_681u64)),
            gas_cost: Amount::new(usdc, U256::from(100u64)),
            safety_margin: Amount::new(usdc, U256::from(200u64)),
            hop_quotes: vec![q1, q2],
            state_versions: vec![1, 1],
            gross: SignedAmount::positive(usdc, U256::from(7_681u64)),
            net: SignedAmount::positive(usdc, U256::from(7_381u64)),
        };
        SizedTrade {
            amount_in: Amount::new(usdc, U256::from(10_000u64)),
            outcome,
            iterations: 1,
        }
    }

    fn synth_pool_metas() -> Vec<PoolMeta> {
        let usdc = tok(1);
        let eth = tok(2);
        vec![
            PoolMeta {
                id: pid(0x0a, DexKind::JediSwapV1),
                token0: usdc,
                token1: eth,
            },
            PoolMeta {
                id: pid(0x0b, DexKind::JediSwapV1),
                token0: usdc,
                token1: eth,
            },
        ]
    }

    #[test]
    fn builds_transfer_plus_two_swaps_for_2_hop_jediswap() {
        let sized = synth_sized();
        let metas = synth_pool_metas();
        let executor = Felt::from(0xffff_u64);
        let calls = build_path_calls(&sized, &metas, executor).unwrap();

        // Expect 3 calls: transfer USDC→A, swap on A → B, swap on B → executor.
        assert_eq!(calls.len(), 3);

        // 1. Transfer USDC to pool A.
        assert_eq!(calls[0].to, Felt::from(1u64)); // USDC address
        assert_eq!(calls[0].selector, TRANSFER_SELECTOR);
        assert_eq!(calls[0].calldata[0], Felt::from(0x0a_u64)); // recipient = pool A
        assert_eq!(calls[0].calldata[1], Felt::from(10_000_u64)); // lo
        assert_eq!(calls[0].calldata[2], Felt::from(0u64)); // hi

        // 2. Swap on pool A: amount0_out = 0 (USDC out), amount1_out = 9 (ETH out)
        //    since amount_in.token (USDC) == meta.token0.
        assert_eq!(calls[1].to, Felt::from(0x0a_u64));
        assert_eq!(calls[1].selector, SWAP_SELECTOR);
        assert_eq!(calls[1].calldata[0], Felt::from(0u64)); // amount0_out lo
        assert_eq!(calls[1].calldata[2], Felt::from(9u64)); // amount1_out lo
        assert_eq!(calls[1].calldata[4], Felt::from(0x0b_u64)); // recipient = pool B
        assert_eq!(calls[1].calldata[5], Felt::from(0u64)); // data len 0

        // 3. Swap on pool B: amount_in.token (ETH) == meta.token1, so amount0_out
        //    (USDC out) = 17_681, amount1_out = 0. Recipient = executor.
        assert_eq!(calls[2].to, Felt::from(0x0b_u64));
        assert_eq!(calls[2].selector, SWAP_SELECTOR);
        assert_eq!(calls[2].calldata[0], Felt::from(17_681_u64));
        assert_eq!(calls[2].calldata[2], Felt::from(0u64));
        assert_eq!(calls[2].calldata[4], executor);
    }

    #[test]
    fn rejects_hop_count_mismatch() {
        let sized = synth_sized();
        let metas = vec![synth_pool_metas()[0].clone()]; // only one
        let r = build_path_calls(&sized, &metas, Felt::from(0u64));
        assert!(matches!(r, Err(BuildError::HopCountMismatch { .. })));
    }

    #[test]
    fn rejects_unsupported_dex() {
        let mut sized = synth_sized();
        // Mutate first hop's pool dex to Ekubo (concentrated, unsupported here).
        sized.outcome.hop_quotes[0].pool.dex = DexKind::Ekubo;
        let mut metas = synth_pool_metas();
        metas[0].id.dex = DexKind::Ekubo;
        // The path's hops still hold the old dex value — patch:
        let hops = sized.outcome.path.hops().to_vec();
        let mut new_hops = hops;
        new_hops[0].pool.dex = DexKind::Ekubo;
        sized.outcome.path = Path::new(new_hops).unwrap();
        let r = build_path_calls(&sized, &metas, Felt::from(0u64));
        assert!(matches!(r, Err(BuildError::UnsupportedDex(DexKind::Ekubo))));
    }
}
