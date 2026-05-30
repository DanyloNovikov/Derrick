// SPDX-License-Identifier: UNLICENSED
//! # derrick executor — atomic arbitrage multicall with on-chain profit assertion
//!
//! A deliberately "dumb" execution layer for the off-chain arbitrage bot. The
//! contract knows nothing about DEXes, swaps, or AMM math. It executes an
//! owner-supplied list of arbitrary calls atomically and refuses to settle
//! unless the balance of a chosen token grew by at least `min_profit`.
//!
//! All protocol-specific logic (which pools, which selectors, how to encode a
//! swap's calldata, how to size a trade) lives off-chain in the Rust bot. This
//! contract is the on-chain backstop that guarantees a submitted bundle either
//! makes money or reverts.
//!
//! ------------------------------------------------------------------------
//! ## Execution flow — `execute(token_in, min_profit, calls)`
//! ------------------------------------------------------------------------
//!   1. Take the reentrancy lock, then assert the caller is the owner.
//!   2. Validate every `(call.to, call.selector)` against the whitelist.
//!      Calldata is NOT inspected (see "Security boundaries" below).
//!   3. Snapshot `initial = token_in.balance_of(self)`.
//!   4. Execute each `call` in order via `call_contract_syscall`. Any call that
//!      reverts bubbles up and reverts the entire transaction.
//!   5. Snapshot `final = token_in.balance_of(self)` and assert
//!      `final >= initial + min_profit`. Otherwise revert `INSUFFICIENT_PROFIT`.
//!   6. Return the realized profit `final - initial`.
//!
//! A typical bundle for a STARK -> X -> STARK cycle looks like:
//!   [ approve(routerA, amount_in) on STARK,
//!     swap(...) on routerA,            // STARK -> X
//!     approve(routerB, amount_mid) on X,
//!     swap(...) on routerB ]           // X -> STARK
//! with `token_in = STARK`. The four calls settle atomically; if the round trip
//! does not net at least `min_profit` STARK, nothing happens.
//!
//! ------------------------------------------------------------------------
//! ## Trust model
//! ------------------------------------------------------------------------
//! There is exactly one privileged role: the `owner` (the bot's Oracle wallet).
//! `execute`, `withdraw`, the whitelist setters, and `transfer_ownership` are
//! all owner-only. The contract assumes the owner constructs honest bundles;
//! the whitelist and the profit assertion exist to bound the blast radius of a
//! buggy bundle, NOT to defend against a malicious owner. If the owner key is
//! compromised, the attacker can simply call `withdraw` — no on-chain control
//! here changes that. Protect the key.
//!
//! ------------------------------------------------------------------------
//! ## What the profit assertion does and does NOT guarantee
//! ------------------------------------------------------------------------
//! The invariant `final >= initial + min_profit` is precise and narrow:
//!
//!   * It is denominated ONLY in `token_in`. The balances of any OTHER token
//!     the contract may hold are never checked. A bundle that grows `token_in`
//!     while draining a different token would still pass. => Keep only the
//!     trading token (e.g. STARK) on this contract at rest; do not park USDC/
//!     ETH/USDT here between trades. Sweep dust out with `withdraw`.
//!
//!   * It holds ONLY within the transaction. It says nothing about state that
//!     outlives the tx — most importantly ERC20 *allowances*. Any `approve`
//!     left standing after `execute` is a hole the invariant cannot see:
//!     a later-compromised spender could `transferFrom` the contract outside
//!     any profit check. => Approve the EXACT amount each leg consumes (an
//!     exact-in swap's `transferFrom` drives the allowance back to zero by
//!     itself, so no residual approval remains and no extra reset call is
//!     needed). Avoid infinite (u256::MAX) approvals.
//!
//!   * It is GROSS of gas. Transaction fees are paid by the owner *account*
//!     that submits the tx (in STRK/ETH), from a balance separate from the
//!     trading capital on this contract. Gas is never subtracted from the
//!     measured profit. Since the profit token (STARK) and the gas token
//!     (STRK) are the same asset, gas directly erodes net P&L. => The bot must
//!     set `min_profit` high enough to cover expected gas plus a margin.
//!
//! ------------------------------------------------------------------------
//! ## Gas & failed attempts (operational)
//! ------------------------------------------------------------------------
//! On Starknet a reverted transaction is still included in a block and STILL
//! charges a fee. A bundle that reverts on `INSUFFICIENT_PROFIT` (e.g. because
//! the pool state moved after the bot decided to send) costs gas and yields
//! zero profit — a pure loss to the owner account. Therefore the bot MUST
//! simulate `execute` (via `starknet_call`) against fresh state and only submit
//! when the simulated net profit exceeds gas plus margin. `min_profit` is the
//! on-chain safety net for the race between simulation and inclusion, not a
//! substitute for simulation: simulation decides "is it worth sending", while
//! `min_profit` guarantees "what we send cannot lose token_in on-chain".
//!
//! ------------------------------------------------------------------------
//! ## Security boundaries — read before extending the whitelist
//! ------------------------------------------------------------------------
//!   * The whitelist authorizes a `(target, selector)` PAIR, not the call's
//!     arguments. A whitelisted method may be invoked with any calldata. The
//!     profit assertion is what neutralizes hostile calldata *for token_in*,
//!     subject to the two caveats above. Consequently: do NOT whitelist
//!     generic value-moving methods (`transfer`, `transferFrom`, and ideally
//!     `approve`) on token contracts unless strictly required, and prefer
//!     whitelisting concrete DEX-router methods.
//!
//! ------------------------------------------------------------------------
//! ## Halting & observability
//! ------------------------------------------------------------------------
//!   * No on-chain pause. Because only the owner can call `execute`, halting
//!     trading just means stopping the bot process: no signer, no tx. The
//!     escape hatch `withdraw` remains available regardless.
//!   * No events are emitted. The bot reconstructs realized profit from the
//!     ERC20 `Transfer` events already present in the transaction receipt.
//!     Note this means admin actions (whitelist edits, withdrawals, ownership
//!     changes) are not independently logged on-chain; audit them via traces.
//!
//! ------------------------------------------------------------------------
//! ## Known limitations (intentional, documented for reviewers)
//! ------------------------------------------------------------------------
//!   * `transfer_ownership` is single-step. A mistyped `new_owner` permanently
//!     bricks every owner-only function, including `withdraw`. Double-check the
//!     address; a two-step (propose/accept) handover would harden this.
//!   * The reentrancy guard is defense-in-depth and largely redundant: any
//!     re-entry into `execute` mid-bundle would have `caller == the DEX`, which
//!     `assert_owner` already rejects. It cannot get stuck "locked" because a
//!     Cairo revert rolls back the storage write.

use starknet::ContractAddress;
use starknet::account::Call;

/// External interface of the arbitrage executor. Every method is owner-gated
/// except the two read-only views (`is_target_allowed`, `owner`).
#[starknet::interface]
pub trait IArbExecutor<TContractState> {
    /// Execute `calls` atomically and assert the contract's `token_in` balance
    /// grew by at least `min_profit`.
    ///
    /// # Arguments
    /// * `token_in`   - the token whose balance delta defines "profit". For a
    ///                  STARK -> X -> STARK cycle this is STARK. Must be the
    ///                  asset the cycle starts and ends in.
    /// * `min_profit` - minimum required balance increase, in `token_in`'s
    ///                  smallest unit. GROSS of gas — set it to cover gas plus
    ///                  margin (see module docs). May be `0`.
    /// * `calls`      - ordered list of calls to perform (approves, swaps, ...).
    ///                  Every `(to, selector)` must be whitelisted; calldata is
    ///                  not validated.
    ///
    /// # Returns
    /// The realized profit `final - initial` in `token_in` units.
    ///
    /// # Access
    /// Owner only.
    ///
    /// # Panics
    /// * `REENTRANCY`          - re-entered while a prior `execute` is in flight.
    /// * `ONLY_OWNER`          - caller is not the owner.
    /// * `TARGET_NOT_ALLOWED`  - some `(to, selector)` is not whitelisted.
    /// * `INSUFFICIENT_PROFIT` - `final < initial + min_profit`.
    /// * any panic propagated from an inner call (reverts the whole tx).
    fn execute(
        ref self: TContractState,
        token_in: ContractAddress,
        min_profit: u256,
        calls: Array<Call>,
    ) -> u256;

    /// Whitelist a `(target, selector)` pair so `execute` may invoke it.
    /// Owner only. Panics `TARGET_ZERO` if `target` is the zero address.
    /// See module "Security boundaries" before whitelisting token methods.
    fn allow_target(ref self: TContractState, target: ContractAddress, selector: felt252);

    /// Remove a `(target, selector)` pair from the whitelist. Owner only.
    fn disallow_target(ref self: TContractState, target: ContractAddress, selector: felt252);

    /// View: whether `(target, selector)` is currently whitelisted.
    fn is_target_allowed(
        self: @TContractState, target: ContractAddress, selector: felt252,
    ) -> bool;

    /// Hand ownership to `new_owner`. Owner only. Single-step and irreversible:
    /// a wrong address bricks the contract (see module "Known limitations").
    /// Panics `OWNER_ZERO` if `new_owner` is the zero address.
    fn transfer_ownership(ref self: TContractState, new_owner: ContractAddress);

    /// View: the current owner.
    fn owner(self: @TContractState) -> ContractAddress;

    /// Escape hatch: transfer `amount` of `token` out of the contract to `to`.
    /// Owner only. Use it to recover trading capital or sweep non-`token_in`
    /// dust. Panics `TO_ZERO` on a zero recipient, `TRANSFER_FAILED` if the
    /// ERC20 `transfer` returns false.
    fn withdraw(
        ref self: TContractState,
        token: ContractAddress,
        to: ContractAddress,
        amount: u256,
    );
}

/// Minimal ERC20 surface this contract depends on: reading its own balance to
/// measure profit, and `transfer` for `withdraw`.
#[starknet::interface]
trait IERC20<TContractState> {
    fn balance_of(self: @TContractState, account: ContractAddress) -> u256;
    fn transfer(ref self: TContractState, recipient: ContractAddress, amount: u256) -> bool;
}

#[starknet::contract]
mod ArbExecutor {
    use core::num::traits::Zero;
    use starknet::storage::{
        Map,
        StorageMapReadAccess,
        StorageMapWriteAccess,
        StoragePointerReadAccess,
        StoragePointerWriteAccess,
    };
    use starknet::syscalls::call_contract_syscall;
    use starknet::{ContractAddress, get_caller_address, get_contract_address, SyscallResultTrait};
    use starknet::account::Call;
    use super::{IERC20Dispatcher, IERC20DispatcherTrait};

    #[storage]
    struct Storage {
        /// The single privileged role; see module "Trust model".
        owner: ContractAddress,
        /// Reentrancy flag; defense-in-depth, self-clearing on revert.
        reentrancy_locked: bool,
        /// Whitelist of callable `(target, selector)` pairs. Calldata is not
        /// part of the key — only the method identity is gated.
        allowed_targets: Map<(ContractAddress, felt252), bool>,
    }

    /// Set the initial owner at deploy time. Panics `OWNER_ZERO` on a zero
    /// owner, which would otherwise lock every privileged function forever.
    #[constructor]
    fn constructor(ref self: ContractState, owner: ContractAddress) {
        assert(!owner.is_zero(), 'OWNER_ZERO');
        self.owner.write(owner);
    }

    #[generate_trait]
    impl InternalImpl of InternalTrait {
        /// Revert `ONLY_OWNER` unless the caller is the stored owner.
        fn assert_owner(self: @ContractState) {
            assert(get_caller_address() == self.owner.read(), 'ONLY_OWNER');
        }

        /// Take the reentrancy lock. Reverts `REENTRANCY` if already held.
        fn enter_nonreentrant(ref self: ContractState) {
            assert(!self.reentrancy_locked.read(), 'REENTRANCY');
            self.reentrancy_locked.write(true);
        }

        /// Release the reentrancy lock. On any revert the write is rolled back
        /// anyway, so the lock can never get stuck in the held state.
        fn exit_nonreentrant(ref self: ContractState) {
            self.reentrancy_locked.write(false);
        }
    }

    #[abi(embed_v0)]
    impl ArbExecutorImpl of super::IArbExecutor<ContractState> {
        /// See `IArbExecutor::execute`. Steps mirror the module-level flow.
        fn execute(
            ref self: ContractState,
            token_in: ContractAddress,
            min_profit: u256,
            calls: Array<Call>,
        ) -> u256 {
            // (1) Gate. Lock first, then ownership. A non-owner that trips the
            // lock simply reverts, rolling the write back.
            self.enter_nonreentrant();
            self.assert_owner();

            // (2) Whitelist every call's (target, selector) up front, before
            // touching balances or executing anything. Calldata is not checked.
            let calls_span = calls.span();
            let num_calls = calls_span.len();
            let mut i: u32 = 0;
            while i < num_calls {
                let c = calls_span.at(i);
                assert(
                    self.allowed_targets.read((*c.to, *c.selector)), 'TARGET_NOT_ALLOWED',
                );
                i += 1;
            }

            // (3) Snapshot the pre-trade token_in balance of this contract.
            let token = IERC20Dispatcher { contract_address: token_in };
            let me = get_contract_address();
            let initial = token.balance_of(me);

            // (4) Run the bundle in order. The caller seen by each target is
            // THIS contract, so funds and approvals must belong to it. Any
            // inner revert (unwrap_syscall) aborts the whole transaction.
            let mut k: u32 = 0;
            while k < num_calls {
                let c = calls_span.at(k);
                call_contract_syscall(*c.to, *c.selector, *c.calldata).unwrap_syscall();
                k += 1;
            }

            // (5) Enforce the profit invariant. `+` on u256 panics on overflow,
            // which only reverts (safe). The assert guarantees final >= initial,
            // so the subtraction below cannot underflow.
            let final_balance = token.balance_of(me);
            assert(final_balance >= initial + min_profit, 'INSUFFICIENT_PROFIT');
            let profit = final_balance - initial;

            // (6) Release the lock and return realized profit.
            self.exit_nonreentrant();
            profit
        }

        /// See `IArbExecutor::allow_target`.
        fn allow_target(ref self: ContractState, target: ContractAddress, selector: felt252) {
            self.assert_owner();
            assert(!target.is_zero(), 'TARGET_ZERO');
            self.allowed_targets.write((target, selector), true);
        }

        /// See `IArbExecutor::disallow_target`.
        fn disallow_target(ref self: ContractState, target: ContractAddress, selector: felt252) {
            self.assert_owner();
            self.allowed_targets.write((target, selector), false);
        }

        /// See `IArbExecutor::is_target_allowed`.
        fn is_target_allowed(
            self: @ContractState, target: ContractAddress, selector: felt252,
        ) -> bool {
            self.allowed_targets.read((target, selector))
        }

        /// See `IArbExecutor::transfer_ownership`. Single-step on purpose;
        /// guard the address carefully.
        fn transfer_ownership(ref self: ContractState, new_owner: ContractAddress) {
            self.assert_owner();
            assert(!new_owner.is_zero(), 'OWNER_ZERO');
            self.owner.write(new_owner);
        }

        /// See `IArbExecutor::owner`.
        fn owner(self: @ContractState) -> ContractAddress {
            self.owner.read()
        }

        /// See `IArbExecutor::withdraw`.
        fn withdraw(
            ref self: ContractState,
            token: ContractAddress,
            to: ContractAddress,
            amount: u256,
        ) {
            self.assert_owner();
            assert(!to.is_zero(), 'TO_ZERO');
            let erc = IERC20Dispatcher { contract_address: token };
            let ok = erc.transfer(to, amount);
            assert(ok, 'TRANSFER_FAILED');
        }
    }
}
