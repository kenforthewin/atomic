-- Knowledge-quality signal preferences and feedback.
--
-- These tables are per logical database, matching SQLite's per-data-DB
-- storage. Signal rows themselves are generated deterministically at read time;
-- only user/provider state is persisted.

CREATE TABLE IF NOT EXISTS knowledge_signal_preferences (
    db_id TEXT NOT NULL DEFAULT 'default',
    provider_id TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    weight REAL NOT NULL DEFAULT 1.0,
    min_score REAL NOT NULL DEFAULT 0.0,
    min_confidence REAL NOT NULL DEFAULT 0.0,
    show_on_dashboard BOOLEAN NOT NULL DEFAULT TRUE,
    include_in_briefing BOOLEAN NOT NULL DEFAULT FALSE,
    config_json TEXT NOT NULL DEFAULT '{}',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (db_id, provider_id)
);

CREATE TABLE IF NOT EXISTS knowledge_signal_feedback (
    db_id TEXT NOT NULL DEFAULT 'default',
    signal_key TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id TEXT,
    state TEXT NOT NULL,
    snoozed_until TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (db_id, signal_key)
);

CREATE INDEX IF NOT EXISTS idx_knowledge_signal_feedback_provider
    ON knowledge_signal_feedback(db_id, provider_id);
CREATE INDEX IF NOT EXISTS idx_knowledge_signal_feedback_target
    ON knowledge_signal_feedback(db_id, target_type, target_id);

INSERT INTO schema_version (version) VALUES (19);
