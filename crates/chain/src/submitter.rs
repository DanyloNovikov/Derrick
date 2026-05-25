//! Signed invoke-transaction submitter.
//!
//! Wraps `starknet-rs`'s `SingleOwnerAccount` so the bot can build calldata
//! via [`ExecutorClient::build_execute_calldata`] and submit a single signed
//! `invoke_v3` to the `ArbExecutor` contract.
//!
//! Private keys never leave the submitter struct. The constructor takes a
//! `Felt` (parsed from the `OPERATOR_PRIVATE_KEY` env var upstream); the
//! struct never serializes itself, and `Debug` redacts the key.

use primitive_types::U256;
use starknet::accounts::{Account, ExecutionEncoding, SingleOwnerAccount};
use starknet::providers::jsonrpc::HttpTransport;
use starknet::providers::JsonRpcClient;
use starknet::signers::{LocalWallet, SigningKey};
use starknet_types_core::felt::Felt;
use url::Url;

use crate::error::ChainError;
use crate::executor::{ExecutorCall, ExecutorClient};
use crate::selectors::EXECUTE_SELECTOR;

/// Submits signed `invoke_v3` transactions to the `ArbExecutor` contract.
///
/// One submitter binds one operator key + one executor contract. Multiple
/// submitters can coexist if you want to spread load across operator
/// accounts; nothing here serializes their state.
pub struct ExecutorSubmitter {
    client: ExecutorClient,
    account: SingleOwnerAccount<JsonRpcClient<HttpTransport>, LocalWallet>,
}

impl ExecutorSubmitter {
    /// Build a submitter from raw fields.
    ///
    /// `operator_private_key` is consumed once and lives only inside the
    /// signer thereafter. Caller is expected to read it from the
    /// `OPERATOR_PRIVATE_KEY` env var and parse to `Felt::from_hex`.
    pub fn new(
        rpc_url: &str,
        operator_address: Felt,
        operator_private_key: Felt,
        executor_address: Felt,
        chain_id: Felt,
    ) -> Result<Self, ChainError> {
        let url = Url::parse(rpc_url)
            .map_err(|e| ChainError::Rpc(format!("invalid RPC URL '{rpc_url}': {e}")))?;
        let provider = JsonRpcClient::new(HttpTransport::new(url));
        let signer =
            LocalWallet::from_signing_key(SigningKey::from_secret_scalar(operator_private_key));
        let account = SingleOwnerAccount::new(
            provider,
            signer,
            operator_address,
            chain_id,
            ExecutionEncoding::New,
        );
        Ok(Self {
            client: ExecutorClient::new(executor_address),
            account,
        })
    }

    pub fn executor_address(&self) -> Felt {
        self.client.executor_address()
    }

    pub fn operator_address(&self) -> Felt {
        self.account.address()
    }

    /// Reference to the internal `ExecutorClient` for code paths that need to
    /// build calldata without going through the submitter (e.g., the
    /// pre-submit simulator).
    pub fn client(&self) -> &ExecutorClient {
        &self.client
    }

    /// Build calldata for `ArbExecutor::execute`, sign + submit as
    /// `invoke_v3`. Returns the transaction hash. Does NOT wait for inclusion.
    ///
    /// The bot's pipeline should follow this with a simulation pre-check
    /// (`crate::simulator::simulate_execute`) and a post-submit watcher on
    /// the tx hash before recording success/revert in the ledger.
    pub async fn submit(
        &self,
        token_in: Felt,
        min_profit: U256,
        calls: &[ExecutorCall],
    ) -> Result<Felt, ChainError> {
        let calldata = ExecutorClient::build_execute_calldata(token_in, min_profit, calls)?;
        let inner = starknet::core::types::Call {
            to: self.client.executor_address(),
            selector: EXECUTE_SELECTOR,
            calldata,
        };
        let result = self
            .account
            .execute_v3(vec![inner])
            .send()
            .await
            .map_err(|e| ChainError::Rpc(format!("submit invoke_v3 failed: {e}")))?;
        Ok(result.transaction_hash)
    }
}

impl std::fmt::Debug for ExecutorSubmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the signer/private key. Surface only public-safe addresses.
        f.debug_struct("ExecutorSubmitter")
            .field(
                "executor",
                &format_args!("{:#x}", self.client.executor_address()),
            )
            .field("operator", &format_args!("{:#x}", self.account.address()))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn dummy_key() -> Felt {
        // Arbitrary non-zero felt; we never sign anything in tests.
        Felt::from_hex("0x1234567890abcdef").unwrap()
    }

    #[test]
    fn construction_succeeds_with_valid_inputs() {
        let s = ExecutorSubmitter::new(
            "https://example.com/rpc",
            Felt::from(0xaaaa_u64),
            dummy_key(),
            Felt::from(0xbbbb_u64),
            Felt::from_hex("0x534e5f4d41494e").unwrap(), // SN_MAIN
        )
        .unwrap();
        assert_eq!(s.executor_address(), Felt::from(0xbbbb_u64));
        assert_eq!(s.operator_address(), Felt::from(0xaaaa_u64));
    }

    #[test]
    fn construction_rejects_invalid_url() {
        let r = ExecutorSubmitter::new(
            "not a url",
            Felt::from(0xaaaa_u64),
            dummy_key(),
            Felt::from(0xbbbb_u64),
            Felt::from(0u64),
        );
        assert!(r.is_err());
    }

    #[test]
    fn debug_redacts_signer_and_shows_addresses() {
        let s = ExecutorSubmitter::new(
            "https://example.com/rpc",
            Felt::from(0xaaaa_u64),
            dummy_key(),
            Felt::from(0xbbbb_u64),
            Felt::from(0u64),
        )
        .unwrap();
        let debug = format!("{s:?}");
        assert!(debug.contains("ExecutorSubmitter"));
        assert!(
            debug.contains("0xaaaa"),
            "operator address should appear: {debug}"
        );
        assert!(
            debug.contains("0xbbbb"),
            "executor address should appear: {debug}"
        );
        // The signing key value should NOT appear in debug output.
        assert!(
            !debug.contains("1234567890abcdef"),
            "private key must be redacted: {debug}"
        );
    }
}
