//! Trade-attempt ledger backed by Postgres.
//!
//! Every trade attempt is written here as it moves through the pipeline. The
//! `attempts` table accumulates the full lifecycle (detected → sized →
//! risk-gated → simulated → submitted → executed/reverted) keyed on a single
//! UUID per attempt.

pub mod ledger;

pub use ledger::{AttemptRecord, AttemptStatus, AttemptStatusUpdate, Ledger, LedgerError};
