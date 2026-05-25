# derrick

> Atomic arbitrage bot for Starknet — Rust pipeline + Cairo executor contract.

**Status:** early development. The skeleton is in place (workspace, types, JediSwap v1 adapter, spatial-2DEX strategy, risk gating, ledger, Cairo executor, full event→detect→submit→inclusion pipeline). Production deployment requires real pool addresses, an operator key, and a deployed executor contract — none of which are committed.

---

## Table of contents

- [Overview (EN)](#overview)
- [Architecture](#architecture)
- [Workspace layout](#workspace-layout)
- [Crate reference](#crate-reference)
- [Cairo executor contract](#cairo-executor-contract)
- [Pipeline & data flow](#pipeline--data-flow)
- [Configuration](#configuration)
- [Profit math](#profit-math)
- [Risk model](#risk-model)
- [Development setup](#development-setup)
- [Testing](#testing)
- [Observability](#observability)
- [Roadmap & known gaps](#roadmap--known-gaps)
- [Документация на русском](#документация-на-русском)

---

## Overview

**derrick** monitors prices across multiple Starknet DEXes, detects 2-DEX spatial arbitrage cycles, sizes each trade with ternary search, and executes them as a single atomic multicall via a Cairo contract that asserts `final_balance >= initial_balance + min_profit` on-chain.

**Target DEXes** (planned coverage, current implementation marked with ✅):

| DEX | Kind | Status |
|---|---|---|
| JediSwap v1 | CPMM (Uniswap v2 fork) | ✅ adapter, calldata builder, event decoder |
| JediSwap v2 | Concentrated liquidity | ⏳ planned |
| MySwap v1/v2 | CPMM / CL | ⏳ planned |
| 10kSwap | CPMM | ⏳ planned |
| SithSwap | Solidly stable + volatile | ⏳ planned |
| Ekubo | Concentrated, singleton | ⏳ planned (priority — gas-efficient multi-hop) |
| Haiko | Range MM | ⏳ planned |
| Avnu / Fibrous | Aggregators | ⏳ fallback / price reference only |

**Hard invariants:**

- Profit math runs in `U256` end-to-end — `f64` is banned in hot paths (`float_arithmetic = "warn"` at workspace level).
- Token decimals are tracked at the type level via `Amount<TokenId>`; mixing tokens is a compile-time error.
- Every trade clears `NetProfit > 0` after gas and safety margin. See [Profit math](#profit-math).
- The on-chain Cairo contract re-asserts profit; the bot's quote is a recommendation, not a promise.

---

## Architecture

```
                ┌──────────────┐
                │  Starknet    │
                │   (RPC+WS)   │
                └───┬──────────┘
                    │ ws events
                    ▼
   ┌────────────────────────────────────────────────────────────┐
   │                       bot (binary)                         │
   │                                                            │
   │   ┌─────────┐   ┌─────────────┐   ┌──────────┐   ┌──────┐  │
   │   │ Watcher │──▶│State Updater│──▶│ Detector │──▶│Incl. │  │
   │   └─────────┘   └─────────────┘   └────┬─────┘   └──┬───┘  │
   │        (chain)        (registry)       │            │      │
   │                                        ▼            │      │
   │                              ┌────────────────┐     │      │
   │                              │ Risk Manager   │     │      │
   │                              └────────┬───────┘     │      │
   │                                       ▼             │      │
   │                              ┌────────────────┐     │      │
   │                              │ Sim (chain)    │     │      │
   │                              └────────┬───────┘     │      │
   │                                       ▼             │      │
   │                              ┌────────────────┐     │      │
   │                              │ Submit (chain) │─────┘      │
   │                              └────────┬───────┘            │
   └───────────────────────────────────────┼────────────────────┘
                                           │
                                           ▼
                                  ┌────────────────┐
                                  │ Ledger (PG)    │
                                  │   attempts     │
                                  └────────────────┘
```

Tasks communicate via bounded `tokio::sync::mpsc` channels. Each task owns a `ShutdownToken` and uses `select!` so `Ctrl+C` cooperatively drains the pipeline.

**Channel capacities** ([crates/bot/src/pipeline.rs:44-46](crates/bot/src/pipeline.rs#L44-L46)):

- `POOL_EVENT_CAPACITY = 1024` (watcher → state_updater)
- `POOL_UPDATE_CAPACITY = 1024` (state_updater → detector)
- `INCLUSION_CAPACITY = 128` (detector → inclusion)

---

## Workspace layout

```
derrick/
├── Cargo.toml              # Workspace root: members, shared deps, lints, profiles
├── rust-toolchain.toml     # Pinned to Rust 1.94
├── Dockerfile.dev          # Dev image: Rust 1.94 + Scarb 2.16.0
├── docker-compose.yml      # postgres + dev container
├── Makefile                # `make check`, `make test`, `make clippy`, `make psql`, …
├── config/
│   └── default.toml        # All runtime settings; env-overridable via DERRICK__*
├── contracts/
│   └── executor/           # Cairo executor contract (Scarb project)
│       ├── src/lib.cairo
│       └── tests/test_executor.cairo
└── crates/
    ├── domain/             # Core types (Amount, Token, Pool trait, Path, Opportunity)
    ├── math/               # Pure CPMM arithmetic (Uniswap v2 formulas, U512 intermediates)
    ├── dex/                # DEX adapters implementing `Pool` (currently JediSwap v1)
    ├── strategy/           # Spatial detector, ternary-search sizer, profit evaluator
    ├── chain/              # Starknet RPC, WS watcher, simulator, submitter, executor encoder
    ├── risk/               # Whitelist, position limits, circuit breaker, daily loss cap
    ├── ledger/             # Postgres persistence (`attempts` table + migrations)
    └── bot/                # Binary: wiring, config, pipeline, registry, inclusion watcher
```

**Workspace lints** ([Cargo.toml:81-99](Cargo.toml#L81-L99)):

- `unsafe_code = forbid` — no `unsafe` anywhere in the workspace.
- `unwrap_used = deny`, `panic = deny` — production code must surface errors.
- `expect_used = warn`, `float_arithmetic = warn` — flagged in CI.
- Clippy `pedantic` group on by default.

---

## Crate reference

### `domain` — I/O-free core types

The bedrock crate. No async, no I/O, no Starknet dependencies beyond `starknet-types-core::Felt`. Everything else builds on top.

| Item | File | Purpose |
|---|---|---|
| `Amount` | [crates/domain/src/amount.rs](crates/domain/src/amount.rs) | `U256` value tagged with its `TokenId`; mixing tokens is rejected at the type level |
| `SignedAmount` | [crates/domain/src/amount.rs](crates/domain/src/amount.rs) | Signed counterpart for P&L (`gross`, `net`) |
| `TokenId`, `ContractAddress` | [crates/domain/src/token.rs](crates/domain/src/token.rs) | Newtype around `Felt`; ERC-20 contract address |
| `Token`, `Symbol`, `Decimals` | [crates/domain/src/token.rs](crates/domain/src/token.rs) | Validated symbol (1-16 ASCII alnum/`-`/`_`), `u8` decimals wrapper |
| `DexKind` | [crates/domain/src/pool.rs](crates/domain/src/pool.rs) | Enum of all supported DEX families |
| `FeeBps` | [crates/domain/src/pool.rs](crates/domain/src/pool.rs) | bps wrapper; `.ppm()` converts to parts-per-million for math |
| `PoolId`, `PoolMeta` | [crates/domain/src/pool.rs](crates/domain/src/pool.rs) | `token0`/`token1` follow **on-chain order**, not lexicographic |
| `PoolEvent`, `EventMeta`, `PoolEventKind` | [crates/domain/src/pool.rs](crates/domain/src/pool.rs) | Raw event from WS watcher with `(block, tx_index, event_index)` ordering key |
| `Quote` | [crates/domain/src/quote.rs](crates/domain/src/quote.rs) | `amount_in`, `amount_out`, `gas_estimate`, `state_version` |
| `Hop`, `Path` | [crates/domain/src/opportunity.rs](crates/domain/src/opportunity.rs) | `Path::new` validates: non-empty, hop chaining, no self-loop |
| `Opportunity` | [crates/domain/src/opportunity.rs](crates/domain/src/opportunity.rs) | Pre-sized arb candidate: `Uuid` + `Path` + `detected_at_ms` |
| `Pool` trait | [crates/domain/src/traits.rs](crates/domain/src/traits.rs) | The contract every DEX adapter implements |

The `Pool` trait:

```rust
#[async_trait]
pub trait Pool: Send + Sync {
    fn meta(&self) -> &PoolMeta;
    fn state_version(&self) -> u64;
    fn quote_in_local(&self, token_in: TokenId, amount_in: Amount) -> Result<Quote, QuoteError>;
    fn quote_out_local(&self, token_out: TokenId, amount_out: Amount) -> Result<Quote, QuoteError>;
    async fn quote_in_onchain(&self, …) -> Result<Quote, QuoteError>;
    fn apply_event(&mut self, event: &PoolEvent) -> Result<u64, StateError>;
}
```

`quote_in_local` is the hot path used by the sizer (no network, just U256 arithmetic against cached reserves).

---

### `math` — Pure CPMM arithmetic

Canonical Uniswap v2 formulas. `U512` is used for intermediate products to avoid overflow; the result is checked back down to `U256`.

```rust
pub fn cpmm_quote_out(reserve_in: U256, reserve_out: U256, amount_in: U256, fee_ppm: u32)
    -> Result<U256, MathError>;
pub fn cpmm_quote_in(reserve_in: U256, reserve_out: U256, amount_out: U256, fee_ppm: u32)
    -> Result<U256, MathError>;
```

- Fees expressed in **parts-per-million** (`FEE_DENOM = 1_000_000`), so 0.30% = `3_000` ppm.
- `cpmm_quote_in` adds the unconditional `+1` floor of Uniswap v2 `getAmountIn` to keep the K-invariant satisfied.
- `MathError`: `ZeroReserves`, `ZeroInput`, `InvalidFee`, `InsufficientLiquidity`, `Overflow`.

Property-based tests (`proptest`) verify monotonicity and roundtrip identity against pathological reserve sizes.

---

### `dex` — Adapter implementations

The factory pattern maps a `PoolMeta` to a concrete `BoxedPool = Box<dyn Pool + Send + Sync>`:

```rust
pub fn build_pool(meta: PoolMeta, quoter: SharedQuoter) -> Option<BoxedPool>;
```

Returns `None` for any `DexKind` not yet implemented.

| Adapter | File | State |
|---|---|---|
| JediSwap v1 | [crates/dex/src/jediswap_v1.rs](crates/dex/src/jediswap_v1.rs) | ✅ full: state from `Sync` events, CPMM quoting, on-chain quote delegation |
| All others | — | `factory` returns `None` |

**JediSwap v1 details:**

- Only `Sync` events mutate state (canonical post-swap reserves). `Swap`/`Mint`/`Burn` are decoded but ignored — the next `Sync` carries authoritative values.
- Event ordering is enforced by `EventMeta.ordering_key()`; out-of-order events yield `StateError::OutOfOrder`, exact duplicates yield `StateError::Duplicate`.
- Cairo `u256` is two felts (`low: u128`, `high: u128`); decoder in `decode_sync` reassembles `U256 = (high << 128) | low`.

**`OnChainQuoter` trait** lives here ([crates/dex/src/quoter.rs](crates/dex/src/quoter.rs)) so adapters can delegate to a real chain-level quoter without depending on the `chain` crate. Default `NoopQuoter` returns `QuoteError::LocalUnavailable`.

---

### `strategy` — Detection, sizing, profit

Three pure functions form the optimization core:

```rust
pub fn detect_spatial_opportunities(
    updated_pool: PoolId,
    pools: &[(&PoolMeta, &dyn Pool)],
    params: &SpatialParams,
) -> Vec<SizedTrade>;

pub fn find_optimal_input(
    path: &Path, pools: &[&dyn Pool], params: &ProfitParams,
    min_in: Amount, max_in: Amount, iterations: u32,
) -> Result<SizedTrade, SizerError>;

pub fn evaluate_path(
    path: &Path, amount_in: Amount,
    pools: &[&dyn Pool], params: &ProfitParams,
) -> Result<PathOutcome, EvalError>;
```

**Spatial detector** ([crates/strategy/src/spatial.rs](crates/strategy/src/spatial.rs)):
event-driven. When a pool updates, looks up all other pools sharing the same token pair, then tries both cycle directions (`start → other → start`). O(N) in pools-per-pair, not in global pools.

**Sizer** ([crates/strategy/src/sizer.rs](crates/strategy/src/sizer.rs)):
ternary search over `[min_in, max_in]`. The profit curve is unimodal (small inputs lose to gas, large inputs lose to price impact). The implementation keeps a *globally best* candidate across all iterations rather than returning the final midpoint — this matters because integer division means the final window may not contain the optimum.

**Profit** ([crates/strategy/src/profit.rs](crates/strategy/src/profit.rs)):
walks each hop, threading `amount_out → amount_in`, then computes:

```
gross         = amount_out_final − amount_in_initial
safety_margin = max(2 × gas_cost,  amount_in × safety_margin_bps / 10_000)
net           = gross − gas_cost − safety_margin
```

Returns `PathOutcome` with `hop_quotes` and `state_versions` for replayability.

---

### `chain` — Starknet I/O

| Module | Purpose |
|---|---|
| `provider.rs` | `Provider` trait (`call`, `get_nonce`, `get_tx_status`), `ProviderCall`, `BlockTarget`, `TxStatus`, `EventLog` |
| `rpc.rs` | `RpcProvider` — production `starknet-rs` JSON-RPC client |
| `watcher.rs` | `WsWatcher` — WebSocket event subscription with exponential backoff reconnect |
| `simulator.rs` | `simulate_execute` — calls `execute` on `Pending` block, asserts realized vs expected divergence ≤ 500 bps |
| `submitter.rs` | `ExecutorSubmitter` — `SingleOwnerAccount` wraps the operator key, signs and sends `invoke_v3` |
| `executor.rs` | `ExecutorClient::build_execute_calldata` — serializes `(token_in, min_profit_u256, calls)` into `Vec<Felt>` |
| `selectors.rs` | Constants: `EXECUTE_SELECTOR`, ERC-20 selectors, `SWAP_SELECTOR`, `EXECUTED_EVENT_SELECTOR` |
| `error.rs` | `ChainError`: `Rpc`, `Reverted`, `Encoding`, `ModelDivergence`, `InvalidFelt` |

**Reconnect strategy:** `WsWatcher` deliberately does *not* catch up missed events after a disconnect. Pool state is rebuilt from the next `Sync` — for v2-style pools this is always authoritative.

**Simulation guard:** `MAX_DIVERGENCE_BPS = 500` (5%). If the on-chain `Pending` simulation diverges from the local quote by more than 5%, the trade is rejected as `ModelDivergence`.

---

### `risk` — Pre-trade gating + post-trade accounting

Synchronous (`Mutex<State>`), no I/O. Two methods:

```rust
pub fn evaluate(&self, p: &TradeProposal) -> Result<(), RiskRejection>;
pub fn record(&self, token: TokenId, outcome: TradeOutcome);
```

**`evaluate` checks in order:**

1. `token_in` is in the whitelist → else `NotWhitelisted`
2. Per-token limits exist → else `NoLimitsConfigured` (strict-mode, fail-closed)
3. `amount_in ≤ max_position` → else `PositionTooLarge`
4. `expected_profit > 0` → else `NonPositiveExpectedProfit`
5. `expected_profit ≥ min_profit` → else `ProfitBelowThreshold`
6. Circuit breaker not active → else `CircuitBreakerActive`
7. `daily_loss < daily_max_loss` → else `DailyLossExceeded`

**`record` outcomes:**

- `Executed { positive_profit }` → reset `consecutive_failures` to 0
- `Executed { negative_profit }`, `Reverted { gas_paid }`, `SkippedSimulation` → `consecutive_failures += 1`, add to `daily_loss`
- When `consecutive_failures ≥ max_consecutive_failures` → set `paused_until_ms = now + pause_seconds`
- Daily loss window auto-resets every 24h

**`Clock` trait** ([crates/risk/src/clock.rs](crates/risk/src/clock.rs)) abstracts time so tests use a deterministic `FakeClock`; production uses `SystemClock`.

---

### `ledger` — Postgres persistence

One row per trade attempt in `attempts`, updated as it moves through the pipeline.

**Schema** ([crates/ledger/migrations/0001_attempts.sql](crates/ledger/migrations/0001_attempts.sql)):

```sql
CREATE TABLE attempts (
    id              UUID        PRIMARY KEY,
    detected_at     TIMESTAMPTZ NOT NULL,
    completed_at    TIMESTAMPTZ,
    status          TEXT        NOT NULL,
    token_in_addr   TEXT        NOT NULL,
    amount_in       TEXT        NOT NULL,    -- U256 as decimal string
    expected_profit TEXT,
    realized_profit TEXT,
    gas_paid        TEXT,
    reason          TEXT,
    path            JSONB       NOT NULL,
    tx_hash         TEXT
);
```

Indexes on `(detected_at DESC)`, `(status)`, `(token_in_addr)`.

**`AttemptStatus`** progresses through: `Detected → Sized → RiskRejected | SimulationFailed | Submitted → Executed | Reverted | PaperTraded`.

`update_attempt_status` uses `COALESCE` so partial updates (just `tx_hash`, just `realized_profit`) don't clobber other fields.

> **Known TODO:** U256 stored as TEXT decimal. Migrating to `NUMERIC(78,0)` requires a custom `sqlx::Encode`/`Decode` for `primitive_types::U256` — deferred.

---

### `bot` — Binary, wiring, runtime

The orchestrator. Translates TOML into runtime types, spawns the pipeline, owns shutdown.

| Module | File | Purpose |
|---|---|---|
| `main` | [crates/bot/src/main.rs](crates/bot/src/main.rs) | CLI (`--config`), startup sequence, signal handling |
| `lib` | [crates/bot/src/lib.rs](crates/bot/src/lib.rs) | Re-exports for integration tests |
| `config` | [crates/bot/src/config.rs](crates/bot/src/config.rs) | TOML schema; env override via `DERRICK__SECTION__FIELD` |
| `wiring` | [crates/bot/src/wiring.rs](crates/bot/src/wiring.rs) | Parse pools/tokens/spatial-params; build `WatcherConfig`, `ExecutorSubmitter`, `RiskConfig`; log + skip on invalid entries |
| `registry` | [crates/bot/src/registry.rs](crates/bot/src/registry.rs) | `PoolRegistry`: per-pool `RwLock`, pair index for fast lookup |
| `pipeline` | [crates/bot/src/pipeline.rs](crates/bot/src/pipeline.rs) | Spawns 4 tasks: watcher, state_updater, detector, inclusion |
| `calls` | [crates/bot/src/calls.rs](crates/bot/src/calls.rs) | `build_path_calls` — turns a `SizedTrade` into `Vec<ExecutorCall>` |
| `inclusion` | [crates/bot/src/inclusion.rs](crates/bot/src/inclusion.rs) | Polls submitted tx hashes to terminal state; closes ledger + risk record |
| `observability` | [crates/bot/src/observability.rs](crates/bot/src/observability.rs) | JSON `tracing` + Prometheus exporter |
| `shutdown` | [crates/bot/src/shutdown.rs](crates/bot/src/shutdown.rs) | `tokio::watch`-based cooperative shutdown signal |

**No-op gating:** missing WS URL, no spatial config, no operator key, placeholder executor address — each independently downgrades its task to a no-op so the bot still starts and logs *what* is wired and what isn't. Useful for paper-trading and shadow runs.

---

## Cairo executor contract

[contracts/executor/src/lib.cairo](contracts/executor/src/lib.cairo) — atomic multicall with on-chain profit assertion.

**`IArbExecutor` interface:**

```cairo
fn execute(token_in: ContractAddress, min_profit: u256, calls: Array<Call>) -> u256;

fn pause(); fn unpause(); fn is_paused() -> bool;
fn add_operator(op: ContractAddress); fn remove_operator(op);
fn is_operator(addr) -> bool;
fn allow_target(target, selector: felt252); fn disallow_target(target, selector);
fn is_target_allowed(target, selector) -> bool;
fn transfer_ownership(new_owner); fn owner() -> ContractAddress;
fn withdraw(token, to, amount: u256);
```

**`execute` flow:**

1. `enter_nonreentrant()` — flag-based guard, reverts if re-entered.
2. `assert_not_paused()` and `assert_operator(caller)`.
3. For each `Call` in `calls`: assert `(target, selector)` is in the whitelist (fail-fast).
4. Snapshot `initial = IERC20(token_in).balance_of(self)`.
5. For each call: `call_contract_syscall(c.to, c.selector, c.calldata).unwrap_syscall()`. Any inner revert reverts the entire tx (atomicity).
6. Snapshot `final = IERC20(token_in).balance_of(self)`.
7. **Assert** `final >= initial + min_profit` — else revert `'INSUFFICIENT_PROFIT'`.
8. Emit `Executed { operator, token_in, profit, num_calls }`.
9. `exit_nonreentrant()`, return `profit`.

**Storage:**

```cairo
owner:             ContractAddress
paused:            bool
reentrancy_locked: bool
operators:         Map<ContractAddress, bool>
allowed_targets:   Map<(ContractAddress, felt252), bool>
```

**Tests** ([contracts/executor/tests/test_executor.cairo](contracts/executor/tests/test_executor.cairo)) cover:

- `happy_path_returns_profit` — mocks `balance_of` (0 → 1000), asserts profit == 1000
- `insufficient_profit_reverts` — balance unchanged, expects `'INSUFFICIENT_PROFIT'`
- `non_operator_cannot_execute` — expects `'ONLY_OPERATOR'`
- `unallowed_target_reverts` — expects `'TARGET_NOT_ALLOWED'`

> **snforge note:** `Scarb.toml` has `snforge_std` commented out — the dev image doesn't yet ship `snforge`. Tests are written and will run once it's installed; for now `make cairo-build` only compiles the contract.

---

## Pipeline & data flow

Four tasks, wired by `mpsc` channels:

1. **`watcher`** — `WsWatcher::run()` subscribes to `Sync/Swap/Mint/Burn` events on registered pools, decodes JSON-RPC notifications, sends `PoolEvent` downstream. Idles if no `WatcherConfig` was built. Exponential backoff on disconnect.

2. **`state_updater`** — for each `PoolEvent`: takes a `read_lock` to locate the pool, a `write_lock` to apply the event, increments `state_version`, emits `PoolStateUpdate { pool, state_version }`. On error (`OutOfOrder`, `Duplicate`, `Malformed`) logs at `warn` and continues; bumps `derrick_apply_event_errors_total`.

3. **`detector`** — on each `PoolStateUpdate`:
   - If `spatial: None` → log passive mode, continue.
   - Else snapshot all pools sharing the updated pool's pair, run `detect_spatial_opportunities`.
   - For each `SizedTrade`: insert `attempts` row (`Detected → Sized`), call `risk.evaluate`. If rejected → `RiskRejected`, record outcome.
   - If accepted *and* provider+submitter configured: build calldata, run `simulate_execute`, then either submit (real) or mark `PaperTraded` (paper-trading on).
   - On submit: send `PendingTx` to inclusion channel.

4. **`inclusion`** — for each `PendingTx`, spawn a polling task: every 5s call `provider.get_tx_status(hash)` until terminal (or 5min timeout). On success: parse `Executed` event, update ledger, record `TradeOutcome::Executed`. On revert: update ledger, record `Reverted { gas_paid }`. Timeout → `SimulationFailed` + `SkippedSimulation`. Stub variant drains the channel when no provider is configured.

All tasks `select!` on a `ShutdownToken`; `Ctrl+C` triggers `Shutdown::broadcast()` which fans out to every subscriber.

---

## Configuration

`config/default.toml` is the source of truth. All fields are overridable by environment variables using the prefix `DERRICK__` and `__` as separator (e.g. `DERRICK__DATABASE__URL=postgres://...`).

**Sections:**

| Section | Keys | Notes |
|---|---|---|
| `[network]` | `rpc_url`, `ws_url`, `chain_id` | Pathfinder/Juno endpoint and chain selector |
| `[database]` | `url` | Postgres connection string |
| `[observability]` | `log_level`, `metrics_bind` | Tracing filter + Prometheus bind addr |
| `[executor]` | `contract_address`, `operator_account_address`, `chain_id`, `paper_trading` | Executor contract + operator account; `paper_trading = true` runs sim but suppresses submit |
| `[risk]` | `max_consecutive_failures`, `circuit_breaker_pause_seconds` | Defaults: 5 failures, 600s pause |
| `[strategy]` | `safety_margin_bps`, `sizer_iterations` | Defaults: 30 bps, 40 iters |
| `[[tokens]]` | `symbol`, `address`, `decimals`, `risk = { max_position, min_profit, daily_max_loss }` | Repeated. Tokens without `risk` are whitelisted but rejected on evaluate (fail-closed) |
| `[[pools]]` | `dex`, `address`, `token0`, `token1`, `fee_bps` | Repeated. `token0`/`token1` must follow on-chain order |
| `[spatial]` | `start_token`, `gas_cost`, `safety_margin_bps`, `min_amount_in`, `max_amount_in`, `sizer_iterations` | Omit to keep detector in passive mode |

**Secrets — never in config, always in env:**

```
OPERATOR_PRIVATE_KEY=<hex>             # signing key for invoke_v3
DATABASE_URL=postgres://…              # optional override (also in [database].url)
```

`.env.example` documents the minimum required environment.

---

## Profit math

Every trade clears this filter (in `start_token` units):

```
gross_profit  = amount_out_final − amount_in_initial
safety_margin = max( 2 × gas_cost,  amount_in × safety_margin_bps / 10_000 )
net_profit    = gross_profit − gas_cost − safety_margin

Trade accepted iff:  net_profit > 0
```

**Why the `max(2×gas, bps)` shape:**

- The bps term scales with trade size — protects against price-impact misestimates.
- The `2×gas` floor protects small trades where bps would round to dust.
- `safety_margin_bps = 30` (0.30%) is the workspace default; tune per token in `[[tokens]].risk` if needed.

**Ternary search bounds** (`[spatial].min_amount_in / max_amount_in`):
- Too low: gas dominates and there's no positive `amount_in` to find.
- Too high: pool price impact swallows the spread.
- `sizer_iterations = 40` converges any unimodal range with millimeter precision in integer math; raise only if you observe missed optima.

**Decimals are tagged, not implicit.** Every `Amount` carries its `TokenId`; the sizer's `Ord` comparisons go through `cmp_same_token` which `expect()`s the tokens match. Any mixed-token comparison is a bug that surfaces immediately, not silently as a 10^12× off-by.

---

## Risk model

Three independent fail-closed layers:

**1. Static whitelists** (set at startup from config):
- Token whitelist — any token outside this set is rejected.
- DEX whitelist — implicit; only DEXes with adapters can quote.
- Cairo contract `allowed_targets` — only whitelisted `(contract, selector)` pairs can be called by `execute`.

**2. Dynamic limits** (per-token, hot config):
- `max_position` — hard cap on `amount_in`.
- `min_profit` — minimum expected profit; trades below threshold rejected.
- `daily_max_loss` — rolling 24h loss budget; when exceeded, all trades on this token blocked until reset.

**3. Circuit breaker** (global state):
- `max_consecutive_failures = 5` → pause all trades for `circuit_breaker_pause_seconds = 600` (10 min default).
- A successful trade with positive profit resets the counter.

**On-chain belt-and-suspenders:**
- `min_profit` is asserted *on-chain* in `execute` — if reality diverges from simulation, the tx reverts and we pay only gas.
- `ReentrancyGuard` (flag-based) blocks direct re-entrance.
- `Pausable` lets the owner kill execution without redeploy.

---

## Development setup

**Prerequisites:** Docker + Make. The dev container ships everything else (Rust 1.94, Scarb 2.16, rustfmt, clippy).

```bash
# One-time
make build         # build dev image
make up            # start postgres in background

# Daily
make shell         # bash inside dev container (cargo cache mounted)
make check         # cargo check --workspace --all-targets
make fmt           # cargo fmt --all
make clippy        # cargo clippy --workspace --all-targets -- -D warnings
make test          # cargo test --workspace
make psql          # open psql against the dev database

# Cairo
make cairo-build   # scarb build (compiles executor contract)
make cairo-clean   # scarb clean
make shell-cairo   # interactive shell in contracts/executor
```

**Without Docker:** `rust-toolchain.toml` pins Rust 1.94; install Scarb 2.16.0 manually if working on the Cairo contract. Postgres can be any local instance — point `DATABASE_URL` at it.

**Cargo workspace** (`Cargo.toml`):
- `resolver = "2"`
- Release profile: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`, `panic = "abort"`.
- Dev profile: `opt-level = 1`, `debug = true`.

---

## Testing

```bash
make test                                  # entire workspace
docker compose --profile dev run --rm dev \
    cargo test -p strategy spatial         # one crate/one module
```

**Coverage:**

- `math` — proptests for roundtrip + monotonicity + Uniswap v2 parity.
- `domain` — Path validation, Amount/SignedAmount arithmetic.
- `dex/jediswap_v1` — Sync decoding, event ordering, dedupe, local quoting parity.
- `strategy` — spatial detector (positive + negative cases), sizer (peak finding, empty range, token mismatch), profit (cycle invariants, safety margin shape).
- `risk` — every rejection variant, circuit breaker state machine with `FakeClock`.
- `ledger` — uses `Ledger::lazy()` for unit tests; integration tests require live Postgres.
- `chain/simulator` — divergence calculation.
- `bot/wiring` — config parsing edge cases (unknown DEX, missing token, dup address, chain ID variants).
- `bot/shutdown` — broadcast wakes all waiters, sender drop = shutdown.
- `contracts/executor` — 4 snforge tests (build path, profit assert, operator gate, target whitelist) ready to run once `snforge_std` is wired into the dev image.

---

## Observability

**Tracing** (JSON, on stdout):
- `RUST_LOG=info,derrick=debug` by default (override in `.env`).
- Spans include `target` and current-span context.

**Prometheus exporter** on `[observability].metrics_bind` (`0.0.0.0:9090` default). Key metrics:

| Metric | Type | Labels |
|---|---|---|
| `derrick_pool_events_total` | counter | `event_kind` |
| `derrick_apply_event_errors_total` | counter | — |
| `derrick_attempts_total` | counter | `status` (Sized, RiskRejected, SimulationFailed, Submitted, Executed, Reverted, PaperTraded) |
| `derrick_handle_update_duration_seconds` | histogram | — |
| `derrick_simulate_duration_seconds` | histogram | — |
| `derrick_submit_duration_seconds` | histogram | — |

Scrape into Prometheus/Grafana; alert on `attempts_total{status="Reverted"}` rate spikes or `simulate_duration_seconds` p99 regressions.

---

## Roadmap & known gaps

**Adapters (highest leverage):**
- Ekubo (concentrated, singleton) — gas-efficient multi-hop, priority for v0.2
- JediSwap v2, MySwap v2 (concentrated)
- 10kSwap, MySwap v1 (CPMM — mostly copy-paste from JediSwap v1)
- SithSwap (stable + volatile curves — different math)
- Haiko (range MM)

**Strategy:**
- Triangular arbitrage via Bellman-Ford over `-log(rate × (1 − fee))`
- Multi-hop multi-DEX (≤4 hops, DFS with pruning)
- Flash loans (zkLend / Nostra)
- CEX-DEX and cross-chain (longer-term)

**Infrastructure:**
- WebSocket catch-up after reconnect (currently relies on next `Sync`)
- `gas_paid` parsing from reverted tx receipts (currently recorded as 0)
- `NUMERIC(78,0)` Postgres encoding for `U256` (currently TEXT decimal)
- snforge integration in the dev image (tests written, not yet runnable)
- Devnet harness with mainnet fork (`starknet-devnet-rs`) for E2E rehearsals

**Hardening:**
- Detector-level rate limiter (risk has per-token circuit breaker; no global rate limit)
- Reentrancy test in Cairo (blocked by snforge cheatcode limits)
- Pre-flight nonce reservation to avoid mid-flight collisions on bursts

---

## License

UNLICENSED. Not published; not for redistribution.

---

# Документация на русском

> Атомарный арбитражный бот для Starknet — Rust-pipeline и Cairo-контракт executor'а.

**Статус:** ранняя стадия. Каркас собран (workspace, типы, адаптер JediSwap v1, spatial-2DEX стратегия, risk gating, ledger, Cairo executor, полный pipeline event→detect→submit→inclusion). Для боевого запуска нужны реальные адреса пулов, ключ оператора и задеплоенный executor-контракт — ничего из этого не закоммичено.

## Что такое derrick

**derrick** мониторит цены на нескольких DEX'ах Starknet, ищет 2-DEX spatial-арбитражные циклы, считает оптимальный размер сделки тернарным поиском и исполняет её атомарным multicall'ом через Cairo-контракт, который **на чейне** проверяет `final_balance >= initial_balance + min_profit`.

## Архитектура — кратко

```
WS-events → Watcher → State Updater → Detector → Risk → Sim → Submit → Inclusion
                          │              │       │      │      │         │
                          ▼              ▼       ▼      ▼      ▼         ▼
                       Registry      Strategy   Risk  chain  chain    Ledger (PG)
```

Все задачи — `tokio::spawn`, общаются через bounded `mpsc`, выключаются кооперативно через `ShutdownToken` (broadcast по `Ctrl+C`).

## Crate'ы — для чего каждый

| Crate | Назначение |
|---|---|
| **domain** | Базовые типы без I/O: `Amount<Token>`, `Pool` трейт, `Path`, `Opportunity`. Mixing разных токенов — ошибка типизации |
| **math** | Чистая CPMM-арифметика (Uniswap v2). `U512` для промежуточных, ошибки — `MathError` |
| **dex** | Адаптеры под `Pool` трейт. Сейчас реализован только **JediSwap v1**, остальные — `factory` возвращает `None` |
| **strategy** | `detect_spatial_opportunities` (event-driven, O(N) по пулам общей пары), `find_optimal_input` (тернарный поиск с глобально-лучшим кандидатом), `evaluate_path` (профит + safety margin) |
| **chain** | Слой Starknet: RPC-провайдер, WebSocket watcher с экспоненциальным backoff'ом, симулятор (`Pending` block, divergence ≤ 500 bps), submitter, encoder calldata для executor'а |
| **risk** | Synchronous gate: whitelist → лимиты → циркуит-брейкер → дневной лосс-кап. `evaluate` перед сделкой, `record` после |
| **ledger** | Postgres. Одна строка на попытку в `attempts`, обновляется по мере прохождения pipeline'а. Статусы: `Detected → Sized → RiskRejected | SimulationFailed | Submitted → Executed | Reverted | PaperTraded` |
| **bot** | Бинарь: парсинг конфига, DI, registry, pipeline (4 задачи), inclusion-watcher, observability, shutdown |

## Cairo executor — суть

`contracts/executor/src/lib.cairo`. Функция `execute(token_in, min_profit, calls)`:

1. ReentrancyGuard вход + pause-check + operator-check.
2. Каждый `Call` сверяется с whitelist'ом `(target, selector)` — fail-fast.
3. Снимок `balance_of(self)` ДО.
4. Прогоняет все calls; любой revert внутри = revert всей tx (атомарность).
5. Снимок `balance_of(self)` ПОСЛЕ.
6. **`assert(final >= initial + min_profit)`** — если симуляция врала, контракт ревертит сам.
7. Emit `Executed { operator, token_in, profit, num_calls }`.

Owner может: `pause/unpause`, `add_operator/remove_operator`, `allow_target/disallow_target`, `transfer_ownership`, `withdraw`.

Тесты на snforge (happy path, insufficient profit, не-оператор, не-whitelisted target) написаны, но `snforge_std` пока закомментирован в `Scarb.toml` — `make cairo-build` сейчас только компилирует контракт.

## Pipeline — что куда течёт

1. **Watcher** — `WsWatcher` подписан на `Sync/Swap/Mint/Burn` зарегистрированных пулов, шлёт `PoolEvent` дальше. Если `WatcherConfig` не построен — idle. После disconnect'а **не догоняет** пропущенные события: для v2-пулов следующий `Sync` всё равно canonical.

2. **State Updater** — берёт read-lock на registry, write-lock на конкретный пул, применяет событие, увеличивает `state_version`, отправляет `PoolStateUpdate`. На `OutOfOrder/Duplicate/Malformed` — `warn` + продолжение.

3. **Detector** — для каждого `PoolStateUpdate`: смотрит все пулы той же пары, прогоняет `detect_spatial_opportunities`, для каждого `SizedTrade` пишет в `attempts` (`Detected → Sized`), зовёт `risk.evaluate`. Если ок и есть provider+submitter — `simulate_execute` → submit или `PaperTraded`.

4. **Inclusion** — для каждого `PendingTx` поллит `get_tx_status` каждые 5с до 5мин таймаута. На успех парсит `Executed` event и пишет реальный профит. На revert — записывает gas_paid. На таймаут — `SimulationFailed`.

## Профит-математика

```
gross         = amount_out_final − amount_in_initial
safety_margin = max( 2 × gas_cost,  amount_in × safety_margin_bps / 10_000 )
net           = gross − gas_cost − safety_margin

Сделка принимается ⇔  net > 0
```

Дефолт `safety_margin_bps = 30` (0.30%). Floor `2×gas` защищает мелкие сделки, где bps был бы пылью. Тернарный поиск (`sizer_iterations = 40`) на интегерах сходится до миллиметра в любом unimodal-диапазоне.

**Decimals — это тип, а не комментарий.** `Amount` несёт `TokenId`; сравнения через `cmp_same_token` `expect()`-ят совпадение токенов. Любой mismatch — это баг, который вылезает сразу, а не как 10^12× ошибка в P&L.

## Risk-модель

**Три fail-closed уровня:**

1. **Статические whitelists** — токены, DEX'и (по факту наличия адаптера), `allowed_targets` в Cairo-контракте.
2. **Динамические лимиты per-token** — `max_position`, `min_profit`, `daily_max_loss` (24h rolling).
3. **Circuit breaker** — `max_consecutive_failures` подряд → пауза на `circuit_breaker_pause_seconds`. Успешная позитивная сделка сбрасывает счётчик.

**On-chain страховка:** `min_profit` проверяется **в контракте**. Если реальность разошлась с симуляцией, tx ревертит, и мы платим только газ.

## Конфиг

Источник истины — `config/default.toml`. Любое поле перекрывается env-переменной `DERRICK__<SECTION>__<FIELD>`. Секреты — **только в env**:

```
OPERATOR_PRIVATE_KEY=<hex>
DATABASE_URL=postgres://…
STARKNET_RPC_URL=…
STARKNET_WS_URL=…
EXECUTOR_CONTRACT_ADDRESS=…
OPERATOR_ACCOUNT_ADDRESS=…
```

`paper_trading = true` в `[executor]` запускает симуляцию, но подавляет submit — статус привычно пишется как `PaperTraded`. Удобно для shadow-run перед запуском.

## Разработка

```bash
make build         # билд dev-образа (Rust 1.94 + Scarb 2.16)
make up            # запустить postgres
make shell         # bash в dev-контейнере
make check         # cargo check --workspace
make clippy        # cargo clippy -- -D warnings
make test          # cargo test --workspace
make psql          # psql в derrick базу
make cairo-build   # scarb build executor'а
```

`unsafe_code = forbid` на всём workspace'е. `unwrap_used = deny`, `panic = deny`. Все `Result`'ы вверх — паника в hotpath'е валит pipeline.

## Observability

Логи — JSON через `tracing` (фильтр `RUST_LOG`). Метрики — Prometheus экспортёр на `0.0.0.0:9090`. Считаются: `pool_events_total{event_kind}`, `apply_event_errors_total`, `attempts_total{status}`, гистограммы длительности `handle_update / simulate / submit`.

Алерты, которые стоит навесить:
- Резкий рост `attempts_total{status="Reverted"}` — модель разошлась с чейном.
- Регрессия p99 `simulate_duration_seconds` — лагает RPC.
- `apply_event_errors_total` > 0 за окно — broken event decoder или гонка.

## Что НЕ реализовано (приоритет сверху)

1. **Ekubo адаптер** — concentrated, singleton, газ-эффективный multi-hop.
2. **CPMM-адаптеры** — 10kSwap, MySwap v1 (copy-paste от JediSwap v1).
3. **Stable + CL адаптеры** — SithSwap (stable), JediSwap v2 / MySwap v2 / Haiko (CL — нужна tick-симуляция).
4. **Триангулярный арбитраж** — Bellman-Ford на `-log(rate × (1-fee))`.
5. **Multi-hop multi-DEX** — ≤4 хопа, DFS с pruning.
6. **Flash loans** — zkLend / Nostra.
7. **WS catch-up после reconnect** — сейчас опираемся на следующий `Sync`.
8. **Gas из reverted-tx** — пишется 0, нужен парсер receipt'ов.
9. **`NUMERIC(78,0)` в Postgres** — сейчас TEXT-decimal, нужен кастомный `sqlx::Encode/Decode` для `U256`.
10. **snforge в dev-образе** — тесты Cairo готовы, нужен бинарь.

## Лицензия

UNLICENSED — не публикуется, не для перераспространения.
