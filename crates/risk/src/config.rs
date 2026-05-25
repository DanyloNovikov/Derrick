use std::collections::{HashMap, HashSet};

use domain::{TokenId, U256};

/// Per-token risk parameters. Every whitelisted token MUST have an entry in
/// [`RiskConfig::per_token`] — a missing entry is treated as a config bug
/// and rejects the proposal (see [`crate::manager::RiskRejection::NoLimitsConfigured`]).
#[derive(Debug, Clone)]
pub struct PerTokenLimits {
    /// Maximum `amount_in` for a single trade, in raw token units.
    pub max_position: U256,
    /// Absolute minimum net profit (post-gas, post-safety) to accept a trade.
    /// Tiny relative spreads on tiny notional trade negative once gas is paid;
    /// this floor catches that case.
    pub min_profit: U256,
    /// Cumulative realized loss in a 24h window above which trades on this
    /// token are halted until the window rolls over.
    pub daily_max_loss: U256,
}

#[derive(Debug, Clone)]
pub struct RiskConfig {
    /// Only tokens in this set are tradeable. Implementation-level check.
    pub token_whitelist: HashSet<TokenId>,
    /// Per-token limits. Whitelist without a matching entry → reject.
    pub per_token: HashMap<TokenId, PerTokenLimits>,
    /// N consecutive reverts / failures / sim-divergences → pause for
    /// `circuit_breaker_pause_seconds` from the failing-event timestamp.
    pub max_consecutive_failures: u32,
    pub circuit_breaker_pause_seconds: u64,
}

impl RiskConfig {
    pub fn limits_for(&self, token: &TokenId) -> Option<&PerTokenLimits> {
        self.per_token.get(token)
    }
}
