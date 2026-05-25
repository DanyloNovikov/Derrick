use chrono::{DateTime, Utc};
use domain::U256;
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Where in the pipeline the attempt currently sits. New variants append
/// without changing existing string codes — the column is plain TEXT.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    /// Detected by the `strategy` crate from a price-watcher event.
    Detected,
    /// Sized via ternary search.
    Sized,
    /// Rejected by `risk` before simulation.
    RiskRejected,
    /// Off-chain↔on-chain simulation diverged or otherwise failed.
    SimulationFailed,
    /// Simulation passed; transaction submitted to the sequencer.
    Submitted,
    /// Transaction included and executor returned a positive profit.
    Executed,
    /// Transaction included but reverted (e.g., `INSUFFICIENT_PROFIT`).
    Reverted,
    /// Paper-trading mode: simulation passed but submission was suppressed.
    /// Useful for shadow-running the bot before going live.
    PaperTraded,
}

impl AttemptStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Detected => "detected",
            Self::Sized => "sized",
            Self::RiskRejected => "risk_rejected",
            Self::SimulationFailed => "simulation_failed",
            Self::Submitted => "submitted",
            Self::Executed => "executed",
            Self::Reverted => "reverted",
            Self::PaperTraded => "paper_traded",
        }
    }
}

/// Fields that may change after an attempt is first inserted. Pass `None`
/// for any field that should keep its existing value.
#[derive(Debug, Clone, Default)]
pub struct AttemptStatusUpdate {
    pub completed_at: Option<DateTime<Utc>>,
    pub realized_profit: Option<U256>,
    pub gas_paid: Option<U256>,
    pub reason: Option<String>,
    pub tx_hash: Option<String>,
}

/// One row in the `attempts` table.
#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub id: Uuid,
    pub detected_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub status: AttemptStatus,
    pub token_in_addr: String,
    pub amount_in: U256,
    pub expected_profit: Option<U256>,
    pub realized_profit: Option<U256>,
    pub gas_paid: Option<U256>,
    pub reason: Option<String>,
    pub path_json: serde_json::Value,
    pub tx_hash: Option<String>,
}

/// Postgres-backed ledger handle. Cheap to clone — the `PgPool` is internally
/// `Arc`-counted.
#[derive(Clone, Debug)]
pub struct Ledger {
    pool: PgPool,
}

impl Ledger {
    /// Connect to Postgres at `database_url`. Caller is expected to run
    /// [`Ledger::run_migrations`] before issuing queries on a fresh DB.
    pub async fn connect(database_url: &str) -> Result<Self, LedgerError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    /// Build a ledger handle that defers connecting to the first query.
    /// Useful in tests and during dependency wiring where we need a `Ledger`
    /// value but don't want to require a live Postgres.
    pub fn lazy(database_url: &str) -> Result<Self, LedgerError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_lazy(database_url)?;
        Ok(Self { pool })
    }

    pub async fn run_migrations(&self) -> Result<(), LedgerError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    /// Insert a new attempt row.
    pub async fn insert_attempt(&self, rec: &AttemptRecord) -> Result<(), LedgerError> {
        sqlx::query(
            r"
            INSERT INTO attempts (
                id, detected_at, completed_at, status, token_in_addr, amount_in,
                expected_profit, realized_profit, gas_paid, reason, path, tx_hash
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ",
        )
        .bind(rec.id)
        .bind(rec.detected_at)
        .bind(rec.completed_at)
        .bind(rec.status.as_str())
        .bind(&rec.token_in_addr)
        .bind(rec.amount_in.to_string())
        .bind(rec.expected_profit.map(|x| x.to_string()))
        .bind(rec.realized_profit.map(|x| x.to_string()))
        .bind(rec.gas_paid.map(|x| x.to_string()))
        .bind(&rec.reason)
        .bind(&rec.path_json)
        .bind(&rec.tx_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update only the status + completion fields on an existing attempt.
    /// Fields left `None` on `update` are preserved in the row.
    pub async fn update_attempt_status(
        &self,
        id: Uuid,
        status: AttemptStatus,
        update: AttemptStatusUpdate,
    ) -> Result<(), LedgerError> {
        sqlx::query(
            r"
            UPDATE attempts
            SET status = $2,
                completed_at = COALESCE($3, completed_at),
                realized_profit = COALESCE($4, realized_profit),
                gas_paid = COALESCE($5, gas_paid),
                reason = COALESCE($6, reason),
                tx_hash = COALESCE($7, tx_hash)
            WHERE id = $1
            ",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(update.completed_at)
        .bind(update.realized_profit.map(|x| x.to_string()))
        .bind(update.gas_paid.map(|x| x.to_string()))
        .bind(update.reason)
        .bind(update.tx_hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn status_strings_are_stable_and_distinct() {
        let all = [
            AttemptStatus::Detected,
            AttemptStatus::Sized,
            AttemptStatus::RiskRejected,
            AttemptStatus::SimulationFailed,
            AttemptStatus::Submitted,
            AttemptStatus::Executed,
            AttemptStatus::Reverted,
            AttemptStatus::PaperTraded,
        ];
        let strings: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
        for (i, a) in strings.iter().enumerate() {
            for b in &strings[i + 1..] {
                assert_ne!(a, b, "status strings must be distinct");
            }
        }
        // Sanity check a few specific codings.
        assert_eq!(AttemptStatus::RiskRejected.as_str(), "risk_rejected");
        assert_eq!(
            AttemptStatus::SimulationFailed.as_str(),
            "simulation_failed"
        );
    }
}
