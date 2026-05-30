//! Admin client: reads + signed writes against `DerrickExecutor`.
//!
//! Reads go through `chain::RpcProvider` (no signing). Writes use
//! `starknet-rs`'s `SingleOwnerAccount` built from `OWNER_PRIVATE_KEY`.
//! The private key is loaded lazily — read-only commands work without it.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chain::{BlockTarget, Provider, ProviderCall, RpcProvider, TxStatus};
use primitive_types::U256;
use starknet::accounts::{Account, ExecutionEncoding, SingleOwnerAccount};
use starknet::core::types::Call;
use starknet::macros::selector;
use starknet::providers::jsonrpc::HttpTransport;
use starknet::providers::JsonRpcClient;
use starknet::signers::{LocalWallet, SigningKey};
use starknet_types_core::felt::Felt;
use url::Url;

/// Selectors not in `chain::selectors` (those are only the bot's hot path).
pub mod sel {
    use starknet::macros::selector;
    use starknet_types_core::felt::Felt;

    pub const ALLOW_TARGET: Felt = selector!("allow_target");
    pub const DISALLOW_TARGET: Felt = selector!("disallow_target");
    pub const IS_TARGET_ALLOWED: Felt = selector!("is_target_allowed");

    pub const TRANSFER_OWNERSHIP: Felt = selector!("transfer_ownership");
    pub const OWNER: Felt = selector!("owner");

    pub const WITHDRAW: Felt = selector!("withdraw");
}

/// Polling defaults for `wait_for_inclusion`.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_TIMEOUT: Duration = Duration::from_secs(180);

/// Read-side: configured at construction; never holds a key.
pub struct ReadClient {
    provider: RpcProvider,
    executor: Felt,
}

impl ReadClient {
    pub fn new(rpc_url: &str, executor: Felt) -> Result<Self> {
        let provider = RpcProvider::new(rpc_url).map_err(|e| anyhow!("rpc provider: {e}"))?;
        Ok(Self { provider, executor })
    }

    pub async fn owner(&self) -> Result<Felt> {
        let res = self
            .provider
            .call(
                ProviderCall {
                    to: self.executor,
                    selector: sel::OWNER,
                    calldata: vec![],
                },
                BlockTarget::Latest,
            )
            .await?;
        res.first()
            .copied()
            .ok_or_else(|| anyhow!("owner(): empty response"))
    }

    pub async fn is_target_allowed(&self, target: Felt, selector: Felt) -> Result<bool> {
        let res = self
            .provider
            .call(
                ProviderCall {
                    to: self.executor,
                    selector: sel::IS_TARGET_ALLOWED,
                    calldata: vec![target, selector],
                },
                BlockTarget::Latest,
            )
            .await?;
        Ok(res.first().is_some_and(|f| *f != Felt::ZERO))
    }

    /// ERC20 `balance_of(account)` against an arbitrary token contract.
    pub async fn balance_of(&self, token: Felt, account: Felt) -> Result<U256> {
        let res = self
            .provider
            .call(
                ProviderCall {
                    to: token,
                    selector: selector!("balance_of"),
                    calldata: vec![account],
                },
                BlockTarget::Latest,
            )
            .await?;
        if res.len() < 2 {
            bail!("balance_of: expected 2 felts (low, high), got {}", res.len());
        }
        Ok(felts_to_u256(res[0], res[1]))
    }
}

/// Write-side: needs the Oracle private key. Constructed via `from_env`.
pub struct WriteClient {
    account: SingleOwnerAccount<JsonRpcClient<HttpTransport>, LocalWallet>,
    executor: Felt,
    provider: RpcProvider,
}

impl WriteClient {
    /// Build a write client from explicit pieces. `private_key` is consumed
    /// once and lives only inside the signer.
    pub fn new(
        rpc_url: &str,
        owner_address: Felt,
        private_key: Felt,
        executor: Felt,
        chain_id: Felt,
    ) -> Result<Self> {
        let url = Url::parse(rpc_url).map_err(|e| anyhow!("invalid RPC URL '{rpc_url}': {e}"))?;
        let provider_inner = JsonRpcClient::new(HttpTransport::new(url));
        let signer = LocalWallet::from_signing_key(SigningKey::from_secret_scalar(private_key));
        let account = SingleOwnerAccount::new(
            provider_inner,
            signer,
            owner_address,
            chain_id,
            ExecutionEncoding::New,
        );
        let provider = RpcProvider::new(rpc_url).map_err(|e| anyhow!("rpc provider: {e}"))?;
        Ok(Self {
            account,
            executor,
            provider,
        })
    }

    /// Convenience: read `OWNER_PRIVATE_KEY` from env. Returns a descriptive
    /// error when the variable is absent or not a valid hex felt.
    pub fn from_env(
        rpc_url: &str,
        owner_address: Felt,
        executor: Felt,
        chain_id: Felt,
    ) -> Result<Self> {
        let raw = std::env::var("OWNER_PRIVATE_KEY")
            .context("OWNER_PRIVATE_KEY must be set for write commands")?;
        let pk = Felt::from_hex(raw.trim())
            .map_err(|_| anyhow!("OWNER_PRIVATE_KEY is not a valid hex felt"))?;
        Self::new(rpc_url, owner_address, pk, executor, chain_id)
    }

    /// Submit one `invoke_v3` call to `executor`. Does NOT wait for inclusion.
    async fn invoke_one(&self, selector: Felt, calldata: Vec<Felt>) -> Result<Felt> {
        let call = Call {
            to: self.executor,
            selector,
            calldata,
        };
        let res = self
            .account
            .execute_v3(vec![call])
            .send()
            .await
            .map_err(|e| anyhow!("invoke_v3 failed: {e}"))?;
        Ok(res.transaction_hash)
    }

    /// Submit N `invoke_v3` calls in a single tx. Atomic — any reverts roll
    /// back the whole batch. Used by `setup` for batched whitelist.
    pub async fn invoke_many(&self, calls: Vec<(Felt, Vec<Felt>)>) -> Result<Felt> {
        if calls.is_empty() {
            bail!("invoke_many: empty calls list");
        }
        let inner: Vec<Call> = calls
            .into_iter()
            .map(|(selector, calldata)| Call {
                to: self.executor,
                selector,
                calldata,
            })
            .collect();
        let res = self
            .account
            .execute_v3(inner)
            .send()
            .await
            .map_err(|e| anyhow!("invoke_v3 (batch) failed: {e}"))?;
        Ok(res.transaction_hash)
    }

    // ── high-level operations ────────────────────────────────────────

    pub async fn allow_target(&self, target: Felt, selector: Felt) -> Result<Felt> {
        self.invoke_one(sel::ALLOW_TARGET, vec![target, selector])
            .await
    }

    pub async fn disallow_target(&self, target: Felt, selector: Felt) -> Result<Felt> {
        self.invoke_one(sel::DISALLOW_TARGET, vec![target, selector])
            .await
    }

    pub async fn transfer_ownership(&self, new_owner: Felt) -> Result<Felt> {
        self.invoke_one(sel::TRANSFER_OWNERSHIP, vec![new_owner])
            .await
    }

    pub async fn withdraw(&self, token: Felt, to: Felt, amount: U256) -> Result<Felt> {
        let (lo, hi) = u256_split(amount);
        self.invoke_one(sel::WITHDRAW, vec![token, to, Felt::from(lo), Felt::from(hi)])
            .await
    }

    /// Poll `tx_hash` until included or `POLL_TIMEOUT` elapses.
    pub async fn wait_for_inclusion(&self, tx_hash: Felt) -> Result<TxStatus> {
        let start = std::time::Instant::now();
        loop {
            match self.provider.get_tx_status(tx_hash).await? {
                TxStatus::Pending | TxStatus::NotFound => {
                    if start.elapsed() > POLL_TIMEOUT {
                        bail!(
                            "timeout waiting for tx {tx_hash:#x} after {POLL_TIMEOUT:?}"
                        );
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                terminal => return Ok(terminal),
            }
        }
    }
}

fn u256_split(n: U256) -> (u128, u128) {
    let low = n.low_u128();
    let high = (n >> 128).low_u128();
    (low, high)
}

fn felts_to_u256(lo: Felt, hi: Felt) -> U256 {
    let lo_bytes = lo.to_bytes_be();
    let hi_bytes = hi.to_bytes_be();
    // u128 lives in the lower 16 bytes of each felt's 32-byte BE repr.
    let mut buf = [0u8; 32];
    buf[16..].copy_from_slice(&lo_bytes[16..]);
    let low = U256::from_big_endian(&buf);
    let mut buf = [0u8; 32];
    buf[16..].copy_from_slice(&hi_bytes[16..]);
    let high = U256::from_big_endian(&buf);
    (high << 128) | low
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn u256_split_roundtrips() {
        let n = (U256::from(0xdead_beef_u64) << 128) | U256::from(0x1234_5678_u64);
        let (lo, hi) = u256_split(n);
        assert_eq!(lo, 0x1234_5678);
        assert_eq!(hi, 0xdead_beef);
    }

    #[test]
    fn felts_to_u256_reconstructs() {
        let lo = Felt::from(0x1234_5678_u64);
        let hi = Felt::from(0xdead_beef_u64);
        let n = felts_to_u256(lo, hi);
        assert_eq!(n, (U256::from(0xdead_beef_u64) << 128) | U256::from(0x1234_5678_u64));
    }

    #[test]
    fn read_client_constructs_with_valid_inputs() {
        let r = ReadClient::new("https://example.com/rpc", Felt::from(0xbbbb_u64));
        assert!(r.is_ok());
    }

    #[test]
    fn write_client_rejects_garbage_url() {
        let r = WriteClient::new(
            "not a url",
            Felt::from(1u64),
            Felt::from(2u64),
            Felt::from(3u64),
            Felt::from(0u64),
        );
        assert!(r.is_err());
    }
}
