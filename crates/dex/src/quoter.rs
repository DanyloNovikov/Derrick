use std::sync::Arc;

use async_trait::async_trait;
use domain::{PoolId, QuoteError, TokenId, U256};

/// On-chain quoting bridge for DEX adapters.
///
/// Adapters that need to call the pool contract (e.g., for CL pools whose
/// tick state isn't loaded, or for final verification before submission)
/// depend on an implementation of this trait. `chain` provides the
/// production implementation backed by a Starknet provider; tests mock it.
#[async_trait]
pub trait OnChainQuoter: Send + Sync {
    async fn quote_in(
        &self,
        pool: PoolId,
        token_in: TokenId,
        amount_in: U256,
    ) -> Result<U256, QuoteError>;
}

/// Reference-counted boxed [`OnChainQuoter`], used in adapter struct fields.
pub type SharedQuoter = Arc<dyn OnChainQuoter>;

/// No-op quoter used when on-chain quoting isn't configured. CPMM adapters
/// only need on-chain quoting as a fallback for final verification; the
/// `quote_in_local` path keeps working off cached reserves from Sync events.
#[derive(Debug, Default)]
pub struct NoopQuoter;

#[async_trait]
impl OnChainQuoter for NoopQuoter {
    async fn quote_in(
        &self,
        _pool: PoolId,
        _token_in: TokenId,
        _amount_in: U256,
    ) -> Result<U256, QuoteError> {
        Err(QuoteError::OnChain("on-chain quoter not configured".into()))
    }
}
