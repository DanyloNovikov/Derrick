//! Library half of the derrick binary. The actual entry point lives in
//! `src/main.rs`; everything below is exposed so unit tests can exercise
//! config parsing, shutdown plumbing, etc., without spinning up the binary.

pub mod calls;
pub mod config;
pub mod inclusion;
pub mod observability;
pub mod pipeline;
pub mod registry;
pub mod shutdown;
pub mod wiring;

pub use config::{AppConfig, ConfigError};
pub use observability::init_observability;
pub use registry::{BoxedPool, PoolRegistry, RegistryError};
pub use shutdown::Shutdown;
