//! Client for the on-chain `DerrickExecutor` Cairo contract.
//!
//! `ExecutorClient` is a pure data-transformer: given a token, a min-profit
//! bound, and a list of inner calls, it serializes the calldata for
//! `DerrickExecutor::execute`. It does NOT perform I/O. Sending the resulting
//! transaction is the caller's job (in production via `starknet-rs`).
//!
//! Serialization layout (matches the Cairo ABI):
//!
//! ```text
//! [
//!     token_in: ContractAddress (1 felt),
//!     min_profit_low: u128 (1 felt),
//!     min_profit_high: u128 (1 felt),
//!     calls_len: u32 (1 felt),
//!     // for each call:
//!     to: ContractAddress (1 felt),
//!     selector: felt252 (1 felt),
//!     calldata_len: u32 (1 felt),
//!     calldata: felt252... (calldata_len felts),
//! ]
//! ```

use primitive_types::U256;
use starknet_types_core::felt::Felt;

use crate::error::ChainError;
use crate::provider::ProviderCall;
use crate::selectors::EXECUTE_SELECTOR;

/// One inner call inside an `execute` multicall.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorCall {
    pub to: Felt,
    pub selector: Felt,
    pub calldata: Vec<Felt>,
}

/// Builds calldata for the on-chain `DerrickExecutor` contract at `executor_address`.
#[derive(Clone, Debug)]
pub struct ExecutorClient {
    executor_address: Felt,
}

impl ExecutorClient {
    pub const fn new(executor_address: Felt) -> Self {
        Self { executor_address }
    }

    pub const fn executor_address(&self) -> Felt {
        self.executor_address
    }

    /// Serialize the calldata for `DerrickExecutor::execute(token_in, min_profit, calls)`.
    ///
    /// The Cairo `u256` is two felts: low (u128) then high (u128).
    /// `Array<Call>` is serialized as `len` then each `Call` flattened
    /// (`to`, `selector`, `calldata.len`, `calldata...`).
    ///
    /// Returns `ChainError::Encoding` if `calls.len()` or any inner calldata
    /// length exceeds `u32::MAX` (the on-chain `Array` length type).
    pub fn build_execute_calldata(
        token_in: Felt,
        min_profit: U256,
        calls: &[ExecutorCall],
    ) -> Result<Vec<Felt>, ChainError> {
        let calls_count: u32 = u32::try_from(calls.len())
            .map_err(|_| ChainError::Encoding("execute(): too many inner calls".into()))?;

        let mut buf = Vec::with_capacity(estimate_calldata_len(calls));
        buf.push(token_in);

        let (lo, hi) = u256_to_low_high(min_profit);
        buf.push(Felt::from(lo));
        buf.push(Felt::from(hi));

        buf.push(Felt::from(calls_count));
        for c in calls {
            let inner_len: u32 = u32::try_from(c.calldata.len()).map_err(|_| {
                ChainError::Encoding("execute(): inner calldata exceeds u32::MAX".into())
            })?;
            buf.push(c.to);
            buf.push(c.selector);
            buf.push(Felt::from(inner_len));
            buf.extend(c.calldata.iter().copied());
        }
        Ok(buf)
    }

    /// Wrap the calldata into a `ProviderCall` targeting the executor's
    /// `execute` selector. Hand this to a real provider to actually submit.
    pub fn build_invocation(
        &self,
        token_in: Felt,
        min_profit: U256,
        calls: &[ExecutorCall],
    ) -> Result<ProviderCall, ChainError> {
        Ok(ProviderCall {
            to: self.executor_address,
            selector: EXECUTE_SELECTOR,
            calldata: Self::build_execute_calldata(token_in, min_profit, calls)?,
        })
    }
}

fn estimate_calldata_len(calls: &[ExecutorCall]) -> usize {
    // token_in (1) + min_profit (2) + calls_len (1) + per call: to+selector+len (3) + calldata
    let per_call_overhead: usize = 3;
    let inner_total: usize = calls.iter().map(|c| c.calldata.len()).sum();
    4 + calls.len() * per_call_overhead + inner_total
}

fn u256_to_low_high(n: U256) -> (u128, u128) {
    let low = n.low_u128();
    let high = (n >> 128).low_u128();
    (low, high)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn felt(n: u64) -> Felt {
        Felt::from(n)
    }

    #[test]
    fn empty_calls_serialize_to_header_only() {
        let cd =
            ExecutorClient::build_execute_calldata(felt(0x1234), U256::from(100u64), &[]).unwrap();
        // [token_in=0x1234, min_profit_low=100, min_profit_high=0, calls_len=0]
        assert_eq!(cd.len(), 4);
        assert_eq!(cd[0], felt(0x1234));
        assert_eq!(cd[1], felt(100));
        assert_eq!(cd[2], felt(0));
        assert_eq!(cd[3], felt(0));
    }

    #[test]
    fn single_call_serializes_correctly() {
        let call = ExecutorCall {
            to: felt(0xaaaa),
            selector: felt(0xbb),
            calldata: vec![felt(1), felt(2), felt(3)],
        };
        let cd = ExecutorClient::build_execute_calldata(felt(0x1234), U256::from(999u64), &[call])
            .unwrap();
        // token_in, lo, hi, calls_len=1, to, selector, inner_len=3, 1, 2, 3
        assert_eq!(cd.len(), 4 + 3 + 3);
        assert_eq!(cd[0], felt(0x1234));
        assert_eq!(cd[1], felt(999));
        assert_eq!(cd[2], felt(0));
        assert_eq!(cd[3], felt(1));
        assert_eq!(cd[4], felt(0xaaaa));
        assert_eq!(cd[5], felt(0xbb));
        assert_eq!(cd[6], felt(3));
        assert_eq!(cd[7], felt(1));
        assert_eq!(cd[8], felt(2));
        assert_eq!(cd[9], felt(3));
    }

    #[test]
    fn multicall_serializes_in_order() {
        let c1 = ExecutorCall {
            to: felt(0xaaaa),
            selector: felt(0xbb),
            calldata: vec![felt(1)],
        };
        let c2 = ExecutorCall {
            to: felt(0xcccc),
            selector: felt(0xdd),
            calldata: vec![felt(2), felt(3)],
        };
        let cd = ExecutorClient::build_execute_calldata(felt(0x1234), U256::from(0u64), &[c1, c2])
            .unwrap();
        // 4 header + (3 + 1) + (3 + 2) = 13
        assert_eq!(cd.len(), 13);
        assert_eq!(cd[3], felt(2)); // calls_len
        assert_eq!(cd[4], felt(0xaaaa)); // c1.to
        assert_eq!(cd[5], felt(0xbb));
        assert_eq!(cd[6], felt(1)); // c1.calldata.len
        assert_eq!(cd[7], felt(1)); // c1.calldata[0]
        assert_eq!(cd[8], felt(0xcccc)); // c2.to
        assert_eq!(cd[9], felt(0xdd));
        assert_eq!(cd[10], felt(2)); // c2.calldata.len
        assert_eq!(cd[11], felt(2));
        assert_eq!(cd[12], felt(3));
    }

    #[test]
    fn u256_low_high_handles_max() {
        let max = U256::MAX;
        let (lo, hi) = u256_to_low_high(max);
        assert_eq!(lo, u128::MAX);
        assert_eq!(hi, u128::MAX);
    }

    #[test]
    fn u256_low_high_handles_split_at_boundary() {
        let n = (U256::from(0x1234u64) << 128) | U256::from(0xabcdu64);
        let (lo, hi) = u256_to_low_high(n);
        assert_eq!(lo, 0xabcd);
        assert_eq!(hi, 0x1234);
    }

    #[test]
    fn min_profit_max_serializes_both_halves() {
        let cd = ExecutorClient::build_execute_calldata(felt(0), U256::MAX, &[]).unwrap();
        assert_eq!(cd[1], Felt::from(u128::MAX));
        assert_eq!(cd[2], Felt::from(u128::MAX));
    }

    #[test]
    fn build_invocation_uses_execute_selector() {
        let client = ExecutorClient::new(felt(0xdead_beef));
        let inv = client
            .build_invocation(felt(1), U256::from(1u64), &[])
            .unwrap();
        assert_eq!(inv.to, felt(0xdead_beef));
        assert_eq!(inv.selector, EXECUTE_SELECTOR);
        assert_eq!(inv.calldata.len(), 4);
    }
}
