-- Enforce at most one non-terminal task_runs row per (db_id, task_id,
-- subject_id) via a partial unique index. Without this, two scheduler
-- ticks finding no active row race to insert + claim, and both win —
-- driving the same report twice. `COALESCE(subject_id, '')` is needed
-- because UNIQUE treats NULL as distinct, so two rows with NULL
-- subject_id would otherwise both be "unique".
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_runs_active_unique
    ON task_runs(db_id, task_id, COALESCE(subject_id, ''))
    WHERE state IN ('pending', 'running');

INSERT INTO schema_version (version) VALUES (17);
