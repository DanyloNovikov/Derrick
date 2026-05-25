//! Risk gating for derrick.
//!
//! Every trade proposal goes through [`RiskManager::evaluate`] before
//! simulation. After each execution attempt, the outcome is recorded via
//! [`RiskManager::record`]; this drives the circuit breaker and the daily
//! loss accounting.
//!
//! All checks are deterministic and synchronous — no I/O, no async. State
//! lives behind a `std::sync::Mutex`; the locks are never held across awaits.

pub mod clock;
pub mod config;
pub mod manager;

pub use clock::{Clock, SystemClock};
pub use config::{PerTokenLimits, RiskConfig};
pub use manager::{RiskManager, RiskRejection, TradeOutcome, TradeProposal};
