use std::collections::HashMap;
use std::sync::Mutex;

use domain::{SignedAmount, TokenId, U256};
use thiserror::Error;
use tracing::warn;

use crate::clock::Clock;
use crate::config::RiskConfig;

const DAY_MS: u64 = 86_400_000;

#[derive(Debug, Clone, Copy)]
pub struct TradeProposal {
    pub token_in: TokenId,
    pub amount_in: U256,
    /// Off-chain-estimated net profit (post-gas, post-safety-margin).
    pub expected_profit: SignedAmount,
}

#[derive(Debug, Clone, Copy)]
pub enum TradeOutcome {
    /// Trade went on-chain. `realized_profit` can be negative if executor
    /// emitted profit smaller than gas paid (shouldn't happen given the
    /// on-chain `assert(final >= initial + min_profit)`, but we still account
    /// for it defensively).
    Executed {
        token: TokenId,
        realized_profit: SignedAmount,
    },
    /// Transaction reverted on-chain. Gas paid is a pure loss.
    Reverted { token: TokenId, gas_paid: U256 },
    /// Simulation flagged divergence (or other pre-flight reject) — we
    /// skipped submission. Counted as a soft failure.
    SkippedSimulation { token: TokenId },
}

#[derive(Debug, Error, Clone)]
pub enum RiskRejection {
    #[error("token {0} is not in the whitelist")]
    NotWhitelisted(TokenId),

    #[error("token {0} has no configured per-token limits (config bug)")]
    NoLimitsConfigured(TokenId),

    #[error("position too large for {token}: requested={requested}, max={max}")]
    PositionTooLarge {
        token: TokenId,
        requested: U256,
        max: U256,
    },

    #[error("expected profit non-positive: {profit:?}")]
    NonPositiveExpectedProfit { profit: SignedAmount },

    #[error("profit below threshold for {token}: profit={profit}, min={min}")]
    ProfitBelowThreshold {
        token: TokenId,
        profit: U256,
        min: U256,
    },

    #[error("circuit breaker active until ts_ms={until_ms}")]
    CircuitBreakerActive { until_ms: u64 },

    #[error("daily loss exceeded for {token}: loss={loss}, max={max}")]
    DailyLossExceeded {
        token: TokenId,
        loss: U256,
        max: U256,
    },
}

#[derive(Debug)]
struct State {
    consecutive_failures: u32,
    paused_until_ms: u64,
    daily_loss: HashMap<TokenId, U256>,
    daily_reset_ms: u64,
}

pub struct RiskManager<C: Clock> {
    config: RiskConfig,
    state: Mutex<State>,
    clock: C,
}

impl<C: Clock> RiskManager<C> {
    pub fn new(config: RiskConfig, clock: C) -> Self {
        let now = clock.now_ms();
        Self {
            config,
            state: Mutex::new(State {
                consecutive_failures: 0,
                paused_until_ms: 0,
                daily_loss: HashMap::new(),
                daily_reset_ms: now,
            }),
            clock,
        }
    }

    /// Gate a trade proposal. Returns `Ok(())` if all checks pass.
    pub fn evaluate(&self, prop: &TradeProposal) -> Result<(), RiskRejection> {
        if !self.config.token_whitelist.contains(&prop.token_in) {
            return Err(RiskRejection::NotWhitelisted(prop.token_in));
        }
        let limits = self
            .config
            .limits_for(&prop.token_in)
            .ok_or(RiskRejection::NoLimitsConfigured(prop.token_in))?;

        if prop.amount_in > limits.max_position {
            return Err(RiskRejection::PositionTooLarge {
                token: prop.token_in,
                requested: prop.amount_in,
                max: limits.max_position,
            });
        }

        if !prop.expected_profit.is_positive() {
            return Err(RiskRejection::NonPositiveExpectedProfit {
                profit: prop.expected_profit,
            });
        }
        let expected_abs = prop.expected_profit.abs();
        if expected_abs < limits.min_profit {
            return Err(RiskRejection::ProfitBelowThreshold {
                token: prop.token_in,
                profit: expected_abs,
                min: limits.min_profit,
            });
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.clock.now_ms();
        Self::maybe_roll_daily(&mut state, now);

        if state.paused_until_ms > now {
            return Err(RiskRejection::CircuitBreakerActive {
                until_ms: state.paused_until_ms,
            });
        }

        if let Some(&loss) = state.daily_loss.get(&prop.token_in) {
            if loss > limits.daily_max_loss {
                return Err(RiskRejection::DailyLossExceeded {
                    token: prop.token_in,
                    loss,
                    max: limits.daily_max_loss,
                });
            }
        }

        Ok(())
    }

    /// Record the outcome of a trade attempt. Updates failure counters and
    /// daily loss; may trip the circuit breaker.
    pub fn record(&self, outcome: TradeOutcome) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.clock.now_ms();
        Self::maybe_roll_daily(&mut state, now);

        match outcome {
            TradeOutcome::Executed {
                realized_profit, ..
            } => {
                if realized_profit.is_positive() {
                    state.consecutive_failures = 0;
                } else if realized_profit.is_negative() {
                    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                    let token = realized_profit.token();
                    let entry = state.daily_loss.entry(token).or_default();
                    *entry = entry.saturating_add(realized_profit.abs());
                }
            }
            TradeOutcome::Reverted { token, gas_paid } => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                let entry = state.daily_loss.entry(token).or_default();
                *entry = entry.saturating_add(gas_paid);
            }
            TradeOutcome::SkippedSimulation { .. } => {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            }
        }

        if state.consecutive_failures >= self.config.max_consecutive_failures {
            let pause_ms = self
                .config
                .circuit_breaker_pause_seconds
                .saturating_mul(1000);
            state.paused_until_ms = now.saturating_add(pause_ms);
            warn!(
                consecutive_failures = state.consecutive_failures,
                until_ms = state.paused_until_ms,
                "circuit breaker tripped"
            );
        }
    }

    pub fn is_paused(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.paused_until_ms > self.clock.now_ms()
    }

    fn maybe_roll_daily(state: &mut State, now: u64) {
        if now >= state.daily_reset_ms.saturating_add(DAY_MS) {
            state.daily_loss.clear();
            state.daily_reset_ms = now;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::config::PerTokenLimits;
    use domain::{ContractAddress, Felt};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    struct MockClock {
        ms: Mutex<u64>,
    }

    impl MockClock {
        fn new(initial: u64) -> Self {
            Self {
                ms: Mutex::new(initial),
            }
        }
        fn advance(&self, ms: u64) {
            let mut g = self
                .ms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *g = g.saturating_add(ms);
        }
    }

    impl Clock for MockClock {
        fn now_ms(&self) -> u64 {
            *self
                .ms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    fn tok(n: u128) -> TokenId {
        TokenId::new(ContractAddress::new(Felt::from(n)))
    }

    fn config_with(token: TokenId, limits: PerTokenLimits, breaker_at: u32) -> RiskConfig {
        let mut whitelist = HashSet::new();
        whitelist.insert(token);
        let mut per_token = HashMap::new();
        per_token.insert(token, limits);
        RiskConfig {
            token_whitelist: whitelist,
            per_token,
            max_consecutive_failures: breaker_at,
            circuit_breaker_pause_seconds: 60,
        }
    }

    fn default_limits() -> PerTokenLimits {
        PerTokenLimits {
            max_position: U256::from(1_000_000u64),
            min_profit: U256::from(100u64),
            daily_max_loss: U256::from(10_000u64),
        }
    }

    fn good_proposal(token: TokenId) -> TradeProposal {
        TradeProposal {
            token_in: token,
            amount_in: U256::from(50_000u64),
            expected_profit: SignedAmount::positive(token, U256::from(500u64)),
        }
    }

    #[test]
    fn happy_path_accepts_proposal() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 3);
        let mgr = RiskManager::new(cfg, MockClock::new(0));
        assert!(mgr.evaluate(&good_proposal(t)).is_ok());
    }

    #[test]
    fn rejects_unwhitelisted_token() {
        let t = tok(1);
        let other = tok(2);
        let cfg = config_with(t, default_limits(), 3);
        let mgr = RiskManager::new(cfg, MockClock::new(0));
        let bad = TradeProposal {
            token_in: other,
            ..good_proposal(t)
        };
        assert!(matches!(
            mgr.evaluate(&bad),
            Err(RiskRejection::NotWhitelisted(_))
        ));
    }

    #[test]
    fn rejects_position_too_large() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 3);
        let mgr = RiskManager::new(cfg, MockClock::new(0));
        let bad = TradeProposal {
            amount_in: U256::from(2_000_000u64),
            ..good_proposal(t)
        };
        assert!(matches!(
            mgr.evaluate(&bad),
            Err(RiskRejection::PositionTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_non_positive_expected_profit() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 3);
        let mgr = RiskManager::new(cfg, MockClock::new(0));
        let bad = TradeProposal {
            expected_profit: SignedAmount::negative(t, U256::from(10u64)),
            ..good_proposal(t)
        };
        assert!(matches!(
            mgr.evaluate(&bad),
            Err(RiskRejection::NonPositiveExpectedProfit { .. })
        ));
    }

    #[test]
    fn rejects_profit_below_threshold() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 3);
        let mgr = RiskManager::new(cfg, MockClock::new(0));
        let bad = TradeProposal {
            expected_profit: SignedAmount::positive(t, U256::from(50u64)),
            ..good_proposal(t)
        };
        assert!(matches!(
            mgr.evaluate(&bad),
            Err(RiskRejection::ProfitBelowThreshold { .. })
        ));
    }

    #[test]
    fn rejects_without_limits_configured() {
        let t = tok(1);
        let mut whitelist = HashSet::new();
        whitelist.insert(t);
        let cfg = RiskConfig {
            token_whitelist: whitelist,
            per_token: HashMap::new(), // empty — limits missing for t
            max_consecutive_failures: 3,
            circuit_breaker_pause_seconds: 60,
        };
        let mgr = RiskManager::new(cfg, MockClock::new(0));
        assert!(matches!(
            mgr.evaluate(&good_proposal(t)),
            Err(RiskRejection::NoLimitsConfigured(_))
        ));
    }

    #[test]
    fn circuit_breaker_trips_after_n_failures_and_clears_after_pause() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 3);
        let clock = MockClock::new(1_000_000);
        let mgr = RiskManager::new(cfg, clock);

        // 3 consecutive reverts → tripped
        for _ in 0..3 {
            mgr.record(TradeOutcome::Reverted {
                token: t,
                gas_paid: U256::from(10u64),
            });
        }
        assert!(mgr.is_paused());
        assert!(matches!(
            mgr.evaluate(&good_proposal(t)),
            Err(RiskRejection::CircuitBreakerActive { .. })
        ));

        // After 60s the pause clears
        mgr.clock.advance(60_000);
        assert!(!mgr.is_paused());
        // The consecutive_failures counter is NOT auto-reset by time alone — it
        // resets on the next successful execution. Until then we're paused-but-clearable.
        assert!(mgr.evaluate(&good_proposal(t)).is_ok());
    }

    #[test]
    fn successful_execution_resets_consecutive_failures() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 3);
        let mgr = RiskManager::new(cfg, MockClock::new(0));

        mgr.record(TradeOutcome::Reverted {
            token: t,
            gas_paid: U256::from(1u64),
        });
        mgr.record(TradeOutcome::Reverted {
            token: t,
            gas_paid: U256::from(1u64),
        });
        // After 2 reverts, still not paused (threshold is 3).
        assert!(!mgr.is_paused());

        mgr.record(TradeOutcome::Executed {
            token: t,
            realized_profit: SignedAmount::positive(t, U256::from(100u64)),
        });

        // Next 2 reverts shouldn't trip yet (counter reset to 0).
        mgr.record(TradeOutcome::Reverted {
            token: t,
            gas_paid: U256::from(1u64),
        });
        mgr.record(TradeOutcome::Reverted {
            token: t,
            gas_paid: U256::from(1u64),
        });
        assert!(!mgr.is_paused());
    }

    #[test]
    fn daily_loss_exceeds_max_blocks_evaluation() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 100); // breaker effectively off
        let mgr = RiskManager::new(cfg, MockClock::new(0));

        // daily_max_loss is 10_000. One revert paying 15_000 gas → over.
        mgr.record(TradeOutcome::Reverted {
            token: t,
            gas_paid: U256::from(15_000u64),
        });

        assert!(matches!(
            mgr.evaluate(&good_proposal(t)),
            Err(RiskRejection::DailyLossExceeded { .. })
        ));
    }

    #[test]
    fn daily_loss_resets_after_24h() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 100);
        let clock = MockClock::new(0);
        let mgr = RiskManager::new(cfg, clock);

        mgr.record(TradeOutcome::Reverted {
            token: t,
            gas_paid: U256::from(15_000u64),
        });
        assert!(matches!(
            mgr.evaluate(&good_proposal(t)),
            Err(RiskRejection::DailyLossExceeded { .. })
        ));

        mgr.clock.advance(DAY_MS + 1);
        // After roll-over, daily loss clears.
        assert!(mgr.evaluate(&good_proposal(t)).is_ok());
    }

    #[test]
    fn executed_with_negative_realized_counts_as_loss() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 100);
        let mgr = RiskManager::new(cfg, MockClock::new(0));

        mgr.record(TradeOutcome::Executed {
            token: t,
            realized_profit: SignedAmount::negative(t, U256::from(15_000u64)),
        });
        assert!(matches!(
            mgr.evaluate(&good_proposal(t)),
            Err(RiskRejection::DailyLossExceeded { .. })
        ));
    }

    #[test]
    fn skipped_simulation_counts_as_soft_failure_for_breaker() {
        let t = tok(1);
        let cfg = config_with(t, default_limits(), 2);
        let mgr = RiskManager::new(cfg, MockClock::new(0));

        mgr.record(TradeOutcome::SkippedSimulation { token: t });
        mgr.record(TradeOutcome::SkippedSimulation { token: t });
        assert!(mgr.is_paused());
    }
}
