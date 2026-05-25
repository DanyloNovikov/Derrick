//! Pre-submit on-chain simulation.
//!
//! Project `critical_rules` mandate calling `starknet_call` on the executor's
//! `execute` entry point with `BlockTarget::Pending` immediately before each
//! submission. The simulation returns the realized profit on success or
//! propagates the contract's revert reason; either way we cross-check against
//! the off-chain model and reject if they diverge by more than `MAX_DIVERGENCE_BPS`.

use primitive_types::{U256, U512};
use starknet_types_core::felt::Felt;

use crate::error::ChainError;
use crate::executor::{ExecutorCall, ExecutorClient};
use crate::provider::{BlockTarget, Provider};

/// Maximum allowed off-chain↔on-chain divergence, in basis points.
/// 500 bps = 5% per project `critical_rules`. Above this we skip + alert.
pub const MAX_DIVERGENCE_BPS: u32 = 500;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SimulationResult {
    /// Profit returned by the executor's `execute()` call. Always non-negative
    /// since the contract reverts on insufficient profit.
    pub realized_profit: U256,
    /// `|realized - expected| / expected` in bps. Saturates at `u32::MAX`.
    pub divergence_bps: u32,
}

/// Run `ArbExecutor::execute` as a read-only call against the Pending block
/// and parse the return value.
///
/// Returns `ChainError::ModelDivergence` if the realized profit deviates from
/// `expected_profit` by more than [`MAX_DIVERGENCE_BPS`] — this signals the
/// off-chain model and the on-chain truth disagree, and we should NOT send
/// the transaction.
pub async fn simulate_execute(
    provider: &dyn Provider,
    executor: &ExecutorClient,
    token_in: Felt,
    min_profit: U256,
    calls: &[ExecutorCall],
    expected_profit: U256,
) -> Result<SimulationResult, ChainError> {
    let invocation = executor.build_invocation(token_in, min_profit, calls)?;
    let return_data = provider.call(invocation, BlockTarget::Pending).await?;

    if return_data.len() != 2 {
        return Err(ChainError::Encoding(format!(
            "execute() must return u256 (2 felts), got {} felts",
            return_data.len()
        )));
    }

    let realized_profit = felts_to_u256(return_data[0], return_data[1])?;
    let divergence_bps = compute_divergence_bps(realized_profit, expected_profit);

    if divergence_bps > MAX_DIVERGENCE_BPS {
        return Err(ChainError::ModelDivergence(format!(
            "realized={realized_profit}, expected={expected_profit}, divergence={divergence_bps}bps"
        )));
    }

    Ok(SimulationResult {
        realized_profit,
        divergence_bps,
    })
}

fn felts_to_u256(low: Felt, high: Felt) -> Result<U256, ChainError> {
    let low_u128 = u128::try_from(low)
        .map_err(|_| ChainError::InvalidFelt("u256 low half exceeds u128".into()))?;
    let high_u128 = u128::try_from(high)
        .map_err(|_| ChainError::InvalidFelt("u256 high half exceeds u128".into()))?;
    Ok((U256::from(high_u128) << 128) | U256::from(low_u128))
}

/// `|realized - expected| * 10_000 / expected`, saturating to `u32::MAX`.
/// U512 intermediates so the `* 10_000` step never overflows.
fn compute_divergence_bps(realized: U256, expected: U256) -> u32 {
    if expected.is_zero() {
        // No meaningful ratio against zero. If realized is also zero, perfect
        // agreement; otherwise, treat as max divergence so the gate trips.
        return if realized.is_zero() { 0 } else { u32::MAX };
    }
    let diff = if realized > expected {
        realized - expected
    } else {
        expected - realized
    };
    let bps_512 = U512::from(diff) * U512::from(10_000u64) / U512::from(expected);
    if bps_512 > U512::from(u32::MAX) {
        u32::MAX
    } else {
        bps_512.as_u64() as u32
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::provider::ProviderCall;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use tokio::sync::Mutex;

    fn felt(n: u64) -> Felt {
        Felt::from(n)
    }

    struct MockProvider {
        responses: Mutex<VecDeque<Result<Vec<Felt>, ChainError>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<Result<Vec<Felt>, ChainError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn call(
            &self,
            _call: ProviderCall,
            _block: BlockTarget,
        ) -> Result<Vec<Felt>, ChainError> {
            self.responses
                .lock()
                .await
                .pop_front()
                .unwrap_or_else(|| Err(ChainError::Rpc("mock exhausted".into())))
        }

        async fn get_nonce(&self, _account: Felt, _block: BlockTarget) -> Result<Felt, ChainError> {
            Ok(felt(0))
        }

        async fn get_tx_status(
            &self,
            _tx_hash: Felt,
        ) -> Result<crate::provider::TxStatus, ChainError> {
            Ok(crate::provider::TxStatus::NotFound)
        }
    }

    fn u256_to_felt_pair(n: U256) -> (Felt, Felt) {
        let low = n.low_u128();
        let high = (n >> 128).low_u128();
        (Felt::from(low), Felt::from(high))
    }

    #[tokio::test]
    async fn simulate_success_returns_profit_and_zero_divergence() {
        let expected = U256::from(1000u64);
        let (lo, hi) = u256_to_felt_pair(expected);
        let provider = MockProvider::new(vec![Ok(vec![lo, hi])]);
        let executor = ExecutorClient::new(felt(0xabc));

        let r = simulate_execute(
            &provider,
            &executor,
            felt(1),
            U256::from(0u64),
            &[],
            expected,
        )
        .await
        .unwrap();
        assert_eq!(r.realized_profit, expected);
        assert_eq!(r.divergence_bps, 0);
    }

    #[tokio::test]
    async fn simulate_divergence_50pct_is_rejected() {
        let realized = U256::from(50u64);
        let (lo, hi) = u256_to_felt_pair(realized);
        let provider = MockProvider::new(vec![Ok(vec![lo, hi])]);
        let executor = ExecutorClient::new(felt(0xabc));

        // Expected 100, realized 50 → 50% divergence (5000 bps) >> 500 bps cap.
        let r = simulate_execute(
            &provider,
            &executor,
            felt(1),
            U256::from(0u64),
            &[],
            U256::from(100u64),
        )
        .await;
        assert!(matches!(r, Err(ChainError::ModelDivergence(_))));
    }

    #[tokio::test]
    async fn simulate_within_threshold_passes() {
        // Realized 970, expected 1000 → 3% divergence (300 bps) < 500 cap.
        let realized = U256::from(970u64);
        let (lo, hi) = u256_to_felt_pair(realized);
        let provider = MockProvider::new(vec![Ok(vec![lo, hi])]);
        let executor = ExecutorClient::new(felt(0xabc));

        let r = simulate_execute(
            &provider,
            &executor,
            felt(1),
            U256::from(0u64),
            &[],
            U256::from(1000u64),
        )
        .await
        .unwrap();
        assert_eq!(r.divergence_bps, 300);
    }

    #[tokio::test]
    async fn simulate_provider_reverted_propagates_error() {
        let provider = MockProvider::new(vec![Err(ChainError::Reverted(
            "INSUFFICIENT_PROFIT".into(),
        ))]);
        let executor = ExecutorClient::new(felt(0xabc));
        let r = simulate_execute(
            &provider,
            &executor,
            felt(1),
            U256::from(0u64),
            &[],
            U256::from(100u64),
        )
        .await;
        assert!(matches!(r, Err(ChainError::Reverted(_))));
    }

    #[tokio::test]
    async fn simulate_malformed_return_rejected() {
        // execute() must return 2 felts. 1 felt should be rejected.
        let provider = MockProvider::new(vec![Ok(vec![felt(123)])]);
        let executor = ExecutorClient::new(felt(0xabc));
        let r = simulate_execute(
            &provider,
            &executor,
            felt(1),
            U256::from(0u64),
            &[],
            U256::from(100u64),
        )
        .await;
        assert!(matches!(r, Err(ChainError::Encoding(_))));
    }

    #[test]
    fn divergence_zero_expected_zero_realized_is_zero() {
        assert_eq!(compute_divergence_bps(U256::zero(), U256::zero()), 0);
    }

    #[test]
    fn divergence_zero_expected_nonzero_realized_is_max() {
        assert_eq!(
            compute_divergence_bps(U256::from(1u64), U256::zero()),
            u32::MAX
        );
    }

    #[test]
    fn divergence_realized_greater_than_expected_still_flags() {
        // Bot got 200% of expected → also a 100% (10_000 bps) divergence.
        let d = compute_divergence_bps(U256::from(200u64), U256::from(100u64));
        assert_eq!(d, 10_000);
    }
}
