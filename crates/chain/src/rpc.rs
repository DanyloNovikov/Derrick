//! `starknet-rs`-backed implementation of [`crate::Provider`].
//!
//! Wraps `JsonRpcClient<HttpTransport>` and maps our minimal `ProviderCall` /
//! `BlockTarget` types to starknet-rs's `FunctionCall` / `BlockId`. Errors
//! flatten into [`ChainError::Rpc`] — the underlying provider error message
//! is preserved as text so the operator can grep for it.

use std::sync::Arc;

use async_trait::async_trait;
use primitive_types::U256;
use starknet::core::types::{
    BlockId, BlockTag, ExecutionResult, FunctionCall, ReceiptBlock, TransactionReceipt,
    TransactionReceiptWithBlockInfo,
};
use starknet::providers::jsonrpc::HttpTransport;
use starknet::providers::{JsonRpcClient, Provider as StarknetProvider};
use starknet_types_core::felt::Felt;
use url::Url;

use crate::error::ChainError;
use crate::provider::{BlockTarget, EventLog, Provider, ProviderCall, TxStatus};

/// Production Provider — backed by `starknet-rs` over JSON-RPC HTTP.
///
/// Cheap to clone (the underlying client is behind an `Arc`).
#[derive(Clone)]
pub struct RpcProvider {
    inner: Arc<JsonRpcClient<HttpTransport>>,
    rpc_url: Url,
}

impl RpcProvider {
    /// Build a provider for the given RPC URL. Fails fast if the URL doesn't
    /// parse — production code should validate this at startup, not on the
    /// first request.
    pub fn new(rpc_url: &str) -> Result<Self, ChainError> {
        let url = Url::parse(rpc_url)
            .map_err(|e| ChainError::Rpc(format!("invalid RPC URL '{rpc_url}': {e}")))?;
        let inner = Arc::new(JsonRpcClient::new(HttpTransport::new(url.clone())));
        Ok(Self {
            inner,
            rpc_url: url,
        })
    }

    pub fn rpc_url(&self) -> &Url {
        &self.rpc_url
    }
}

impl std::fmt::Debug for RpcProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcProvider")
            .field("rpc_url", &self.rpc_url.as_str())
            .finish_non_exhaustive()
    }
}

fn to_block_id(t: BlockTarget) -> BlockId {
    match t {
        BlockTarget::Latest => BlockId::Tag(BlockTag::Latest),
        // Starknet JSON-RPC v0.8+ renamed the "pending" tag to "pre_confirmed".
        // Our public `BlockTarget::Pending` keeps the historical name; map it
        // to the on-the-wire spelling here.
        BlockTarget::Pending => BlockId::Tag(BlockTag::PreConfirmed),
    }
}

#[async_trait]
impl Provider for RpcProvider {
    async fn call(&self, call: ProviderCall, block: BlockTarget) -> Result<Vec<Felt>, ChainError> {
        let request = FunctionCall {
            contract_address: call.to,
            entry_point_selector: call.selector,
            calldata: call.calldata,
        };
        self.inner
            .call(request, to_block_id(block))
            .await
            .map_err(|e| ChainError::Rpc(e.to_string()))
    }

    async fn get_nonce(&self, account: Felt, block: BlockTarget) -> Result<Felt, ChainError> {
        self.inner
            .get_nonce(to_block_id(block), account)
            .await
            .map_err(|e| ChainError::Rpc(e.to_string()))
    }

    async fn get_tx_status(&self, tx_hash: Felt) -> Result<TxStatus, ChainError> {
        match self.inner.get_transaction_receipt(tx_hash).await {
            Ok(rwb) => Ok(receipt_to_status(rwb)),
            Err(e) => {
                let s = e.to_string();
                // Hash not yet seen — treat as a transient poll-again signal,
                // not a hard failure. The exact error string varies by node;
                // match conservatively on both spellings.
                if s.contains("TransactionHashNotFound") || s.contains("Transaction hash not found")
                {
                    Ok(TxStatus::NotFound)
                } else {
                    Err(ChainError::Rpc(s))
                }
            }
        }
    }
}

fn receipt_to_status(rwb: TransactionReceiptWithBlockInfo) -> TxStatus {
    // Treat any non-`Block` variant (Pending / PreConfirmed / future-spec)
    // as still-in-flight. Robust to enum-variant renames between starknet
    // protocol versions.
    if !matches!(rwb.block, ReceiptBlock::Block { .. }) {
        return TxStatus::Pending;
    }
    // Only Invoke receipts are relevant for our bot's submissions.
    let TransactionReceipt::Invoke(inv) = rwb.receipt else {
        return TxStatus::Pending;
    };
    let actual_fee = felt_to_u256(inv.actual_fee.amount);
    let events = inv
        .events
        .into_iter()
        .map(|e| EventLog {
            from_address: e.from_address,
            keys: e.keys,
            data: e.data,
        })
        .collect();
    match inv.execution_result {
        ExecutionResult::Succeeded => TxStatus::Succeeded { actual_fee, events },
        ExecutionResult::Reverted { reason } => TxStatus::Reverted { reason, actual_fee },
    }
}

fn felt_to_u256(f: Felt) -> U256 {
    let bytes = f.to_bytes_be();
    U256::from_big_endian(&bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn rejects_garbage_url() {
        let r = RpcProvider::new("not a url at all");
        assert!(r.is_err());
    }

    #[test]
    fn accepts_well_formed_https_url() {
        let r = RpcProvider::new("https://starknet-mainnet.public.blastapi.io").unwrap();
        assert_eq!(r.rpc_url().scheme(), "https");
    }

    #[test]
    fn accepts_local_http_url() {
        let r = RpcProvider::new("http://localhost:9545").unwrap();
        assert_eq!(r.rpc_url().host_str(), Some("localhost"));
        assert_eq!(r.rpc_url().port(), Some(9545));
    }

    #[test]
    fn debug_includes_rpc_url() {
        let r = RpcProvider::new("https://example.com/rpc").unwrap();
        let s = format!("{r:?}");
        assert!(s.contains("example.com"), "debug output: {s}");
    }
}
