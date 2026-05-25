use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChainError {
    #[error("rpc transport error: {0}")]
    Rpc(String),

    #[error("contract call reverted: {0}")]
    Reverted(String),

    #[error("encoding error: {0}")]
    Encoding(String),

    #[error("simulation diverged from off-chain model: {0}")]
    ModelDivergence(String),

    #[error("invalid felt: {0}")]
    InvalidFelt(String),
}
