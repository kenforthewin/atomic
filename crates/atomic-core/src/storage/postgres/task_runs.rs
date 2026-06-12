//! Postgres storage for the `task_runs` execution ledger.
//!
//! Mirrors `sqlite/task_runs.rs` row-for-row but binds `db_id` everywhere so
//! multiple logical databases sharing one Postgres pool can run their own
//! schedulers without seeing each other's runs. The conditional-update
//! predicate `RETURNING id` pattern is the Postgres equivalent of SQLite's
//! `changes() == 1` check.

use super::retry::with_retry;
use super::PostgresStorage;
use crate::error::AtomicCoreError;
use crate::models::{TaskRun, TaskRunState, TaskRunTrigger};
use crate::storage::traits::{StorageResult, TaskRunStore};
use async_trait::async_trait;
use sqlx::Row;
use std::str::FromStr;

const COLS: &str = "id, task_id, subject_id, state, trigger, attempts, max_attempts, \
                    lease_until, next_attempt_at, scope, result_id, last_error, \
                    started_at, finished_at, created_at, updated_at";

fn row_to_task_run(row: &sqlx::postgres::PgRow) -> Result<TaskRun, AtomicCoreError> {
    let state_str: String = row
        .try_get("state")
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    let trigger_str: String = row
        .try_get("trigger")
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    let scope_str: Option<String> = row
        .try_get("scope")
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    let scope = scope_str
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| AtomicCoreError::DatabaseOperation(format!("scope deserialize: {e}")))?;
    Ok(TaskRun {
        id: row
            .try_get("id")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        task_id: row
            .try_get("task_id")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        subject_id: row
            .try_get("subject_id")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        state: TaskRunState::from_str(&state_str).map_err(AtomicCoreError::DatabaseOperation)?,
        trigger: TaskRunTrigger::from_str(&trigger_str)
            .map_err(AtomicCoreError::DatabaseOperation)?,
        attempts: row
            .try_get("attempts")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        max_attempts: row
            .try_get("max_attempts")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        lease_until: row
            .try_get("lease_until")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        next_attempt_at: row
            .try_get("next_attempt_at")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        scope,
        result_id: row
            .try_get("result_id")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        last_error: row
            .try_get("last_error")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        started_at: row
            .try_get("started_at")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        created_at: row
            .try_get("created_at")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
        updated_at: row
            .try_get("updated_at")
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?,
    })
}

#[async_trait]
impl TaskRunStore for PostgresStorage {
    async fn insert_task_run(&self, run: &TaskRun) -> StorageResult<()> {
        let scope_json = match &run.scope {
            Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("scope serialize: {e}"))
            })?),
            None => None,
        };
        sqlx::query(
            "INSERT INTO task_runs (id, task_id, subject_id, state, trigger, attempts, \
                                    max_attempts, lease_until, next_attempt_at, scope, \
                                    result_id, last_error, started_at, finished_at, \
                                    created_at, updated_at, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(&run.id)
        .bind(&run.task_id)
        .bind(&run.subject_id)
        .bind(run.state.as_str())
        .bind(run.trigger.as_str())
        .bind(run.attempts)
        .bind(run.max_attempts)
        .bind(&run.lease_until)
        .bind(&run.next_attempt_at)
        .bind(scope_json)
        .bind(&run.result_id)
        .bind(&run.last_error)
        .bind(&run.started_at)
        .bind(&run.finished_at)
        .bind(&run.created_at)
        .bind(&run.updated_at)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    async fn try_insert_task_run(&self, run: &TaskRun) -> StorageResult<bool> {
        let scope_json = match &run.scope {
            Some(v) => Some(serde_json::to_string(v).map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("scope serialize: {e}"))
            })?),
            None => None,
        };
        // `ON CONFLICT DO NOTHING` without a column tuple catches any
        // UNIQUE violation — both the PK and the partial active-row
        // index. Returns 0 affected rows when a duplicate active row
        // already exists.
        let result = sqlx::query(
            "INSERT INTO task_runs (id, task_id, subject_id, state, trigger, attempts, \
                                    max_attempts, lease_until, next_attempt_at, scope, \
                                    result_id, last_error, started_at, finished_at, \
                                    created_at, updated_at, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
             ON CONFLICT DO NOTHING",
        )
        .bind(&run.id)
        .bind(&run.task_id)
        .bind(&run.subject_id)
        .bind(run.state.as_str())
        .bind(run.trigger.as_str())
        .bind(run.attempts)
        .bind(run.max_attempts)
        .bind(&run.lease_until)
        .bind(&run.next_attempt_at)
        .bind(scope_json)
        .bind(&run.result_id)
        .bind(&run.last_error)
        .bind(&run.started_at)
        .bind(&run.finished_at)
        .bind(&run.created_at)
        .bind(&run.updated_at)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(result.rows_affected() == 1)
    }

    async fn get_task_run(&self, id: &str) -> StorageResult<Option<TaskRun>> {
        let sql = format!("SELECT {COLS} FROM task_runs WHERE id = $1 AND db_id = $2");
        let row = sqlx::query(&sql)
            .bind(id)
            .bind(&self.db_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        row.map(|r| row_to_task_run(&r)).transpose()
    }

    async fn find_runnable_task_run(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
        now: &str,
    ) -> StorageResult<Option<TaskRun>> {
        let subject_pred = if subject_id.is_some() {
            "subject_id = $5"
        } else {
            "subject_id IS NULL"
        };
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE db_id = $1
               AND task_id = $2
               AND {subject_pred}
               AND (
                    (state = 'pending' AND next_attempt_at <= $3)
                 OR (state = 'running' AND lease_until IS NOT NULL AND lease_until < $4)
               )
             ORDER BY next_attempt_at ASC
             LIMIT 1"
        );
        let mut q = sqlx::query(&sql)
            .bind(&self.db_id)
            .bind(task_id)
            .bind(now)
            .bind(now);
        if let Some(s) = subject_id {
            q = q.bind(s);
        }
        let row = q
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        row.map(|r| row_to_task_run(&r)).transpose()
    }

    async fn list_runnable_task_runs(
        &self,
        task_id: &str,
        now: &str,
    ) -> StorageResult<Vec<TaskRun>> {
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE db_id = $1
               AND task_id = $2
               AND (
                    (state = 'pending' AND next_attempt_at <= $3)
                 OR (state = 'running' AND lease_until IS NOT NULL AND lease_until < $3)
               )
             ORDER BY next_attempt_at ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(&self.db_id)
            .bind(task_id)
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        rows.iter().map(row_to_task_run).collect()
    }

    async fn count_active_task_runs(&self) -> StorageResult<i32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM task_runs
             WHERE db_id = $1 AND state IN ('pending', 'running')",
        )
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(count as i32)
    }

    async fn find_active_task_run(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
    ) -> StorageResult<Option<TaskRun>> {
        let subject_pred = if subject_id.is_some() {
            "subject_id = $3"
        } else {
            "subject_id IS NULL"
        };
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE db_id = $1
               AND task_id = $2
               AND {subject_pred}
               AND state IN ('pending', 'running')
             ORDER BY created_at DESC
             LIMIT 1"
        );
        let mut q = sqlx::query(&sql).bind(&self.db_id).bind(task_id);
        if let Some(s) = subject_id {
            q = q.bind(s);
        }
        let row = q
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        row.map(|r| row_to_task_run(&r)).transpose()
    }

    async fn claim_pending_task_run(
        &self,
        id: &str,
        now: &str,
        lease_until: &str,
    ) -> StorageResult<bool> {
        let row = with_retry(|| async {
            sqlx::query(
                "UPDATE task_runs
                    SET state       = 'running',
                        started_at  = $2,
                        lease_until = $3,
                        attempts    = attempts + 1,
                        updated_at  = $2
                  WHERE id = $1 AND state = 'pending' AND db_id = $4
                    AND next_attempt_at <= $2
                  RETURNING id",
            )
            .bind(id)
            .bind(now)
            .bind(lease_until)
            .bind(&self.db_id)
            .fetch_optional(&self.pool)
            .await
        })
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(row.is_some())
    }

    async fn reclaim_expired_task_run(
        &self,
        id: &str,
        now: &str,
        lease_until: &str,
    ) -> StorageResult<bool> {
        let row = with_retry(|| async {
            sqlx::query(
                "UPDATE task_runs
                    SET started_at  = $2,
                        lease_until = $3,
                        updated_at  = $2
                  WHERE id = $1
                    AND state = 'running'
                    AND lease_until IS NOT NULL
                    AND lease_until < $2
                    AND db_id = $4
                  RETURNING id",
            )
            .bind(id)
            .bind(now)
            .bind(lease_until)
            .bind(&self.db_id)
            .fetch_optional(&self.pool)
            .await
        })
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(row.is_some())
    }

    async fn heartbeat_task_run(
        &self,
        id: &str,
        expected_lease: &str,
        new_lease_until: &str,
    ) -> StorageResult<bool> {
        // Lease fence: a peer that reclaimed our row replaced lease_until
        // with its own value, so our refresh no longer matches.
        let row = sqlx::query(
            "UPDATE task_runs
                SET lease_until = $2,
                    updated_at  = $2
              WHERE id = $1
                AND state = 'running'
                AND lease_until = $3
                AND db_id = $4
              RETURNING id",
        )
        .bind(id)
        .bind(new_lease_until)
        .bind(expected_lease)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(row.is_some())
    }

    async fn complete_task_run(
        &self,
        id: &str,
        expected_lease: &str,
        result_id: Option<&str>,
        finished_at: &str,
    ) -> StorageResult<bool> {
        let row = sqlx::query(
            "UPDATE task_runs
                SET state       = 'succeeded',
                    result_id   = $3,
                    finished_at = $2,
                    lease_until = NULL,
                    last_error  = NULL,
                    updated_at  = $2
              WHERE id = $1
                AND state = 'running'
                AND lease_until = $4
                AND db_id = $5
              RETURNING id",
        )
        .bind(id)
        .bind(finished_at)
        .bind(result_id)
        .bind(expected_lease)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(row.is_some())
    }

    async fn fail_task_run_retry(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        now: &str,
        next_attempt_at: &str,
    ) -> StorageResult<bool> {
        let row = sqlx::query(
            "UPDATE task_runs
                SET state           = 'pending',
                    last_error      = $2,
                    next_attempt_at = $4,
                    lease_until     = NULL,
                    started_at      = NULL,
                    updated_at      = $3
              WHERE id = $1
                AND state = 'running'
                AND lease_until = $5
                AND db_id = $6
              RETURNING id",
        )
        .bind(id)
        .bind(last_error)
        .bind(now)
        .bind(next_attempt_at)
        .bind(expected_lease)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(row.is_some())
    }

    async fn fail_task_run_abandon(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        finished_at: &str,
    ) -> StorageResult<bool> {
        let row = sqlx::query(
            "UPDATE task_runs
                SET state       = 'abandoned',
                    last_error  = $2,
                    finished_at = $3,
                    lease_until = NULL,
                    updated_at  = $3
              WHERE id = $1
                AND state = 'running'
                AND lease_until = $4
                AND db_id = $5
              RETURNING id",
        )
        .bind(id)
        .bind(last_error)
        .bind(finished_at)
        .bind(expected_lease)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(row.is_some())
    }

    /// Force-settle moot rows for a deleted subject. No lease fence and no
    /// runnability gate — see `TaskRunStore::settle_task_runs_moot` for why
    /// both are deliberately skipped. Fenced on `db_id` like every other
    /// writer here, so deleting a feed in one logical database can't settle
    /// a sibling's runs.
    async fn settle_task_runs_moot(
        &self,
        task_id: &str,
        subject_id: &str,
        finished_at: &str,
    ) -> StorageResult<u64> {
        let result = with_retry(|| async {
            sqlx::query(
                "UPDATE task_runs
                    SET state       = 'succeeded',
                        finished_at = $3,
                        lease_until = NULL,
                        last_error  = NULL,
                        updated_at  = $3
                  WHERE task_id = $1
                    AND subject_id = $2
                    AND db_id = $4
                    AND state IN ('pending', 'running')",
            )
            .bind(task_id)
            .bind(subject_id)
            .bind(finished_at)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
        })
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(result.rows_affected())
    }

    async fn list_recent_task_runs(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
        limit: i32,
    ) -> StorageResult<Vec<TaskRun>> {
        let subject_pred = if subject_id.is_some() {
            "AND subject_id = $4"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {COLS}
             FROM task_runs
             WHERE db_id = $1 AND task_id = $2
             {subject_pred}
             ORDER BY created_at DESC
             LIMIT $3"
        );
        let mut q = sqlx::query(&sql)
            .bind(&self.db_id)
            .bind(task_id)
            .bind(limit as i64);
        if let Some(s) = subject_id {
            q = q.bind(s);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        rows.iter().map(row_to_task_run).collect()
    }

    /// One bounded retention-GC batch — the same SQL shape as the SQLite
    /// impl (see `TaskRunStore::gc_task_runs` for the eligibility
    /// contract) with every table scan fenced on `db_id` so one logical
    /// database's GC can't rank or delete a sibling's history.
    async fn gc_task_runs(
        &self,
        keep_per_subject: i32,
        age_cutoff: &str,
        failed_cutoff: &str,
        batch_size: i32,
    ) -> StorageResult<u64> {
        let result = with_retry(|| async {
            sqlx::query(
                "WITH terminal AS (
                     SELECT id, created_at,
                            ROW_NUMBER() OVER (
                                PARTITION BY task_id, subject_id
                                ORDER BY created_at DESC, id DESC
                            ) AS recency_rank
                       FROM task_runs
                      WHERE db_id = $1
                        AND state IN ('succeeded', 'failed', 'abandoned')
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
                            WHERE db_id = $1
                              AND state IN ('failed', 'abandoned')
                       ) f
                      WHERE failure_rank = 1 AND created_at >= $4
                 )
                 DELETE FROM task_runs
                  WHERE db_id = $1
                    AND id IN (
                        SELECT t.id
                          FROM terminal t
                         WHERE (t.recency_rank > $2 OR t.created_at < $3)
                           AND t.id NOT IN (SELECT id FROM protected_failure)
                         ORDER BY t.created_at ASC, t.id ASC
                         LIMIT $5
                    )",
            )
            .bind(&self.db_id)
            .bind(keep_per_subject as i64)
            .bind(age_cutoff)
            .bind(failed_cutoff)
            .bind(batch_size as i64)
            .execute(&self.pool)
            .await
        })
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(result.rows_affected())
    }
}
