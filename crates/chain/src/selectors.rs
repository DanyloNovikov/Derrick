//! Precomputed Starknet entry-point selectors used by derrick.
//!
//! Selectors are `starknet_keccak(name) & 0x7fff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff`
//! — i.e., a 250-bit hash. We compute them once at startup via the
//! `starknet::macros::selector!` macro so we don't repeat the hash on every call.

use starknet::macros::selector;
use starknet_types_core::felt::Felt;

/// Selector for `ArbExecutor::execute(token_in, min_profit, calls)`.
pub const EXECUTE_SELECTOR: Felt = selector!("execute");

/// Selector for `IERC20::balance_of(account)`.
pub const BALANCE_OF_SELECTOR: Felt = selector!("balance_of");

/// Selector for `IERC20::transfer(recipient, amount)`.
pub const TRANSFER_SELECTOR: Felt = selector!("transfer");

/// Selector for `IERC20::approve(spender, amount)`.
pub const APPROVE_SELECTOR: Felt = selector!("approve");

/// Selector for `JediSwapV1Pair::swap(amount0_out, amount1_out, to, data)`.
/// Uniswap v2 swap convention; reused by `JediSwap` v1 / `MySwap` v1 / 10kSwap.
pub const SWAP_SELECTOR: Felt = selector!("swap");

/// Selector for the `Executed` event emitted by `ArbExecutor::execute`.
/// Used by the inclusion watcher to filter events when computing realized
/// profit from a transaction receipt.
pub const EXECUTED_EVENT_SELECTOR: Felt = selector!("Executed");

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn selectors_are_nonzero() {
        assert_ne!(EXECUTE_SELECTOR, Felt::from(0u64));
        assert_ne!(BALANCE_OF_SELECTOR, Felt::from(0u64));
        assert_ne!(TRANSFER_SELECTOR, Felt::from(0u64));
        assert_ne!(APPROVE_SELECTOR, Felt::from(0u64));
    }

    #[test]
    fn selectors_are_distinct() {
        let all = [
            EXECUTE_SELECTOR,
            BALANCE_OF_SELECTOR,
            TRANSFER_SELECTOR,
            APPROVE_SELECTOR,
            SWAP_SELECTOR,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
