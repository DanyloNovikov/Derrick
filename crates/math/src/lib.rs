//! Pure math for derrick. I/O-free; no async; no domain types from domain.
//!
//! Why isolated: the math is the hottest path in the bot and the easiest place
//! to make catastrophic mistakes. Keeping it as a thin layer over `U256` /
//! `U512` makes property-based testing and backtesting trivial.

pub mod cpmm;

pub use cpmm::{cpmm_quote_in, cpmm_quote_out, MathError, FEE_DENOM};
