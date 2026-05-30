# derrick

> Atomic arbitrage bot for Starknet — Rust pipeline + Cairo executor contract.

`derrick` spots price gaps across Starknet DEXes on STARK pairs and captures the
spread in a single atomic transaction, backed by a Cairo contract
(`DerrickExecutor`) that reverts unless the trade nets a minimum profit.

This README covers two things:

1. [Deploying the `DerrickExecutor` contract](#1-deploy-the-contract)
2. [Operating the contract with the admin CLI (`derrick-admin`)](#2-admin-cli)

---

## Prerequisites

| Tool | Used for | Check |
|---|---|---|
| [Scarb](https://docs.swmansion.com/scarb/) 2.16+ | build the Cairo contract | `scarb --version` |
| [Starknet Foundry](https://foundry-rs.github.io/starknet-foundry/) (`sncast`, `snforge`) 0.57+ | declare/deploy + tests | `sncast --version` |
| [Rust](https://rustup.rs/) (stable) | build the admin CLI | `cargo --version` |
| A Starknet **RPC endpoint** | talk to the network | see below |
| A deployed **Oracle wallet** (Argent/Braavos/OZ) | owns the contract, signs txs | funded with STRK/ETH for gas |

**RPC endpoint.** Use a provider (e.g. Alchemy, Blast, GetBlock) or your own
node. The endpoint must speak the JSON-RPC spec the client expects — currently
**v0.8** (`starknet-rs 0.17`). Example Alchemy URL:
`https://starknet-mainnet.g.alchemy.com/starknet/version/rpc/v0_8/<KEY>`.

> Newer is not better here: the latest `starknet-rs` (0.17) speaks spec 0.8, so
> pick the `v0_8` endpoint path even if the node also serves 0.10.

---

## 1. Deploy the contract

The contract is a deliberately "dumb" executor: it runs an owner-supplied list
of calls atomically and reverts unless the chosen token's balance grew by at
least `min_profit`. The constructor takes a single argument — `owner` — which
must be the Oracle wallet that will sign trades.

### 1.1 Build the artifacts

```bash
cd contracts/executor
scarb build
```

Produces `target/dev/derrick_executor_DerrickExecutor.contract_class.json`.

### 1.2 Import the Oracle wallet into sncast

Load the private key from env so it never lands in shell history:

```bash
set -a; source .env; set +a          # provides OWNER_PRIVATE_KEY

sncast account import \
  --name oracle \
  --address 0x<ORACLE_ADDRESS> \
  --private-key "$OWNER_PRIVATE_KEY" \
  --type braavos                      # or argent / oz — match your wallet
```

The Oracle wallet must already be a **deployed** account on the target network
and hold STRK/ETH for gas.

### 1.3 Declare (publish the class)

```bash
sncast --account oracle \
  declare --network mainnet \
  --contract-name DerrickExecutor
```

Returns a **class hash**. If it says the class is already declared, reuse that
hash — declaration is one-time per code version.

> Test on Sepolia first: swap `--network mainnet` for `--network sepolia`.

### 1.4 Deploy (create the instance)

`<owner>` is the Oracle wallet address — the same account that signs.

```bash
sncast --account oracle \
  deploy --network mainnet \
  --class-hash 0x<CLASS_HASH> \
  --arguments '0x<ORACLE_ADDRESS>'
```

Returns the **contract address** — this is your `DerrickExecutor` instance.

> ⚠️ `transfer_ownership` is **single-step and irreversible**. A wrong `owner`
> bricks the contract. Double-check the address before deploying.

### 1.5 Record the address

Put the results in `.env`:

```bash
DERRICK__EXECUTOR__CONTRACT_ADDRESS=0x<CONTRACT_ADDRESS>
DERRICK__EXECUTOR__OWNER_ACCOUNT_ADDRESS=0x<ORACLE_ADDRESS>
DERRICK__NETWORK__RPC_URL=https://<your-rpc>/v0_8/<KEY>
DERRICK__NETWORK__CHAIN_ID=SN_MAIN
OWNER_PRIVATE_KEY=0x<...>             # secret — keep .env out of git
```

### 1.6 Verify the deploy

A quick on-chain read of `owner()` (no build needed):

```bash
URL="$DERRICK__NETWORK__RPC_URL"
curl -s -X POST "$URL" -H 'Content-Type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"starknet_call",
  "params":[{
    "contract_address":"0x<CONTRACT_ADDRESS>",
    "entry_point_selector":"0x2016836a56b71f0d02689e69e326f4f4c1b9057164ef592671cf0d37c8040c0",
    "calldata":[]
  },"latest"]
}'
```

The returned value should equal your Oracle address. Or just use the admin CLI
(`derrick-admin status`, below).

---

## 2. Admin CLI

`derrick-admin` reads and modifies on-chain contract state: inspect the owner
and balances, manage the call whitelist, withdraw funds, transfer ownership, and
batch-whitelist everything from the config.

### 2.1 Build

```bash
cargo build -p admin-cli --release
# binary: target/release/derrick-admin
```

### 2.2 Set up your shell (once per session)

```bash
set -a; source .env; set +a           # loads OWNER_PRIVATE_KEY + addresses + RPC
alias da="./target/release/derrick-admin"
```

Contract address, owner, RPC, and chain id are all read from env — you don't
pass them on every command. Read commands work **without** a key; write commands
require `OWNER_PRIVATE_KEY`.

### 2.3 Read commands (no key, safe)

```bash
# Owner (on-chain vs configured) + balances of every token in config
da status

# Is a (target, selector) pair whitelisted?  selector = name or raw 0x...
da is-allowed --target 0x<POOL>  --selector swap
da is-allowed --target 0x<TOKEN> --selector transfer
```

### 2.4 Write commands (need `OWNER_PRIVATE_KEY`, cost gas)

Each signs with the Oracle key, waits for inclusion, and prints `actual_fee`
(or the revert reason — note a reverted tx still costs gas).

```bash
# Whitelist / un-whitelist one (target, selector)
da allow-target    --target 0x<POOL>  --selector swap
da allow-target    --target 0x<TOKEN> --selector approve
da disallow-target --target 0x<POOL>  --selector swap

# Withdraw tokens from the contract.
#   --token : a config symbol (STARK, USDC) or a raw 0x address
#   --amount: RAW units — 1 USDC = 1000000, not 1.0
da withdraw --token STARK --to 0x<DEST> --amount 1000000000000000000

# Change owner — irreversible, requires explicit confirmation flag
da transfer-ownership --new-owner 0x<NEW> --yes-i-mean-it
```

### 2.5 `setup` — batch whitelist from config

Whitelists `transfer` on every `[[tokens]]` and `swap` on every `[[pools]]` in
the config, as a single atomic transaction.

```bash
da setup --dry-run     # print the planned allow_target calls, send nothing
da setup               # send (prompts for confirmation)
da setup --yes         # send without the prompt
```

If the config has no tokens/pools, it prints `Nothing to do`.

### 2.6 Command reference

| Command | Key? | Description |
|---|---|---|
| `status` | no | owner + token balances on the contract |
| `is-allowed` | no | check one `(target, selector)` |
| `allow-target` | yes | whitelist a pair |
| `disallow-target` | yes | remove a pair |
| `withdraw` | yes | move tokens out of the contract |
| `transfer-ownership` | yes | hand over ownership (single-step) |
| `setup` | yes | batch-whitelist tokens + pools from config |

### 2.7 Global flags

| Flag | Effect |
|---|---|
| `--no-wait` | send the tx but don't block on inclusion |
| `--rpc-url <URL>` | override the RPC for this run (e.g. point at Sepolia) |
| `--executor <0x>` | override the contract address |
| `--owner <0x>` | override the owner account address |
| `--chain-id <ID>` | `SN_MAIN` / `SN_SEPOLIA` / raw hex |
| `--config <path>` | use a different config file |

### 2.8 Typical first-run flow

```bash
set -a; source .env; set +a
da status                                       # 1. owner matches? contract live?
# (fill [[tokens]] / [[pools]] in config/default.toml)
da setup --dry-run                              # 2. preview the whitelist
da setup                                        # 3. apply it
da is-allowed --target 0x<POOL> --selector swap # 4. confirm it took
da status                                       # 5. check balances
```

---

## Contract internals & safety

The full security model, profit-invariant boundaries, gas accounting, and known
limitations are documented at the top of
[`contracts/executor/src/lib.cairo`](contracts/executor/src/lib.cairo). Key
points for operators:

- The profit assertion is denominated **only in `token_in`** and holds **only
  within the transaction** — keep only the trading token on the contract at
  rest, and prefer exact-amount approvals (not infinite) in trade bundles.
- `min_profit` is **gross of gas**; the Oracle account pays fees separately, and
  reverted txs still cost gas. Size `min_profit` to cover gas plus a margin.
- Only the owner can call `execute`/`withdraw`/admin functions. To halt trading,
  stop the bot — there is no on-chain pause.

## Running the tests

```bash
cd contracts/executor && snforge test     # Cairo contract (32 tests)
cargo test                                # Rust crates
```
