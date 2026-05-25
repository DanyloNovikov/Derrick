-- Every trade attempt — detected, sized, risk-gated, simulated, submitted —
-- gets one row here, updated as it progresses through the pipeline.
--
-- U256 values are stored as TEXT (decimal). NUMERIC(78,0) would be the
-- semantically correct choice but requires a custom Encode/Decode for
-- primitive_types::U256; deferred to a follow-up migration.

CREATE TABLE IF NOT EXISTS attempts (
    id              UUID        PRIMARY KEY,
    detected_at     TIMESTAMPTZ NOT NULL,
    completed_at    TIMESTAMPTZ,
    status          TEXT        NOT NULL,
    token_in_addr   TEXT        NOT NULL,
    amount_in       TEXT        NOT NULL,
    expected_profit TEXT,
    realized_profit TEXT,
    gas_paid        TEXT,
    reason          TEXT,
    path            JSONB       NOT NULL,
    tx_hash         TEXT
);

CREATE INDEX IF NOT EXISTS attempts_detected_at_idx ON attempts (detected_at DESC);
CREATE INDEX IF NOT EXISTS attempts_status_idx      ON attempts (status);
CREATE INDEX IF NOT EXISTS attempts_token_in_idx    ON attempts (token_in_addr);
