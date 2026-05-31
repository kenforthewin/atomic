-- Knowledge-quality signal action audit log.
--
-- These rows record only actions taken through the knowledge-signal action
-- endpoint. General app mutations continue to use their existing routes.

CREATE TABLE IF NOT EXISTS knowledge_signal_action_log (
    db_id TEXT NOT NULL DEFAULT 'default',
    id TEXT NOT NULL,
    signal_key TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    action TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id TEXT,
    before_state_json TEXT,
    after_state_json TEXT,
    status TEXT NOT NULL,
    error TEXT,
    executed_at TEXT NOT NULL,
    undone_at TEXT,
    PRIMARY KEY (db_id, id)
);

CREATE INDEX IF NOT EXISTS idx_knowledge_signal_action_log_signal
    ON knowledge_signal_action_log(db_id, signal_key);
CREATE INDEX IF NOT EXISTS idx_knowledge_signal_action_log_provider
    ON knowledge_signal_action_log(db_id, provider_id);
CREATE INDEX IF NOT EXISTS idx_knowledge_signal_action_log_status
    ON knowledge_signal_action_log(db_id, status);

INSERT INTO schema_version (version) VALUES (20);
