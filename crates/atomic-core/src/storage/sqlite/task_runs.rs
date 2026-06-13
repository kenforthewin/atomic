//! SQLite storage for the `task_runs` execution ledger.
//!
//! All writers are conditional: terminal transitions and lease refreshes
//! include a `state` predicate so a stale reclaimer can't overwrite a row
//! that has already moved on. Callers branch on the returned `bool` to
//! detect lost races. See `docs/plans/reports.md` §"Execution ledger —
//! task_runs" for the contract this implements.

use super::SqliteStorage;
use crate::error::AtomicCoreError;
use crate::models::{TaskRun, TaskRunState, TaskRunTrigger};
use crate::storage::traits::{StorageResult, TaskRunStore};
use async_trait::async_trait;
use rusqlite::{params, params_from_iter, OptionalExtension, Row, ToSql};
use std::str::FromStr;

/// Column list used by every SELECT so row ordering stays consistent with
/// [`row_to_task_run`].
const COLS: &str = "id, task_id, subject_id, state, trigger, attempts, max_attempts, \
                    lease_until, next_attempt_at, scope, result_id, last_error, \
                    started_at, finished_at, created_at, updated_at";

fn row_to_task_run(row: &Row<'_>) -> rusqlite::Result<TaskRun> {
    let state_str: String = row.get(3)?;
    let trigger_str: String = row.get(4)?;
    let scope_str: Option<String> = row.get(9)?;
    let scope = scope_str
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(9, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(TaskRun {
        id: row.get(0)?,
        task_id: row.get(1)?,
        subject_id: row.get(2)?,
        state: TaskRunState::from_str(&state_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into())
        })?,
        trigger: TaskRunTrigger::from_str(&trigger_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into())
        })?,
        attempts: row.get(5)?,
        max_attempts: row.get(6)?,
        lease_until: row.get(7)?,
        next_attempt_at: row.get(8)?,
        scope,
        result_id: row.get(10)?,
        last_error: row.get(11)?,
        started_at: row.get(12)?,
        finished_at: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

impl SqliteStorage {
    pub(crate) fn insert_task_run_sync(&self, run: &TaskRun) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let scope_json = match &run.scope {
            Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("scope serialize: {e}"))
            })?),
            None => None,
        };
        conn.execute(
            "INSERT INTO task_runs (id, task_id, subject_id, state, trigger, attempts, \
                                    max_attempts, lease_until, next_attempt_at, scope, \
                                    result_id, last_error, started_at, finished_at, \
                                    created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                run.id,
                run.task_id,
                run.subject_id,
                run.state.as_str(),
                run.trigger.as_str(),
                run.attempts,
                run.max_attempts,
                run.lease_until,
                run.next_attempt_at,
                scope_json,
                run.result_id,
                run.last_error,
                run.started_at,
                run.finished_at,
                run.created_at,
                run.updated_at,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn try_insert_task_run_sync(&self, run: &TaskRun) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let scope_json = match &run.scope {
            Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("scope serialize: {e}"))
            })?),
            None => None,
        };
        // `INSERT OR IGNORE` returns Ok(0) when any UNIQUE constraint
        // rejects the row — both the PK and the partial active-row index.
        // PK collisions on uuid v7 are vanishingly unlikely, so a 0
        // return effectively means "the active-row constraint caught a
        // duplicate."
        let changed = conn.execute(
            "INSERT OR IGNORE INTO task_runs
                (id, task_id, subject_id, state, trigger, attempts, \
                 max_attempts, lease_until, next_attempt_at, scope, \
                 result_id, last_error, started_at, finished_at, \
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                run.id,
                run.task_id,
                run.subject_id,
                run.state.as_str(),
                run.trigger.as_str(),
                run.attempts,
                run.max_attempts,
                run.lease_until,
                run.next_attempt_at,
                scope_json,
                run.result_id,
                run.last_error,
                run.started_at,
                run.finished_at,
                run.created_at,
                run.updated_at,
            ],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn get_task_run_sync(&self, id: &str) -> StorageResult<Option<TaskRun>> {
        let conn = self.db.read_conn()?;
        let sql = format!("SELECT {COLS} FROM task_runs WHERE id = ?1");
        let row = conn
            .query_row(&sql, params![id], row_to_task_run)
            .optional()?;
        Ok(row)
    }

    pub(crate) fn find_runnable_task_run_sync(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
        now: &str,
    ) -> StorageResult<Option<TaskRun>> {
        let conn = self.db.read_conn()?;
        // Subject predicate: an explicit `Some(...)` filters; `None` matches
        // only rows where `subject_id IS NULL` so we don't accidentally
        // return another subject's row for tasks that do use subjects.
        let (subject_pred, subject_bind): (&str, Option<&str>) = match subject_id {
            Some(s) => ("subject_id = ?", Some(s)),
            None => ("subject_id IS NULL", None),
        };
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE task_id = ?
               AND {subject_pred}
               AND (
                    (state = 'pending' AND next_attempt_at <= ?)
                 OR (state = 'running' AND lease_until IS NOT NULL AND lease_until < ?)
               )
             ORDER BY next_attempt_at ASC
             LIMIT 1"
        );
        let mut binds: Vec<&dyn ToSql> = vec![&task_id];
        if let Some(s) = &subject_bind {
            binds.push(s as &dyn ToSql);
        }
        binds.push(&now);
        binds.push(&now);
        let row = conn
            .query_row(&sql, params_from_iter(binds.iter()), row_to_task_run)
            .optional()?;
        Ok(row)
    }

    /// Every runnable row for `task_id` across all subjects — the sweep
    /// query for event-triggered tasks like wiki regen. Same runnability
    /// predicate as [`Self::find_runnable_task_run_sync`], minus the
    /// subject filter and the `LIMIT 1`.
    pub(crate) fn list_runnable_task_runs_sync(
        &self,
        task_id: &str,
        now: &str,
    ) -> StorageResult<Vec<TaskRun>> {
        let conn = self.db.read_conn()?;
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE task_id = ?1
               AND (
                    (state = 'pending' AND next_attempt_at <= ?2)
                 OR (state = 'running' AND lease_until IS NOT NULL AND lease_until < ?2)
               )
             ORDER BY next_attempt_at ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![task_id, now], row_to_task_run)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Count every non-terminal row regardless of task, subject, or timing
    /// — the ledger-emptiness check (see the trait docs).
    pub(crate) fn count_active_task_runs_sync(&self) -> StorageResult<i32> {
        let conn = self.db.read_conn()?;
        conn.query_row(
            "SELECT COUNT(*) FROM task_runs WHERE state IN ('pending', 'running')",
            [],
            |row| row.get(0),
        )
        .map_err(AtomicCoreError::from)
    }

    /// Find any non-terminal row for `(task_id, subject_id)` — pending OR
    /// running — regardless of timing. Most-recent first. Used by the
    /// scheduler to detect "this task already has work in flight" before
    /// inserting a duplicate pending row.
    pub(crate) fn find_active_task_run_sync(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
    ) -> StorageResult<Option<TaskRun>> {
        let conn = self.db.read_conn()?;
        let (subject_pred, subject_bind): (&str, Option<&str>) = match subject_id {
            Some(s) => ("subject_id = ?", Some(s)),
            None => ("subject_id IS NULL", None),
        };
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE task_id = ?
               AND {subject_pred}
               AND state IN ('pending', 'running')
             ORDER BY created_at DESC
             LIMIT 1"
        );
        let mut binds: Vec<&dyn ToSql> = vec![&task_id];
        if let Some(s) = &subject_bind {
            binds.push(s as &dyn ToSql);
        }
        let row = conn
            .query_row(&sql, params_from_iter(binds.iter()), row_to_task_run)
            .optional()?;
        Ok(row)
    }

    pub(crate) fn claim_pending_task_run_sync(
        &self,
        id: &str,
        now: &str,
        lease_until: &str,
    ) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let changed = conn.execute(
            "UPDATE task_runs
                SET state       = 'running',
                    started_at  = ?2,
                    lease_until = ?3,
                    attempts    = attempts + 1,
                    updated_at  = ?2
              WHERE id = ?1 AND state = 'pending'
                AND next_attempt_at <= ?2",
            params![id, now, lease_until],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn reclaim_expired_task_run_sync(
        &self,
        id: &str,
        now: &str,
        lease_until: &str,
    ) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let changed = conn.execute(
            "UPDATE task_runs
                SET started_at  = ?2,
                    lease_until = ?3,
                    updated_at  = ?2
              WHERE id = ?1
                AND state = 'running'
                AND lease_until IS NOT NULL
                AND lease_until < ?2",
            params![id, now, lease_until],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn heartbeat_task_run_sync(
        &self,
        id: &str,
        expected_lease: &str,
        new_lease_until: &str,
    ) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        // The `lease_until = ?3` fence stops a slow worker from extending a
        // lease that has already been reclaimed by a peer. The peer
        // replaced our lease_until with its own value; our refresh no
        // longer matches and we lose the race cleanly.
        let changed = conn.execute(
            "UPDATE task_runs
                SET lease_until = ?2,
                    updated_at  = ?2
              WHERE id = ?1 AND state = 'running' AND lease_until = ?3",
            params![id, new_lease_until, expected_lease],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn complete_task_run_sync(
        &self,
        id: &str,
        expected_lease: &str,
        result_id: Option<&str>,
        finished_at: &str,
    ) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        // Same lease fence as `heartbeat_task_run_sync` — a stale worker
        // whose lease was reclaimed mid-execution must not be able to
        // mark a re-attempted run as succeeded under the peer's feet.
        let changed = conn.execute(
            "UPDATE task_runs
                SET state       = 'succeeded',
                    result_id   = ?3,
                    finished_at = ?2,
                    lease_until = NULL,
                    last_error  = NULL,
                    updated_at  = ?2
              WHERE id = ?1 AND state = 'running' AND lease_until = ?4",
            params![id, finished_at, result_id, expected_lease],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn fail_task_run_retry_sync(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        now: &str,
        next_attempt_at: &str,
    ) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let changed = conn.execute(
            "UPDATE task_runs
                SET state           = 'pending',
                    last_error      = ?2,
                    next_attempt_at = ?4,
                    lease_until     = NULL,
                    started_at      = NULL,
                    updated_at      = ?3
              WHERE id = ?1 AND state = 'running' AND lease_until = ?5",
            params![id, last_error, now, next_attempt_at, expected_lease],
        )?;
        Ok(changed == 1)
    }

    /// Deferral (environmental failure): back to `pending` at the caller's
    /// horizon with the claim's attempt increment refunded — see
    /// `TaskRunStore::defer_task_run`. `MAX(attempts - 1, 0)` rather than a
    /// bare decrement so a defensive caller can never drive the counter
    /// negative.
    pub(crate) fn defer_task_run_sync(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        now: &str,
        next_attempt_at: &str,
    ) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let changed = conn.execute(
            "UPDATE task_runs
                SET state           = 'pending',
                    attempts        = MAX(attempts - 1, 0),
                    last_error      = ?2,
                    next_attempt_at = ?4,
                    lease_until     = NULL,
                    started_at      = NULL,
                    updated_at      = ?3
              WHERE id = ?1 AND state = 'running' AND lease_until = ?5",
            params![id, last_error, now, next_attempt_at, expected_lease],
        )?;
        Ok(changed == 1)
    }

    pub(crate) fn list_waiting_task_runs_sync(&self, now: &str) -> StorageResult<Vec<TaskRun>> {
        let conn = self.db.read_conn()?;
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE state = 'pending' AND next_attempt_at > ?1
             ORDER BY next_attempt_at ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([now], row_to_task_run)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn rearm_task_runs_sync(&self, ids: &[String], now: &str) -> StorageResult<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let placeholders = std::iter::repeat("?")
            .take(ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE task_runs
                SET next_attempt_at = ?1,
                    updated_at      = ?1
              WHERE state = 'pending' AND id IN ({placeholders})"
        );
        let mut binds: Vec<&dyn ToSql> = vec![&now];
        for id in ids {
            binds.push(id as &dyn ToSql);
        }
        let changed = conn.execute(&sql, params_from_iter(binds.iter()))?;
        Ok(changed as u64)
    }

    pub(crate) fn fail_task_run_abandon_sync(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        finished_at: &str,
    ) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let changed = conn.execute(
            "UPDATE task_runs
                SET state       = 'abandoned',
                    last_error  = ?2,
                    finished_at = ?3,
                    lease_until = NULL,
                    updated_at  = ?3
              WHERE id = ?1 AND state = 'running' AND lease_until = ?4",
            params![id, last_error, finished_at, expected_lease],
        )?;
        Ok(changed == 1)
    }

    /// Force-settle moot rows for a deleted subject. No lease fence and no
    /// runnability gate — see `TaskRunStore::settle_task_runs_moot` for why
    /// both are deliberately skipped. Mirrors `complete_task_run_sync`'s
    /// column writes (succeeded, lease and error cleared) so settled rows
    /// read like any other success in history.
    pub(crate) fn settle_task_runs_moot_sync(
        &self,
        task_id: &str,
        subject_id: &str,
        finished_at: &str,
    ) -> StorageResult<u64> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let changed = conn.execute(
            "UPDATE task_runs
                SET state       = 'succeeded',
                    finished_at = ?3,
                    lease_until = NULL,
                    last_error  = NULL,
                    updated_at  = ?3
              WHERE task_id = ?1
                AND subject_id = ?2
                AND state IN ('pending', 'running')",
            params![task_id, subject_id, finished_at],
        )?;
        Ok(changed as u64)
    }

    pub(crate) fn list_recent_task_runs_sync(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
        limit: i32,
    ) -> StorageResult<Vec<TaskRun>> {
        let conn = self.db.read_conn()?;
        // `None` here means "any subject" (history-by-task), not "subject IS
        // NULL" — that read shape matches what UI history views want.
        let (subject_pred, subject_bind): (&str, Option<&str>) = match subject_id {
            Some(s) => ("AND subject_id = ?", Some(s)),
            None => ("", None),
        };
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE task_id = ?
             {subject_pred}
             ORDER BY created_at DESC
             LIMIT ?"
        );
        let mut binds: Vec<&dyn ToSql> = vec![&task_id];
        if let Some(s) = &subject_bind {
            binds.push(s as &dyn ToSql);
        }
        binds.push(&limit);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(binds.iter()), row_to_task_run)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One bounded retention-GC batch. See `TaskRunStore::gc_task_runs`
    /// for the eligibility contract; the window functions rank terminal
    /// rows newest-first per `(task_id, subject_id)` group (NULL subjects
    /// partition together, matching the singleton-task grain), and the
    /// `protected_failure` CTE carves out the most recent failure per
    /// group while it's younger than `failed_cutoff`. The `id` tiebreakers
    /// make ranking deterministic when timestamps collide.
    pub(crate) fn gc_task_runs_sync(
        &self,
        keep_per_subject: i32,
        age_cutoff: &str,
        failed_cutoff: &str,
        batch_size: i32,
    ) -> StorageResult<u64> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let deleted = conn.execute(
            "WITH terminal AS (
                 SELECT id, created_at,
                        ROW_NUMBER() OVER (
                            PARTITION BY task_id, subject_id
                            ORDER BY created_at DESC, id DESC
                        ) AS recency_rank
                   FROM task_runs
                  WHERE state IN ('succeeded', 'failed', 'abandoned')
             ),
             protected_failure AS (
                 SELECT id
                   FROM (
                       SELECT id, created_at,
                              ROW_NUMBER() OVER (
                                  PARTITION BY task_id, subject_id
                                  ORDER BY created_at DESC, id DESC
                              ) AS failure_rank
                         FROM task_runs
                        WHERE state IN ('failed', 'abandoned')
                   ) f
                  WHERE failure_rank = 1 AND created_at >= ?3
             )
             DELETE FROM task_runs
              WHERE id IN (
                  SELECT t.id
                    FROM terminal t
                   WHERE (t.recency_rank > ?1 OR t.created_at < ?2)
                     AND t.id NOT IN (SELECT id FROM protected_failure)
                   ORDER BY t.created_at ASC, t.id ASC
                   LIMIT ?4
              )",
            params![keep_per_subject, age_cutoff, failed_cutoff, batch_size],
        )?;
        Ok(deleted as u64)
    }
}

#[async_trait]
impl TaskRunStore for SqliteStorage {
    async fn insert_task_run(&self, run: &TaskRun) -> StorageResult<()> {
        let storage = self.clone();
        let run = run.clone();
        tokio::task::spawn_blocking(move || storage.insert_task_run_sync(&run))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn try_insert_task_run(&self, run: &TaskRun) -> StorageResult<bool> {
        let storage = self.clone();
        let run = run.clone();
        tokio::task::spawn_blocking(move || storage.try_insert_task_run_sync(&run))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_task_run(&self, id: &str) -> StorageResult<Option<TaskRun>> {
        let storage = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || storage.get_task_run_sync(&id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn find_runnable_task_run(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
        now: &str,
    ) -> StorageResult<Option<TaskRun>> {
        let storage = self.clone();
        let task_id = task_id.to_string();
        let subject_id = subject_id.map(|s| s.to_string());
        let now = now.to_string();
        tokio::task::spawn_blocking(move || {
            storage.find_runnable_task_run_sync(&task_id, subject_id.as_deref(), &now)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_runnable_task_runs(
        &self,
        task_id: &str,
        now: &str,
    ) -> StorageResult<Vec<TaskRun>> {
        let storage = self.clone();
        let task_id = task_id.to_string();
        let now = now.to_string();
        tokio::task::spawn_blocking(move || storage.list_runnable_task_runs_sync(&task_id, &now))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn count_active_task_runs(&self) -> StorageResult<i32> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.count_active_task_runs_sync())
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn find_active_task_run(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
    ) -> StorageResult<Option<TaskRun>> {
        let storage = self.clone();
        let task_id = task_id.to_string();
        let subject_id = subject_id.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            storage.find_active_task_run_sync(&task_id, subject_id.as_deref())
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn claim_pending_task_run(
        &self,
        id: &str,
        now: &str,
        lease_until: &str,
    ) -> StorageResult<bool> {
        let storage = self.clone();
        let id = id.to_string();
        let now = now.to_string();
        let lease_until = lease_until.to_string();
        tokio::task::spawn_blocking(move || {
            storage.claim_pending_task_run_sync(&id, &now, &lease_until)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn reclaim_expired_task_run(
        &self,
        id: &str,
        now: &str,
        lease_until: &str,
    ) -> StorageResult<bool> {
        let storage = self.clone();
        let id = id.to_string();
        let now = now.to_string();
        let lease_until = lease_until.to_string();
        tokio::task::spawn_blocking(move || {
            storage.reclaim_expired_task_run_sync(&id, &now, &lease_until)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn heartbeat_task_run(
        &self,
        id: &str,
        expected_lease: &str,
        new_lease_until: &str,
    ) -> StorageResult<bool> {
        let storage = self.clone();
        let id = id.to_string();
        let expected_lease = expected_lease.to_string();
        let new_lease_until = new_lease_until.to_string();
        tokio::task::spawn_blocking(move || {
            storage.heartbeat_task_run_sync(&id, &expected_lease, &new_lease_until)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn complete_task_run(
        &self,
        id: &str,
        expected_lease: &str,
        result_id: Option<&str>,
        finished_at: &str,
    ) -> StorageResult<bool> {
        let storage = self.clone();
        let id = id.to_string();
        let expected_lease = expected_lease.to_string();
        let result_id = result_id.map(|s| s.to_string());
        let finished_at = finished_at.to_string();
        tokio::task::spawn_blocking(move || {
            storage.complete_task_run_sync(&id, &expected_lease, result_id.as_deref(), &finished_at)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn fail_task_run_retry(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        now: &str,
        next_attempt_at: &str,
    ) -> StorageResult<bool> {
        let storage = self.clone();
        let id = id.to_string();
        let expected_lease = expected_lease.to_string();
        let last_error = last_error.to_string();
        let now = now.to_string();
        let next_attempt_at = next_attempt_at.to_string();
        tokio::task::spawn_blocking(move || {
            storage.fail_task_run_retry_sync(
                &id,
                &expected_lease,
                &last_error,
                &now,
                &next_attempt_at,
            )
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn fail_task_run_abandon(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        finished_at: &str,
    ) -> StorageResult<bool> {
        let storage = self.clone();
        let id = id.to_string();
        let expected_lease = expected_lease.to_string();
        let last_error = last_error.to_string();
        let finished_at = finished_at.to_string();
        tokio::task::spawn_blocking(move || {
            storage.fail_task_run_abandon_sync(&id, &expected_lease, &last_error, &finished_at)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn defer_task_run(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        now: &str,
        next_attempt_at: &str,
    ) -> StorageResult<bool> {
        let storage = self.clone();
        let id = id.to_string();
        let expected_lease = expected_lease.to_string();
        let last_error = last_error.to_string();
        let now = now.to_string();
        let next_attempt_at = next_attempt_at.to_string();
        tokio::task::spawn_blocking(move || {
            storage.defer_task_run_sync(&id, &expected_lease, &last_error, &now, &next_attempt_at)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_waiting_task_runs(&self, now: &str) -> StorageResult<Vec<TaskRun>> {
        let storage = self.clone();
        let now = now.to_string();
        tokio::task::spawn_blocking(move || storage.list_waiting_task_runs_sync(&now))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn rearm_task_runs(&self, ids: &[String], now: &str) -> StorageResult<u64> {
        let storage = self.clone();
        let ids = ids.to_vec();
        let now = now.to_string();
        tokio::task::spawn_blocking(move || storage.rearm_task_runs_sync(&ids, &now))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn settle_task_runs_moot(
        &self,
        task_id: &str,
        subject_id: &str,
        finished_at: &str,
    ) -> StorageResult<u64> {
        let storage = self.clone();
        let task_id = task_id.to_string();
        let subject_id = subject_id.to_string();
        let finished_at = finished_at.to_string();
        tokio::task::spawn_blocking(move || {
            storage.settle_task_runs_moot_sync(&task_id, &subject_id, &finished_at)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_recent_task_runs(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
        limit: i32,
    ) -> StorageResult<Vec<TaskRun>> {
        let storage = self.clone();
        let task_id = task_id.to_string();
        let subject_id = subject_id.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            storage.list_recent_task_runs_sync(&task_id, subject_id.as_deref(), limit)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn gc_task_runs(
        &self,
        keep_per_subject: i32,
        age_cutoff: &str,
        failed_cutoff: &str,
        batch_size: i32,
    ) -> StorageResult<u64> {
        let storage = self.clone();
        let age_cutoff = age_cutoff.to_string();
        let failed_cutoff = failed_cutoff.to_string();
        tokio::task::spawn_blocking(move || {
            storage.gc_task_runs_sync(keep_per_subject, &age_cutoff, &failed_cutoff, batch_size)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }
}
