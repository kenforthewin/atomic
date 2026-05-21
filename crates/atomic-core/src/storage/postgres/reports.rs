//! Postgres storage for reports, finding provenance, and citations.
//!
//! Mirrors `sqlite/reports.rs` row-for-row but binds `db_id` everywhere so
//! multiple logical databases sharing one Postgres pool stay isolated.
//! The transactional finding-write helper uses one sqlx `Transaction` and
//! commits at the end so a partial write rolls back cleanly.

use super::PostgresStorage;
use crate::error::AtomicCoreError;
use crate::models::{
    AtomKind, AtomWithTags, CitationPolicy, ContextScopeMode, ContextScopeWindow,
    CreateReportRequest, Report, ReportFinding, ReportFindingCitation, SourceScopeWindow,
    UpdateReportRequest,
};
use crate::storage::traits::{AtomStore, ReportStore, StorageResult};
use crate::CreateAtomRequest;
use async_trait::async_trait;
use sqlx::Row;
use std::str::FromStr;

const COLS: &str = "id, name, description, research_prompt, \
                    source_scope_tag_ids, source_scope_window, source_include_kinds, \
                    context_scope_mode, context_scope_tag_ids, context_scope_window, \
                    context_include_kinds, citation_policy, \
                    max_source_atoms, max_source_tokens, max_tool_iterations, \
                    schedule, schedule_tz, enabled, output_atom_tags, \
                    last_run_at, last_finding_atom_id, last_error, \
                    created_at, updated_at";

fn db_op(msg: impl Into<String>) -> AtomicCoreError {
    AtomicCoreError::DatabaseOperation(msg.into())
}

fn parse_json_string_array(s: &str) -> Result<Vec<String>, AtomicCoreError> {
    serde_json::from_str(s).map_err(|e| db_op(format!("invalid JSON string array: {e}")))
}

fn parse_kind_array(s: &str) -> Result<Vec<AtomKind>, AtomicCoreError> {
    let raw: Vec<String> =
        serde_json::from_str(s).map_err(|e| db_op(format!("invalid JSON kind array: {e}")))?;
    raw.iter()
        .map(|k| AtomKind::from_str(k).map_err(db_op))
        .collect()
}

fn dump_string_array(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

fn dump_kind_array(v: &[AtomKind]) -> String {
    let raw: Vec<&'static str> = v.iter().map(|k| k.as_str()).collect();
    serde_json::to_string(&raw).unwrap_or_else(|_| "[\"captured\"]".to_string())
}

fn row_to_report(row: &sqlx::postgres::PgRow) -> Result<Report, AtomicCoreError> {
    let get_str = |name: &str| -> Result<String, AtomicCoreError> {
        row.try_get(name).map_err(|e| db_op(e.to_string()))
    };
    let get_opt_str = |name: &str| -> Result<Option<String>, AtomicCoreError> {
        row.try_get(name).map_err(|e| db_op(e.to_string()))
    };
    let get_opt_i32 = |name: &str| -> Result<Option<i32>, AtomicCoreError> {
        row.try_get(name).map_err(|e| db_op(e.to_string()))
    };

    let enabled_raw: i32 = row.try_get("enabled").map_err(|e| db_op(e.to_string()))?;
    Ok(Report {
        id: get_str("id")?,
        name: get_str("name")?,
        description: get_opt_str("description")?,
        research_prompt: get_str("research_prompt")?,
        source_scope_tag_ids: parse_json_string_array(&get_str("source_scope_tag_ids")?)?,
        source_scope_window: get_opt_str("source_scope_window")?
            .as_deref()
            .map(SourceScopeWindow::from_storage_str)
            .transpose()
            .map_err(db_op)?,
        source_include_kinds: parse_kind_array(&get_str("source_include_kinds")?)?,
        context_scope_mode: ContextScopeMode::from_str(&get_str("context_scope_mode")?)
            .map_err(db_op)?,
        context_scope_tag_ids: parse_json_string_array(&get_str("context_scope_tag_ids")?)?,
        context_scope_window: get_opt_str("context_scope_window")?
            .as_deref()
            .map(ContextScopeWindow::from_storage_str)
            .transpose()
            .map_err(db_op)?,
        context_include_kinds: parse_kind_array(&get_str("context_include_kinds")?)?,
        citation_policy: CitationPolicy::from_str(&get_str("citation_policy")?).map_err(db_op)?,
        max_source_atoms: get_opt_i32("max_source_atoms")?,
        max_source_tokens: get_opt_i32("max_source_tokens")?,
        max_tool_iterations: get_opt_i32("max_tool_iterations")?,
        schedule: get_str("schedule")?,
        schedule_tz: get_opt_str("schedule_tz")?,
        enabled: enabled_raw != 0,
        output_atom_tags: parse_json_string_array(&get_str("output_atom_tags")?)?,
        last_run_at: get_opt_str("last_run_at")?,
        last_finding_atom_id: get_opt_str("last_finding_atom_id")?,
        last_error: get_opt_str("last_error")?,
        created_at: get_str("created_at")?,
        updated_at: get_str("updated_at")?,
    })
}

fn row_to_finding(row: &sqlx::postgres::PgRow) -> Result<ReportFinding, AtomicCoreError> {
    let get_str = |name: &str| -> Result<String, AtomicCoreError> {
        row.try_get(name).map_err(|e| db_op(e.to_string()))
    };
    let get_opt_str = |name: &str| -> Result<Option<String>, AtomicCoreError> {
        row.try_get(name).map_err(|e| db_op(e.to_string()))
    };
    Ok(ReportFinding {
        finding_atom_id: get_str("finding_atom_id")?,
        report_id: get_opt_str("report_id")?,
        run_id: get_opt_str("run_id")?,
        report_name_snapshot: get_str("report_name_snapshot")?,
        created_at: get_str("created_at")?,
    })
}

#[async_trait]
impl ReportStore for PostgresStorage {
    async fn list_reports(&self) -> StorageResult<Vec<Report>> {
        let sql = format!("SELECT {COLS} FROM reports WHERE db_id = $1 ORDER BY updated_at DESC");
        let rows = sqlx::query(&sql)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        rows.iter().map(row_to_report).collect()
    }

    async fn list_enabled_reports(&self) -> StorageResult<Vec<Report>> {
        let sql = format!(
            "SELECT {COLS} FROM reports WHERE db_id = $1 AND enabled = 1 \
             ORDER BY last_run_at ASC NULLS FIRST"
        );
        let rows = sqlx::query(&sql)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        rows.iter().map(row_to_report).collect()
    }

    async fn get_report(&self, id: &str) -> StorageResult<Option<Report>> {
        let sql = format!("SELECT {COLS} FROM reports WHERE id = $1 AND db_id = $2");
        let row = sqlx::query(&sql)
            .bind(id)
            .bind(&self.db_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        row.as_ref().map(row_to_report).transpose()
    }

    async fn insert_report(&self, request: &CreateReportRequest) -> StorageResult<Report> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO reports (
                id, name, description, research_prompt,
                source_scope_tag_ids, source_scope_window, source_include_kinds,
                context_scope_mode, context_scope_tag_ids, context_scope_window,
                context_include_kinds, citation_policy,
                max_source_atoms, max_source_tokens, max_tool_iterations,
                schedule, schedule_tz, enabled, output_atom_tags,
                created_at, updated_at, db_id
             ) VALUES (
                $1, $2, $3, $4,
                $5, $6, $7,
                $8, $9, $10,
                $11, $12,
                $13, $14, $15,
                $16, $17, $18, $19,
                $20, $20, $21
             )",
        )
        .bind(&id)
        .bind(&request.name)
        .bind(&request.description)
        .bind(&request.research_prompt)
        .bind(dump_string_array(&request.source_scope_tag_ids))
        .bind(
            request
                .source_scope_window
                .as_ref()
                .map(|w| w.to_storage_str()),
        )
        .bind(dump_kind_array(&request.source_include_kinds))
        .bind(request.context_scope_mode.as_str())
        .bind(dump_string_array(&request.context_scope_tag_ids))
        .bind(
            request
                .context_scope_window
                .as_ref()
                .map(|w| w.to_storage_str()),
        )
        .bind(dump_kind_array(&request.context_include_kinds))
        .bind(request.citation_policy.as_str())
        .bind(request.max_source_atoms)
        .bind(request.max_source_tokens)
        .bind(request.max_tool_iterations)
        .bind(&request.schedule)
        .bind(&request.schedule_tz)
        .bind(if request.enabled { 1 } else { 0 })
        .bind(dump_string_array(&request.output_atom_tags))
        .bind(&now)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| db_op(e.to_string()))?;
        self.get_report(&id)
            .await?
            .ok_or_else(|| db_op("Report vanished after insert"))
    }

    async fn update_report(
        &self,
        id: &str,
        request: &UpdateReportRequest,
    ) -> StorageResult<Report> {
        // Read-modify-write same as the SQLite path so partial-update
        // semantics stay identical across backends.
        let mut existing = self
            .get_report(id)
            .await?
            .ok_or_else(|| db_op(format!("Report {id} not found")))?;
        if let Some(v) = &request.name {
            existing.name = v.clone();
        }
        if let Some(v) = &request.description {
            existing.description = v.clone();
        }
        if let Some(v) = &request.research_prompt {
            existing.research_prompt = v.clone();
        }
        if let Some(v) = &request.source_scope_tag_ids {
            existing.source_scope_tag_ids = v.clone();
        }
        if let Some(v) = &request.source_scope_window {
            existing.source_scope_window = v.clone();
        }
        if let Some(v) = &request.source_include_kinds {
            existing.source_include_kinds = v.clone();
        }
        if let Some(v) = request.context_scope_mode {
            existing.context_scope_mode = v;
        }
        if let Some(v) = &request.context_scope_tag_ids {
            existing.context_scope_tag_ids = v.clone();
        }
        if let Some(v) = &request.context_scope_window {
            existing.context_scope_window = v.clone();
        }
        if let Some(v) = &request.context_include_kinds {
            existing.context_include_kinds = v.clone();
        }
        if let Some(v) = request.citation_policy {
            existing.citation_policy = v;
        }
        if let Some(v) = request.max_source_atoms {
            existing.max_source_atoms = v;
        }
        if let Some(v) = request.max_source_tokens {
            existing.max_source_tokens = v;
        }
        if let Some(v) = request.max_tool_iterations {
            existing.max_tool_iterations = v;
        }
        if let Some(v) = &request.schedule {
            existing.schedule = v.clone();
        }
        if let Some(v) = &request.schedule_tz {
            existing.schedule_tz = v.clone();
        }
        if let Some(v) = request.enabled {
            existing.enabled = v;
        }
        if let Some(v) = &request.output_atom_tags {
            existing.output_atom_tags = v.clone();
        }
        existing.updated_at = chrono::Utc::now().to_rfc3339();

        sqlx::query(
            "UPDATE reports SET
                name = $2, description = $3, research_prompt = $4,
                source_scope_tag_ids = $5, source_scope_window = $6, source_include_kinds = $7,
                context_scope_mode = $8, context_scope_tag_ids = $9, context_scope_window = $10,
                context_include_kinds = $11, citation_policy = $12,
                max_source_atoms = $13, max_source_tokens = $14, max_tool_iterations = $15,
                schedule = $16, schedule_tz = $17, enabled = $18, output_atom_tags = $19,
                updated_at = $20
              WHERE id = $1 AND db_id = $21",
        )
        .bind(id)
        .bind(&existing.name)
        .bind(&existing.description)
        .bind(&existing.research_prompt)
        .bind(dump_string_array(&existing.source_scope_tag_ids))
        .bind(
            existing
                .source_scope_window
                .as_ref()
                .map(|w| w.to_storage_str()),
        )
        .bind(dump_kind_array(&existing.source_include_kinds))
        .bind(existing.context_scope_mode.as_str())
        .bind(dump_string_array(&existing.context_scope_tag_ids))
        .bind(
            existing
                .context_scope_window
                .as_ref()
                .map(|w| w.to_storage_str()),
        )
        .bind(dump_kind_array(&existing.context_include_kinds))
        .bind(existing.citation_policy.as_str())
        .bind(existing.max_source_atoms)
        .bind(existing.max_source_tokens)
        .bind(existing.max_tool_iterations)
        .bind(&existing.schedule)
        .bind(&existing.schedule_tz)
        .bind(if existing.enabled { 1 } else { 0 })
        .bind(dump_string_array(&existing.output_atom_tags))
        .bind(&existing.updated_at)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| db_op(e.to_string()))?;
        Ok(existing)
    }

    async fn set_report_enabled(&self, id: &str, enabled: bool) -> StorageResult<()> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE reports SET enabled = $2, updated_at = $3 WHERE id = $1 AND db_id = $4",
        )
        .bind(id)
        .bind(if enabled { 1 } else { 0 })
        .bind(now)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| db_op(e.to_string()))?;
        Ok(())
    }

    async fn delete_report(&self, id: &str) -> StorageResult<()> {
        sqlx::query("DELETE FROM reports WHERE id = $1 AND db_id = $2")
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        Ok(())
    }

    async fn update_report_cache(
        &self,
        id: &str,
        last_run_at: Option<&str>,
        last_finding_atom_id: Option<Option<&str>>,
        last_error: Option<Option<&str>>,
    ) -> StorageResult<()> {
        let now = chrono::Utc::now().to_rfc3339();
        if let Some(run_at) = last_run_at {
            sqlx::query(
                "UPDATE reports SET last_run_at = $2, updated_at = $3 WHERE id = $1 AND db_id = $4",
            )
            .bind(id)
            .bind(run_at)
            .bind(&now)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        }
        if let Some(finding) = last_finding_atom_id {
            sqlx::query(
                "UPDATE reports SET last_finding_atom_id = $2, updated_at = $3 WHERE id = $1 AND db_id = $4",
            )
            .bind(id)
            .bind(finding)
            .bind(&now)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        }
        if let Some(err) = last_error {
            sqlx::query(
                "UPDATE reports SET last_error = $2, updated_at = $3 WHERE id = $1 AND db_id = $4",
            )
            .bind(id)
            .bind(err)
            .bind(&now)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        }
        Ok(())
    }

    async fn list_findings_for_report(
        &self,
        report_id: &str,
        limit: i32,
    ) -> StorageResult<Vec<(ReportFinding, AtomWithTags)>> {
        let rows = sqlx::query(
            "SELECT finding_atom_id, report_id, run_id, report_name_snapshot, created_at
             FROM report_findings
             WHERE report_id = $1 AND db_id = $2
             ORDER BY created_at DESC
             LIMIT $3",
        )
        .bind(report_id)
        .bind(&self.db_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| db_op(e.to_string()))?;
        let findings: Vec<ReportFinding> = rows
            .iter()
            .map(row_to_finding)
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(findings.len());
        for f in findings {
            if let Some(atom) = self.get_atom(&f.finding_atom_id).await? {
                out.push((f, atom));
            }
        }
        Ok(out)
    }

    async fn get_finding_provenance(
        &self,
        finding_atom_id: &str,
    ) -> StorageResult<Option<ReportFinding>> {
        let row = sqlx::query(
            "SELECT finding_atom_id, report_id, run_id, report_name_snapshot, created_at
             FROM report_findings WHERE finding_atom_id = $1 AND db_id = $2",
        )
        .bind(finding_atom_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| db_op(e.to_string()))?;
        row.as_ref().map(row_to_finding).transpose()
    }

    async fn list_finding_atom_ids_for_report(
        &self,
        report_id: &str,
    ) -> StorageResult<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT finding_atom_id FROM report_findings WHERE report_id = $1 AND db_id = $2",
        )
        .bind(report_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| db_op(e.to_string()))?;
        Ok(rows.into_iter().map(|(s,)| s).collect())
    }

    async fn write_finding_transactionally(
        &self,
        atom_request: &CreateAtomRequest,
        atom_id: &str,
        atom_created_at: &str,
        provenance: &ReportFinding,
        citations: &[ReportFindingCitation],
    ) -> StorageResult<AtomWithTags> {
        use crate::{extract_title_and_snippet, parse_source};
        let (title, snippet) = extract_title_and_snippet(&atom_request.content, 300);
        let source = atom_request.source_url.as_deref().map(parse_source);

        let mut tx = self.pool.begin().await.map_err(|e| db_op(e.to_string()))?;

        // `tagging_status = 'skipped'` keeps auto-tagging off finding
        // atoms (they're already stamped with the report's deterministic
        // output_atom_tags). Matches the SQLite twin.
        sqlx::query(
            "INSERT INTO atoms
                (id, content, source_url, source, published_at, created_at, updated_at,
                 embedding_status, tagging_status, title, snippet, kind, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $6, 'pending', 'skipped', $7, $8, 'report', $9)",
        )
        .bind(atom_id)
        .bind(&atom_request.content)
        .bind(&atom_request.source_url)
        .bind(&source)
        .bind(&atom_request.published_at)
        .bind(atom_created_at)
        .bind(&title)
        .bind(&snippet)
        .bind(&self.db_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_op(e.to_string()))?;

        for tag_id in &atom_request.tag_ids {
            sqlx::query(
                "INSERT INTO atom_tags (atom_id, tag_id, source, db_id)
                 VALUES ($1, $2, 'manual', $3)",
            )
            .bind(atom_id)
            .bind(tag_id)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        }

        sqlx::query(
            "INSERT INTO report_findings
                (finding_atom_id, report_id, run_id, report_name_snapshot, created_at, db_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&provenance.finding_atom_id)
        .bind(&provenance.report_id)
        .bind(&provenance.run_id)
        .bind(&provenance.report_name_snapshot)
        .bind(&provenance.created_at)
        .bind(&self.db_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| db_op(e.to_string()))?;

        for c in citations {
            sqlx::query(
                "INSERT INTO report_finding_citations
                    (finding_atom_id, cited_atom_id, position, excerpt, db_id)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&c.finding_atom_id)
            .bind(&c.cited_atom_id)
            .bind(c.position)
            .bind(&c.excerpt)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_op(e.to_string()))?;
        }

        tx.commit().await.map_err(|e| db_op(e.to_string()))?;

        self.get_atom(atom_id)
            .await?
            .ok_or_else(|| db_op(format!("finding atom {atom_id} vanished after write")))
    }
}
