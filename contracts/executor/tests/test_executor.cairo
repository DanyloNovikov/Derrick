// snforge tests for ArbExecutor.
//
// Coverage:
//   1. happy_path             — execute returns the realized profit
//   2. insufficient_profit    — execute reverts when final < initial + min_profit
//   3. only_operator          — non-operator caller is rejected
//   4. target_not_allowed     — call to non-whitelisted (target, selector) is rejected
//
// We use snforge cheatcodes instead of deploying real ERC-20 / DEX mocks:
//   * `mock_call(token, balance_of, value, n)` queues `n` synthetic balance
//     returns so the executor sees the desired before/after deltas.
//   * `mock_call(target, selector, (), 1)` lets the multicall's syscall succeed.
//
// `start_cheat_caller_address(executor, who)` impersonates the caller of the
// executor's external functions (owner for setup, operator for execute).

use snforge_std::{
    declare, ContractClassTrait, DeclareResultTrait, start_cheat_caller_address,
    stop_cheat_caller_address, mock_call,
};
use starknet::{ContractAddress, contract_address_const};
use starknet::account::Call;
use derrick_executor::{IArbExecutorDispatcher, IArbExecutorDispatcherTrait};

fn owner_addr() -> ContractAddress {
    contract_address_const::<0x1111>()
}

fn operator_addr() -> ContractAddress {
    contract_address_const::<0x2222>()
}

fn token_addr() -> ContractAddress {
    contract_address_const::<0x3333>()
}

fn target_addr() -> ContractAddress {
    contract_address_const::<0x4444>()
}

fn target_selector() -> felt252 {
    selector!("do_swap")
}

fn balance_of_selector() -> felt252 {
    selector!("balance_of")
}

/// Deploy the executor with `owner_addr()` and return a dispatcher. The
/// returned contract address is the one to impersonate with cheats.
fn deploy_executor() -> (ContractAddress, IArbExecutorDispatcher) {
    let contract = declare("ArbExecutor").unwrap().contract_class();
    let mut calldata: Array<felt252> = ArrayTrait::new();
    let owner = owner_addr();
    Serde::serialize(@owner, ref calldata);
    let (addr, _) = contract.deploy(@calldata).unwrap();
    let dispatcher = IArbExecutorDispatcher { contract_address: addr };
    (addr, dispatcher)
}

/// Configure executor: add operator and whitelist (target, do_swap).
fn configure_for_happy_path(executor_addr: ContractAddress, exec: IArbExecutorDispatcher) {
    start_cheat_caller_address(executor_addr, owner_addr());
    exec.add_operator(operator_addr());
    exec.allow_target(target_addr(), target_selector());
    stop_cheat_caller_address(executor_addr);
}

#[test]
fn happy_path_returns_profit() {
    let (executor_addr, exec) = deploy_executor();
    configure_for_happy_path(executor_addr, exec);

    // balance_of called twice: initial=0, final=1000 → profit=1000.
    mock_call(token_addr(), balance_of_selector(), 0_u256, 1);
    mock_call(token_addr(), balance_of_selector(), 1_000_u256, 1);
    // Inner swap call returns nothing.
    mock_call(target_addr(), target_selector(), (), 1);

    let calls: Array<Call> = array![
        Call {
            to: target_addr(),
            selector: target_selector(),
            calldata: array![].span(),
        }
    ];
    start_cheat_caller_address(executor_addr, operator_addr());
    let profit = exec.execute(token_addr(), 100_u256, calls);
    stop_cheat_caller_address(executor_addr);

    assert(profit == 1_000_u256, 'unexpected profit');
}

#[test]
#[should_panic(expected: ('INSUFFICIENT_PROFIT',))]
fn insufficient_profit_reverts() {
    let (executor_addr, exec) = deploy_executor();
    configure_for_happy_path(executor_addr, exec);

    // initial=100, final=100 → realized=0 < min_profit=50 → revert.
    mock_call(token_addr(), balance_of_selector(), 100_u256, 1);
    mock_call(token_addr(), balance_of_selector(), 100_u256, 1);
    mock_call(target_addr(), target_selector(), (), 1);

    let calls: Array<Call> = array![
        Call {
            to: target_addr(),
            selector: target_selector(),
            calldata: array![].span(),
        }
    ];
    start_cheat_caller_address(executor_addr, operator_addr());
    exec.execute(token_addr(), 50_u256, calls);
    stop_cheat_caller_address(executor_addr);
}

#[test]
#[should_panic(expected: ('ONLY_OPERATOR',))]
fn non_operator_cannot_execute() {
    let (executor_addr, exec) = deploy_executor();
    // Whitelist a target so we get past that gate; we still expect the
    // operator check (which runs first) to revert.
    start_cheat_caller_address(executor_addr, owner_addr());
    exec.allow_target(target_addr(), target_selector());
    stop_cheat_caller_address(executor_addr);

    // Random non-operator caller.
    let attacker = contract_address_const::<0x9999>();
    let calls: Array<Call> = array![
        Call {
            to: target_addr(),
            selector: target_selector(),
            calldata: array![].span(),
        }
    ];
    start_cheat_caller_address(executor_addr, attacker);
    exec.execute(token_addr(), 0_u256, calls);
    stop_cheat_caller_address(executor_addr);
}

#[test]
#[should_panic(expected: ('TARGET_NOT_ALLOWED',))]
fn unallowed_target_reverts() {
    let (executor_addr, exec) = deploy_executor();
    // Add operator but DON'T whitelist any target.
    start_cheat_caller_address(executor_addr, owner_addr());
    exec.add_operator(operator_addr());
    stop_cheat_caller_address(executor_addr);

    let calls: Array<Call> = array![
        Call {
            to: target_addr(),
            selector: target_selector(),
            calldata: array![].span(),
        }
    ];
    start_cheat_caller_address(executor_addr, operator_addr());
    exec.execute(token_addr(), 0_u256, calls);
    stop_cheat_caller_address(executor_addr);
}
