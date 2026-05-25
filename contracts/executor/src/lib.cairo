// SPDX-License-Identifier: UNLICENSED
//! derrick executor — atomic arbitrage multicall with on-chain profit assertion.
//!
//! Workflow per `execute(token_in, min_profit, calls)`:
//!   1. Reentrancy + pause + operator gating.
//!   2. Validate every (target, selector) against the whitelist.
//!   3. Snapshot `token_in` balance of self.
//!   4. Execute `calls` in order. Any revert reverts the whole tx.
//!   5. Snapshot again; assert `final >= initial + min_profit`.
//!   6. Emit `Executed`; return the realized profit.

use starknet::ContractAddress;
use starknet::account::Call;

#[starknet::interface]
pub trait IArbExecutor<TContractState> {
    fn execute(
        ref self: TContractState,
        token_in: ContractAddress,
        min_profit: u256,
        calls: Array<Call>,
    ) -> u256;

    fn pause(ref self: TContractState);
    fn unpause(ref self: TContractState);
    fn is_paused(self: @TContractState) -> bool;

    fn add_operator(ref self: TContractState, op: ContractAddress);
    fn remove_operator(ref self: TContractState, op: ContractAddress);
    fn is_operator(self: @TContractState, addr: ContractAddress) -> bool;

    fn allow_target(ref self: TContractState, target: ContractAddress, selector: felt252);
    fn disallow_target(ref self: TContractState, target: ContractAddress, selector: felt252);
    fn is_target_allowed(
        self: @TContractState, target: ContractAddress, selector: felt252,
    ) -> bool;

    fn transfer_ownership(ref self: TContractState, new_owner: ContractAddress);
    fn owner(self: @TContractState) -> ContractAddress;

    fn withdraw(
        ref self: TContractState,
        token: ContractAddress,
        to: ContractAddress,
        amount: u256,
    );
}

#[starknet::interface]
trait IERC20<TContractState> {
    fn balance_of(self: @TContractState, account: ContractAddress) -> u256;
    fn transfer(ref self: TContractState, recipient: ContractAddress, amount: u256) -> bool;
}

#[starknet::contract]
mod ArbExecutor {
    use core::num::traits::Zero;
    use starknet::storage::{
        Map, StorageMapReadAccess, StorageMapWriteAccess, StoragePointerReadAccess,
        StoragePointerWriteAccess,
    };
    use starknet::syscalls::call_contract_syscall;
    use starknet::{ContractAddress, get_caller_address, get_contract_address, SyscallResultTrait};
    use starknet::account::Call;
    use super::{IERC20Dispatcher, IERC20DispatcherTrait};

    #[storage]
    struct Storage {
        owner: ContractAddress,
        paused: bool,
        reentrancy_locked: bool,
        operators: Map<ContractAddress, bool>,
        allowed_targets: Map<(ContractAddress, felt252), bool>,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    enum Event {
        Executed: Executed,
        Paused: Paused,
        Unpaused: Unpaused,
        OwnershipTransferred: OwnershipTransferred,
        OperatorAdded: OperatorAdded,
        OperatorRemoved: OperatorRemoved,
        TargetAllowed: TargetAllowed,
        TargetDisallowed: TargetDisallowed,
        Withdrawn: Withdrawn,
    }

    #[derive(Drop, starknet::Event)]
    struct Executed {
        #[key]
        operator: ContractAddress,
        token_in: ContractAddress,
        profit: u256,
        num_calls: u32,
    }

    #[derive(Drop, starknet::Event)]
    struct Paused {}

    #[derive(Drop, starknet::Event)]
    struct Unpaused {}

    #[derive(Drop, starknet::Event)]
    struct OwnershipTransferred {
        from: ContractAddress,
        to: ContractAddress,
    }

    #[derive(Drop, starknet::Event)]
    struct OperatorAdded {
        operator: ContractAddress,
    }

    #[derive(Drop, starknet::Event)]
    struct OperatorRemoved {
        operator: ContractAddress,
    }

    #[derive(Drop, starknet::Event)]
    struct TargetAllowed {
        target: ContractAddress,
        selector: felt252,
    }

    #[derive(Drop, starknet::Event)]
    struct TargetDisallowed {
        target: ContractAddress,
        selector: felt252,
    }

    #[derive(Drop, starknet::Event)]
    struct Withdrawn {
        token: ContractAddress,
        to: ContractAddress,
        amount: u256,
    }

    #[constructor]
    fn constructor(ref self: ContractState, owner: ContractAddress) {
        assert(!owner.is_zero(), 'OWNER_ZERO');
        self.owner.write(owner);
    }

    #[generate_trait]
    impl InternalImpl of InternalTrait {
        fn assert_owner(self: @ContractState) {
            assert(get_caller_address() == self.owner.read(), 'ONLY_OWNER');
        }

        fn assert_operator(self: @ContractState) {
            assert(self.operators.read(get_caller_address()), 'ONLY_OPERATOR');
        }

        fn assert_not_paused(self: @ContractState) {
            assert(!self.paused.read(), 'PAUSED');
        }

        fn enter_nonreentrant(ref self: ContractState) {
            assert(!self.reentrancy_locked.read(), 'REENTRANCY');
            self.reentrancy_locked.write(true);
        }

        fn exit_nonreentrant(ref self: ContractState) {
            self.reentrancy_locked.write(false);
        }
    }

    #[abi(embed_v0)]
    impl ArbExecutorImpl of super::IArbExecutor<ContractState> {
        fn execute(
            ref self: ContractState,
            token_in: ContractAddress,
            min_profit: u256,
            calls: Array<Call>,
        ) -> u256 {
            self.enter_nonreentrant();
            self.assert_not_paused();
            self.assert_operator();

            // Validate every (target, selector) against the whitelist.
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

            // Snapshot before.
            let token = IERC20Dispatcher { contract_address: token_in };
            let me = get_contract_address();
            let initial = token.balance_of(me);

            // Execute calls in order; any revert reverts the whole tx.
            let mut k: u32 = 0;
            while k < num_calls {
                let c = calls_span.at(k);
                call_contract_syscall(*c.to, *c.selector, *c.calldata).unwrap_syscall();
                k += 1;
            }

            // Snapshot after; assert profit.
            let final_balance = token.balance_of(me);
            assert(final_balance >= initial + min_profit, 'INSUFFICIENT_PROFIT');
            let profit = final_balance - initial;

            self.emit(
                Executed {
                    operator: get_caller_address(), token_in, profit, num_calls,
                },
            );

            self.exit_nonreentrant();
            profit
        }

        fn pause(ref self: ContractState) {
            self.assert_owner();
            self.paused.write(true);
            self.emit(Paused {});
        }

        fn unpause(ref self: ContractState) {
            self.assert_owner();
            self.paused.write(false);
            self.emit(Unpaused {});
        }

        fn is_paused(self: @ContractState) -> bool {
            self.paused.read()
        }

        fn add_operator(ref self: ContractState, op: ContractAddress) {
            self.assert_owner();
            assert(!op.is_zero(), 'OPERATOR_ZERO');
            self.operators.write(op, true);
            self.emit(OperatorAdded { operator: op });
        }

        fn remove_operator(ref self: ContractState, op: ContractAddress) {
            self.assert_owner();
            self.operators.write(op, false);
            self.emit(OperatorRemoved { operator: op });
        }

        fn is_operator(self: @ContractState, addr: ContractAddress) -> bool {
            self.operators.read(addr)
        }

        fn allow_target(ref self: ContractState, target: ContractAddress, selector: felt252) {
            self.assert_owner();
            assert(!target.is_zero(), 'TARGET_ZERO');
            self.allowed_targets.write((target, selector), true);
            self.emit(TargetAllowed { target, selector });
        }

        fn disallow_target(ref self: ContractState, target: ContractAddress, selector: felt252) {
            self.assert_owner();
            self.allowed_targets.write((target, selector), false);
            self.emit(TargetDisallowed { target, selector });
        }

        fn is_target_allowed(
            self: @ContractState, target: ContractAddress, selector: felt252,
        ) -> bool {
            self.allowed_targets.read((target, selector))
        }

        fn transfer_ownership(ref self: ContractState, new_owner: ContractAddress) {
            self.assert_owner();
            assert(!new_owner.is_zero(), 'OWNER_ZERO');
            let old = self.owner.read();
            self.owner.write(new_owner);
            self.emit(OwnershipTransferred { from: old, to: new_owner });
        }

        fn owner(self: @ContractState) -> ContractAddress {
            self.owner.read()
        }

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
            self.emit(Withdrawn { token, to, amount });
        }
    }
}
