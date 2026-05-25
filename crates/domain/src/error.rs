use thiserror::Error;

use crate::token::TokenId;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid token symbol: {0:?}")]
    InvalidSymbol(String),

    #[error("path is empty")]
    EmptyPath,

    #[error("path hops are discontinuous (token_out != next.token_in)")]
    DiscontinuousPath,

    #[error("trivial hop: token_in == token_out")]
    TrivialHop,

    #[error(transparent)]
    Amount(#[from] AmountError),

    #[error(transparent)]
    Quote(#[from] QuoteError),

    #[error(transparent)]
    State(#[from] StateError),
}

#[derive(Debug, Error)]
pub enum AmountError {
    #[error("amount token mismatch: lhs={lhs}, rhs={rhs}")]
    TokenMismatch { lhs: TokenId, rhs: TokenId },

    #[error("amount overflow")]
    Overflow,

    #[error("amount underflow")]
    Underflow,

    #[error("division by zero")]
    DivisionByZero,
}

#[derive(Debug, Error)]
pub enum QuoteError {
    #[error("token {0} is not in this pool")]
    TokenNotInPool(TokenId),

    #[error("pool state not loaded (no snapshot taken yet)")]
    StateNotLoaded,

    #[error("amount_in is zero")]
    ZeroInput,

    #[error("local quote is unavailable for this pool kind; use on-chain quote")]
    LocalUnavailable,

    #[error("insufficient liquidity (requested output exceeds reserves)")]
    InsufficientLiquidity,

    #[error("math overflow during quote computation")]
    MathOverflow,

    #[error("on-chain quote failed: {0}")]
    OnChain(String),
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("malformed event payload: {0}")]
    Malformed(String),

    #[error("out-of-order event: got block={got}, current state at block={current}")]
    OutOfOrder { got: u64, current: u64 },

    #[error("duplicate event (already applied): {0}")]
    Duplicate(String),

    #[error("event references a pool this adapter does not manage")]
    WrongPool,
}
