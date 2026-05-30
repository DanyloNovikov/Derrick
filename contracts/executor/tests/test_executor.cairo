//! Exhaustive snforge test suite for `DerrickExecutor`.
//!
//! Instead of `mock_call`, this suite deploys real mock contracts so the profit
//! accounting is exercised end-to-end against actual ERC20 storage:
//!
//!   * `MockERC20`  — minimal ERC20 with `mint`/`burn` and a `set_transfer_result`
//!                    toggle so we can simulate a token whose `transfer` returns
//!                    false (for the `withdraw` failure path).
//!   * `MockDex`    — stands in for a DEX. `produce(token, amount)` mints `amount`
//!                    of `token` to its caller (the executor) => simulated profit;
//!                    `consume(token, amount)` burns from its caller => simulated
//!                    loss; `boom()` reverts => exercises inner-call propagation.
//!   * `MockReentrant` — calls back into `execute` to trip the reentrancy guard.
//!
//! Coverage map:
//!   constructor : zero-owner rejected, owner stored
//!   ownership   : view, transfer, non-owner, zero, post-transfer authority swap
//!   whitelist   : allow/disallow, default-false, per-selector, access control
//!   execute gate: non-owner, unallowed target, one-of-many unallowed, reentrancy
//!   execute math: happy path, exact boundary, one-below, zero min_profit (±gain),
//!                 empty calls (±min_profit), multi-call accumulation, loss,
//!                 inner-call revert propagation
//!   withdraw    : success, non-owner, zero recipient, transfer-returns-false

use snforge_std::{
    declare, ContractClassTrait, DeclareResultTrait, start_cheat_caller_address,
    stop_cheat_caller_address,
};
use starknet::ContractAddress;
use starknet::account::Call;
use derrick_executor::{IDerrickExecutorDispatcher, IDerrickExecutorDispatcherTrait};

// ---------------------------------------------------------------------------
// Mock ERC20
// ---------------------------------------------------------------------------

#[starknet::interface]
trait IMockERC20<T> {
    fn balance_of(self: @T, account: ContractAddress) -> u256;
    fn transfer(ref self: T, recipient: ContractAddress, amount: u256) -> bool;
    fn mint(ref self: T, to: ContractAddress, amount: u256);
    fn burn(ref self: T, from: ContractAddress, amount: u256);
    fn set_transfer_result(ref self: T, ok: bool);
}

#[starknet::contract]
mod MockERC20 {
    use starknet::ContractAddress;
    use starknet::get_caller_address;
    use starknet::storage::{
        Map, StorageMapReadAccess, StorageMapWriteAccess, StoragePointerReadAccess,
        StoragePointerWriteAccess,
    };

    #[storage]
    struct Storage {
        balances: Map<ContractAddress, u256>,
        transfer_result: bool,
    }

    #[constructor]
    fn constructor(ref self: ContractState) {
        self.transfer_result.write(true);
    }

    #[abi(embed_v0)]
    impl Impl of super::IMockERC20<ContractState> {
        fn balance_of(self: @ContractState, account: ContractAddress) -> u256 {
            self.balances.read(account)
        }

        fn transfer(ref self: ContractState, recipient: ContractAddress, amount: u256) -> bool {
            let ok = self.transfer_result.read();
            if ok {
                let from = get_caller_address();
                self.balances.write(from, self.balances.read(from) - amount);
                self.balances.write(recipient, self.balances.read(recipient) + amount);
            }
            ok
        }

        fn mint(ref self: ContractState, to: ContractAddress, amount: u256) {
            self.balances.write(to, self.balances.read(to) + amount);
        }

        fn burn(ref self: ContractState, from: ContractAddress, amount: u256) {
            self.balances.write(from, self.balances.read(from) - amount);
        }

        fn set_transfer_result(ref self: ContractState, ok: bool) {
            self.transfer_result.write(ok);
        }
    }
}

// ---------------------------------------------------------------------------
// Mock DEX — produces/consumes the caller's balance to simulate swaps
// ---------------------------------------------------------------------------

#[starknet::interface]
trait IMockDex<T> {
    fn produce(ref self: T, token: ContractAddress, amount: u256);
    fn consume(ref self: T, token: ContractAddress, amount: u256);
    fn boom(ref self: T);
}

#[starknet::contract]
mod MockDex {
    use starknet::{ContractAddress, get_caller_address};
    use super::{IMockERC20Dispatcher, IMockERC20DispatcherTrait};

    #[storage]
    struct Storage {}

    #[abi(embed_v0)]
    impl Impl of super::IMockDex<ContractState> {
        fn produce(ref self: ContractState, token: ContractAddress, amount: u256) {
            let caller = get_caller_address();
            IMockERC20Dispatcher { contract_address: token }.mint(caller, amount);
        }

        fn consume(ref self: ContractState, token: ContractAddress, amount: u256) {
            let caller = get_caller_address();
            IMockERC20Dispatcher { contract_address: token }.burn(caller, amount);
        }

        fn boom(ref self: ContractState) {
            assert(false, 'BOOM');
        }
    }
}

// ---------------------------------------------------------------------------
// Mock reentrant target — calls back into execute to trip the guard
// ---------------------------------------------------------------------------

#[starknet::interface]
trait IMockReentrant<T> {
    fn reenter(ref self: T, executor: ContractAddress, token: ContractAddress);
}

#[starknet::contract]
mod MockReentrant {
    use starknet::ContractAddress;
    use starknet::account::Call;
    use derrick_executor::{IDerrickExecutorDispatcher, IDerrickExecutorDispatcherTrait};

    #[storage]
    struct Storage {}

    #[abi(embed_v0)]
    impl Impl of super::IMockReentrant<ContractState> {
        fn reenter(ref self: ContractState, executor: ContractAddress, token: ContractAddress) {
            let calls: Array<Call> = array![];
            IDerrickExecutorDispatcher { contract_address: executor }.execute(token, 0_u256, calls);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn owner_addr() -> ContractAddress {
    0x1111.try_into().unwrap()
}

fn other_addr() -> ContractAddress {
    0x9999.try_into().unwrap()
}

fn deploy_executor() -> (ContractAddress, IDerrickExecutorDispatcher) {
    let contract = declare("DerrickExecutor").unwrap().contract_class();
    let mut calldata = array![];
    Serde::serialize(@owner_addr(), ref calldata);
    let (addr, _) = contract.deploy(@calldata).unwrap();
    (addr, IDerrickExecutorDispatcher { contract_address: addr })
}

fn deploy_token() -> (ContractAddress, IMockERC20Dispatcher) {
    let contract = declare("MockERC20").unwrap().contract_class();
    let (addr, _) = contract.deploy(@array![]).unwrap();
    (addr, IMockERC20Dispatcher { contract_address: addr })
}

fn deploy_dex() -> ContractAddress {
    let contract = declare("MockDex").unwrap().contract_class();
    let (addr, _) = contract.deploy(@array![]).unwrap();
    addr
}

fn deploy_reentrant() -> ContractAddress {
    let contract = declare("MockReentrant").unwrap().contract_class();
    let (addr, _) = contract.deploy(@array![]).unwrap();
    addr
}

/// Whitelist `(target, selector)` as the owner.
fn allow(executor_addr: ContractAddress, exec: IDerrickExecutorDispatcher, target: ContractAddress, selector: felt252) {
    start_cheat_caller_address(executor_addr, owner_addr());
    exec.allow_target(target, selector);
    stop_cheat_caller_address(executor_addr);
}

/// Build a `Call` to `MockDex.produce(token, amount)`.
fn produce_call(dex: ContractAddress, token: ContractAddress, amount: u256) -> Call {
    let mut cd = array![];
    Serde::serialize(@token, ref cd);
    Serde::serialize(@amount, ref cd);
    Call { to: dex, selector: selector!("produce"), calldata: cd.span() }
}

/// Build a `Call` to `MockDex.consume(token, amount)`.
fn consume_call(dex: ContractAddress, token: ContractAddress, amount: u256) -> Call {
    let mut cd = array![];
    Serde::serialize(@token, ref cd);
    Serde::serialize(@amount, ref cd);
    Call { to: dex, selector: selector!("consume"), calldata: cd.span() }
}

/// Deploy executor + token + dex, whitelist produce & consume & boom, mint
/// `initial` token to the executor. Returns the common handles.
fn setup_full(initial: u256) -> (
    ContractAddress, IDerrickExecutorDispatcher, ContractAddress, IMockERC20Dispatcher, ContractAddress,
) {
    let (exec_addr, exec) = deploy_executor();
    let (token_addr, token) = deploy_token();
    let dex = deploy_dex();
    allow(exec_addr, exec, dex, selector!("produce"));
    allow(exec_addr, exec, dex, selector!("consume"));
    allow(exec_addr, exec, dex, selector!("boom"));
    token.mint(exec_addr, initial);
    (exec_addr, exec, token_addr, token, dex)
}

fn run_as_owner(exec_addr: ContractAddress, exec: IDerrickExecutorDispatcher, token: ContractAddress, min_profit: u256, calls: Array<Call>) -> u256 {
    start_cheat_caller_address(exec_addr, owner_addr());
    let p = exec.execute(token, min_profit, calls);
    stop_cheat_caller_address(exec_addr);
    p
}

// ===========================================================================
// Constructor
// ===========================================================================

#[test]
fn constructor_rejects_zero_owner() {
    // A failed constructor surfaces as `Err(panic_data)` from `deploy`, so we
    // inspect the data directly rather than relying on `unwrap`'s own panic.
    let contract = declare("DerrickExecutor").unwrap().contract_class();
    let zero: ContractAddress = 0.try_into().unwrap();
    let mut calldata = array![];
    Serde::serialize(@zero, ref calldata);
    match contract.deploy(@calldata) {
        Result::Ok(_) => panic!("deploy should have reverted on zero owner"),
        Result::Err(data) => assert(*data.at(0) == 'OWNER_ZERO', 'wrong panic reason'),
    }
}

#[test]
fn constructor_stores_owner() {
    let (_, exec) = deploy_executor();
    assert(exec.owner() == owner_addr(), 'owner mismatch');
}

// ===========================================================================
// Ownership
// ===========================================================================

#[test]
fn transfer_ownership_changes_owner() {
    let (exec_addr, exec) = deploy_executor();
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.transfer_ownership(other_addr());
    stop_cheat_caller_address(exec_addr);
    assert(exec.owner() == other_addr(), 'new owner');
}

#[test]
#[should_panic(expected: ('ONLY_OWNER',))]
fn transfer_ownership_rejects_non_owner() {
    let (exec_addr, exec) = deploy_executor();
    start_cheat_caller_address(exec_addr, other_addr());
    exec.transfer_ownership(other_addr());
    stop_cheat_caller_address(exec_addr);
}

#[test]
#[should_panic(expected: ('OWNER_ZERO',))]
fn transfer_ownership_rejects_zero() {
    let (exec_addr, exec) = deploy_executor();
    let zero: ContractAddress = 0.try_into().unwrap();
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.transfer_ownership(zero);
    stop_cheat_caller_address(exec_addr);
}

#[test]
fn new_owner_gains_authority_old_owner_loses_it() {
    let (exec_addr, exec) = deploy_executor();
    let target: ContractAddress = 0x4444.try_into().unwrap();

    // Hand over to `other`.
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.transfer_ownership(other_addr());
    stop_cheat_caller_address(exec_addr);

    // New owner can configure.
    start_cheat_caller_address(exec_addr, other_addr());
    exec.allow_target(target, selector!("x"));
    stop_cheat_caller_address(exec_addr);
    assert(exec.is_target_allowed(target, selector!("x")), 'new owner can allow');
}

#[test]
#[should_panic(expected: ('ONLY_OWNER',))]
fn old_owner_rejected_after_transfer() {
    let (exec_addr, exec) = deploy_executor();
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.transfer_ownership(other_addr());
    stop_cheat_caller_address(exec_addr);

    // Old owner tries to act — must be rejected.
    let target: ContractAddress = 0x4444.try_into().unwrap();
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.allow_target(target, selector!("x"));
    stop_cheat_caller_address(exec_addr);
}

// ===========================================================================
// Whitelist
// ===========================================================================

#[test]
fn allow_then_is_target_allowed() {
    let (exec_addr, exec) = deploy_executor();
    let target: ContractAddress = 0x4444.try_into().unwrap();
    assert(!exec.is_target_allowed(target, selector!("s")), 'default false');
    allow(exec_addr, exec, target, selector!("s"));
    assert(exec.is_target_allowed(target, selector!("s")), 'allowed');
}

#[test]
fn disallow_clears_whitelist() {
    let (exec_addr, exec) = deploy_executor();
    let target: ContractAddress = 0x4444.try_into().unwrap();
    allow(exec_addr, exec, target, selector!("s"));
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.disallow_target(target, selector!("s"));
    stop_cheat_caller_address(exec_addr);
    assert(!exec.is_target_allowed(target, selector!("s")), 'cleared');
}

#[test]
fn whitelist_is_per_selector() {
    let (exec_addr, exec) = deploy_executor();
    let target: ContractAddress = 0x4444.try_into().unwrap();
    allow(exec_addr, exec, target, selector!("a"));
    assert(exec.is_target_allowed(target, selector!("a")), 'a allowed');
    assert(!exec.is_target_allowed(target, selector!("b")), 'b not allowed');
}

#[test]
#[should_panic(expected: ('ONLY_OWNER',))]
fn allow_target_rejects_non_owner() {
    let (exec_addr, exec) = deploy_executor();
    let target: ContractAddress = 0x4444.try_into().unwrap();
    start_cheat_caller_address(exec_addr, other_addr());
    exec.allow_target(target, selector!("s"));
    stop_cheat_caller_address(exec_addr);
}

#[test]
#[should_panic(expected: ('TARGET_ZERO',))]
fn allow_target_rejects_zero_target() {
    let (exec_addr, exec) = deploy_executor();
    let zero: ContractAddress = 0.try_into().unwrap();
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.allow_target(zero, selector!("s"));
    stop_cheat_caller_address(exec_addr);
}

#[test]
#[should_panic(expected: ('ONLY_OWNER',))]
fn disallow_target_rejects_non_owner() {
    let (exec_addr, exec) = deploy_executor();
    let target: ContractAddress = 0x4444.try_into().unwrap();
    start_cheat_caller_address(exec_addr, other_addr());
    exec.disallow_target(target, selector!("s"));
    stop_cheat_caller_address(exec_addr);
}

// ===========================================================================
// execute — access control & gating
// ===========================================================================

#[test]
#[should_panic(expected: ('ONLY_OWNER',))]
fn execute_rejects_non_owner() {
    let (exec_addr, exec, token_addr, _, dex) = setup_full(0);
    let calls = array![produce_call(dex, token_addr, 10_u256)];
    start_cheat_caller_address(exec_addr, other_addr());
    exec.execute(token_addr, 0_u256, calls);
    stop_cheat_caller_address(exec_addr);
}

#[test]
#[should_panic(expected: ('TARGET_NOT_ALLOWED',))]
fn execute_rejects_unallowed_target() {
    let (exec_addr, exec) = deploy_executor();
    let (token_addr, _) = deploy_token();
    let dex = deploy_dex();
    // produce is NOT whitelisted.
    let calls = array![produce_call(dex, token_addr, 10_u256)];
    run_as_owner(exec_addr, exec, token_addr, 0_u256, calls);
}

#[test]
#[should_panic(expected: ('TARGET_NOT_ALLOWED',))]
fn execute_rejects_when_one_of_many_unallowed() {
    let (exec_addr, exec, token_addr, _, dex) = setup_full(0);
    // produce is whitelisted; a bogus selector on dex is not.
    let calls = array![
        produce_call(dex, token_addr, 10_u256),
        Call { to: dex, selector: selector!("not_allowed"), calldata: array![].span() },
    ];
    run_as_owner(exec_addr, exec, token_addr, 0_u256, calls);
}

#[test]
#[should_panic(expected: ('REENTRANCY',))]
fn execute_reentrancy_is_blocked() {
    let (exec_addr, exec) = deploy_executor();
    let (token_addr, _) = deploy_token();
    let reentrant = deploy_reentrant();
    allow(exec_addr, exec, reentrant, selector!("reenter"));

    let mut cd = array![];
    Serde::serialize(@exec_addr, ref cd);
    Serde::serialize(@token_addr, ref cd);
    let calls = array![Call { to: reentrant, selector: selector!("reenter"), calldata: cd.span() }];
    run_as_owner(exec_addr, exec, token_addr, 0_u256, calls);
}

// ===========================================================================
// execute — profit math
// ===========================================================================

#[test]
fn execute_happy_path_returns_profit() {
    let (exec_addr, exec, token_addr, token, dex) = setup_full(1_000_u256);
    let calls = array![produce_call(dex, token_addr, 500_u256)];
    let profit = run_as_owner(exec_addr, exec, token_addr, 100_u256, calls);
    assert(profit == 500_u256, 'profit');
    assert(token.balance_of(exec_addr) == 1_500_u256, 'final balance');
}

#[test]
fn execute_exact_min_profit_passes() {
    let (exec_addr, exec, token_addr, _, dex) = setup_full(1_000_u256);
    // gain == min_profit exactly → `>=` holds.
    let calls = array![produce_call(dex, token_addr, 250_u256)];
    let profit = run_as_owner(exec_addr, exec, token_addr, 250_u256, calls);
    assert(profit == 250_u256, 'exact');
}

#[test]
#[should_panic(expected: ('INSUFFICIENT_PROFIT',))]
fn execute_one_below_min_profit_reverts() {
    let (exec_addr, exec, token_addr, _, dex) = setup_full(1_000_u256);
    // gain = 249, min_profit = 250 → revert.
    let calls = array![produce_call(dex, token_addr, 249_u256)];
    run_as_owner(exec_addr, exec, token_addr, 250_u256, calls);
}

#[test]
fn execute_zero_min_profit_zero_gain() {
    let (exec_addr, exec, token_addr, _, dex) = setup_full(1_000_u256);
    let calls = array![produce_call(dex, token_addr, 0_u256)];
    let profit = run_as_owner(exec_addr, exec, token_addr, 0_u256, calls);
    assert(profit == 0_u256, 'zero profit');
}

#[test]
fn execute_zero_min_profit_with_gain() {
    let (exec_addr, exec, token_addr, _, dex) = setup_full(1_000_u256);
    let calls = array![produce_call(dex, token_addr, 7_u256)];
    let profit = run_as_owner(exec_addr, exec, token_addr, 0_u256, calls);
    assert(profit == 7_u256, 'gain');
}

#[test]
fn execute_empty_calls_zero_min_profit_returns_zero() {
    let (exec_addr, exec, token_addr, _, _) = setup_full(1_000_u256);
    let calls: Array<Call> = array![];
    let profit = run_as_owner(exec_addr, exec, token_addr, 0_u256, calls);
    assert(profit == 0_u256, 'empty zero');
}

#[test]
#[should_panic(expected: ('INSUFFICIENT_PROFIT',))]
fn execute_empty_calls_nonzero_min_profit_reverts() {
    let (exec_addr, exec, token_addr, _, _) = setup_full(1_000_u256);
    let calls: Array<Call> = array![];
    run_as_owner(exec_addr, exec, token_addr, 1_u256, calls);
}

#[test]
fn execute_multi_call_accumulates_profit() {
    let (exec_addr, exec, token_addr, token, dex) = setup_full(1_000_u256);
    let calls = array![
        produce_call(dex, token_addr, 100_u256),
        produce_call(dex, token_addr, 250_u256),
        produce_call(dex, token_addr, 50_u256),
    ];
    let profit = run_as_owner(exec_addr, exec, token_addr, 400_u256, calls);
    assert(profit == 400_u256, 'accumulated');
    assert(token.balance_of(exec_addr) == 1_400_u256, 'final');
}

#[test]
#[should_panic(expected: ('INSUFFICIENT_PROFIT',))]
fn execute_net_loss_reverts() {
    let (exec_addr, exec, token_addr, _, dex) = setup_full(1_000_u256);
    // Balance shrinks → final < initial → revert even with min_profit = 0.
    let calls = array![consume_call(dex, token_addr, 100_u256)];
    run_as_owner(exec_addr, exec, token_addr, 0_u256, calls);
}

#[test]
fn execute_mixed_calls_net_profit() {
    let (exec_addr, exec, token_addr, token, dex) = setup_full(1_000_u256);
    // +500 then -200 → net +300.
    let calls = array![
        produce_call(dex, token_addr, 500_u256),
        consume_call(dex, token_addr, 200_u256),
    ];
    let profit = run_as_owner(exec_addr, exec, token_addr, 300_u256, calls);
    assert(profit == 300_u256, 'net');
    assert(token.balance_of(exec_addr) == 1_300_u256, 'final');
}

#[test]
#[should_panic(expected: ('BOOM',))]
fn execute_inner_call_revert_propagates() {
    let (exec_addr, exec, token_addr, _, dex) = setup_full(1_000_u256);
    let calls = array![Call { to: dex, selector: selector!("boom"), calldata: array![].span() }];
    run_as_owner(exec_addr, exec, token_addr, 0_u256, calls);
}

// ===========================================================================
// withdraw
// ===========================================================================

#[test]
fn withdraw_transfers_to_recipient() {
    let (exec_addr, exec) = deploy_executor();
    let (token_addr, token) = deploy_token();
    token.mint(exec_addr, 1_000_u256);

    start_cheat_caller_address(exec_addr, owner_addr());
    exec.withdraw(token_addr, other_addr(), 400_u256);
    stop_cheat_caller_address(exec_addr);

    assert(token.balance_of(other_addr()) == 400_u256, 'recipient');
    assert(token.balance_of(exec_addr) == 600_u256, 'remaining');
}

#[test]
#[should_panic(expected: ('ONLY_OWNER',))]
fn withdraw_rejects_non_owner() {
    let (exec_addr, exec) = deploy_executor();
    let (token_addr, _) = deploy_token();
    start_cheat_caller_address(exec_addr, other_addr());
    exec.withdraw(token_addr, other_addr(), 1_u256);
    stop_cheat_caller_address(exec_addr);
}

#[test]
#[should_panic(expected: ('TO_ZERO',))]
fn withdraw_rejects_zero_recipient() {
    let (exec_addr, exec) = deploy_executor();
    let (token_addr, _) = deploy_token();
    let zero: ContractAddress = 0.try_into().unwrap();
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.withdraw(token_addr, zero, 1_u256);
    stop_cheat_caller_address(exec_addr);
}

#[test]
#[should_panic(expected: ('TRANSFER_FAILED',))]
fn withdraw_reverts_when_transfer_returns_false() {
    let (exec_addr, exec) = deploy_executor();
    let (token_addr, token) = deploy_token();
    token.set_transfer_result(false);
    start_cheat_caller_address(exec_addr, owner_addr());
    exec.withdraw(token_addr, other_addr(), 1_u256);
    stop_cheat_caller_address(exec_addr);
}
