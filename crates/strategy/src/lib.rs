//! Strategy layer: profit evaluation and trade sizing.
//!
//! Layered on top of `domain::Pool` adapters and `math` primitives.
//! I/O-free — all quotes go through `quote_in_local` against cached pool state.

pub mod profit;
pub mod sizer;
pub mod spatial;

pub use profit::{evaluate_path, EvalError, PathOutcome, ProfitParams};
pub use sizer::{find_optimal_input, SizedTrade, SizerError};
pub use spatial::{detect_spatial_opportunities, SpatialParams};
