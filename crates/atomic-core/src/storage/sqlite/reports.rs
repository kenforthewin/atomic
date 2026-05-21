//! SQLite storage for reports, finding provenance, and citations.
//!
//! The transactional finding-write helper is the load-bearing piece here:
//! it wraps the atom insert, the atom-tags links, the `report_findings`
//! provenance row, and every `report_finding_citations` row in a single
//! transaction so a crash mid-write cannot leave an orphan finding atom
//! without its provenance, or with a partial citation map.

use super::SqliteStorage;
use crate::error::AtomicCoreError;
use crate::models::{
    AtomKind, AtomWithTags, CitationPolicy, ContextScopeMode, ContextScopeWindow,
    CreateReportRequest, Report, ReportFinding, ReportFindingCitation, SourceScopeWindow,
    UpdateReportRequest,
};
use crate::storage::traits::{ReportStore, StorageResult};
use crate::CreateAtomRequest;
use async_trait::async_trait;
use rusqlite::{params, OptionalExtension, Row};
use std::str::FromStr;

/// Column list used by every SELECT so row ordering matches [`row_to_report`].
const COLS: &str = "id, name, description, research_prompt, \
                    source_scope_tag_ids, source_scope_window, source_include_kinds, \
                    context_scope_mode, context_scope_tag_ids, context_scope_window, \
                    context_include_kinds, citation_policy, \
                    max_source_atoms, max_source_tokens, max_tool_iterations, \
                    schedule, schedule_tz, enabled, output_atom_tags, \
                    last_run_at, last_finding_atom_id, last_error, \
                    created_at, updated_at";

fn parse_json_string_array(s: &str) -> Result<Vec<String>, AtomicCoreError> {
    serde_json::from_str(s)
        .map_err(|e| AtomicCoreError::DatabaseOperation(format!("invalid JSON string array: {e}")))
}

fn parse_kind_array(s: &str) -> Result<Vec<AtomKind>, AtomicCoreError> {
    let raw: Vec<String> = serde_json::from_str(s)
        .map_err(|e| AtomicCoreError::DatabaseOperation(format!("invalid JSON kind array: {e}")))?;
    raw.iter()
        .map(|k| AtomKind::from_str(k).map_err(AtomicCoreError::DatabaseOperation))
        .collect()
}

fn dump_string_array(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

fn dump_kind_array(v: &[AtomKind]) -> String {
    let raw: Vec<&'static str> = v.iter().map(|k| k.as_str()).collect();
    serde_json::to_string(&raw).unwrap_or_else(|_| "[\"captured\"]".to_string())
}

fn row_to_report(row: &Row<'_>) -> rusqlite::Result<Report> {
    let source_window_raw: Option<String> = row.get(5)?;
    let context_window_raw: Option<String> = row.get(9)?;
    let source_scope_tag_ids_raw: String = row.get(4)?;
    let source_include_kinds_raw: String = row.get(6)?;
    let context_scope_mode_raw: String = row.get(7)?;
    let context_scope_tag_ids_raw: String = row.get(8)?;
    let context_include_kinds_raw: String = row.get(10)?;
    let citation_policy_raw: String = row.get(11)?;
    let output_tags_raw: String = row.get(18)?;
    let enabled_raw: i32 = row.get(17)?;

    let map_text_err = |col: usize, e: String| {
        rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, e.into())
    };

    Ok(Report {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        research_prompt: row.get(3)?,
        source_scope_tag_ids: parse_json_string_array(&source_scope_tag_ids_raw)
            .map_err(|e| map_text_err(4, e.to_string()))?,
        source_scope_window: source_window_raw
            .as_deref()
            .map(SourceScopeWindow::from_storage_str)
            .transpose()
            .map_err(|e| map_text_err(5, e))?,
        source_include_kinds: parse_kind_array(&source_include_kinds_raw)
            .map_err(|e| map_text_err(6, e.to_string()))?,
        context_scope_mode: ContextScopeMode::from_str(&context_scope_mode_raw)
            .map_err(|e| map_text_err(7, e))?,
        context_scope_tag_ids: parse_json_string_array(&context_scope_tag_ids_raw)
            .map_err(|e| map_text_err(8, e.to_string()))?,
        context_scope_window: context_window_raw
            .as_deref()
            .map(ContextScopeWindow::from_storage_str)
            .transpose()
            .map_err(|e| map_text_err(9, e))?,
        context_include_kinds: parse_kind_array(&context_include_kinds_raw)
            .map_err(|e| map_text_err(10, e.to_string()))?,
        citation_policy: CitationPolicy::from_str(&citation_policy_raw)
            .map_err(|e| map_text_err(11, e))?,
        max_source_atoms: row.get(12)?,
        max_source_tokens: row.get(13)?,
        max_tool_iterations: row.get(14)?,
        schedule: row.get(15)?,
        schedule_tz: row.get(16)?,
        enabled: enabled_raw != 0,
        output_atom_tags: parse_json_string_array(&output_tags_raw)
            .map_err(|e| map_text_err(18, e.to_string()))?,
        last_run_at: row.get(19)?,
        last_finding_atom_id: row.get(20)?,
        last_error: row.get(21)?,
        created_at: row.get(22)?,
        updated_at: row.get(23)?,
    })
}

fn row_to_finding(row: &Row<'_>) -> rusqlite::Result<ReportFinding> {
    Ok(ReportFinding {
        finding_atom_id: row.get(0)?,
        report_id: row.get(1)?,
        run_id: row.get(2)?,
        report_name_snapshot: row.get(3)?,
        created_at: row.get(4)?,
    })
}

impl SqliteStorage {
    pub(crate) fn list_reports_sync(&self) -> StorageResult<Vec<Report>> {
        let conn = self.db.read_conn()?;
        let sql = format!("SELECT {COLS} FROM reports ORDER BY updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], row_to_report)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn list_enabled_reports_sync(&self) -> StorageResult<Vec<Report>> {
        let conn = self.db.read_conn()?;
        let sql = format!(
            "SELECT {COLS} FROM reports WHERE enabled = 1 ORDER BY last_run_at ASC NULLS FIRST"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], row_to_report)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn get_report_sync(&self, id: &str) -> StorageResult<Option<Report>> {
        let conn = self.db.read_conn()?;
        let sql = format!("SELECT {COLS} FROM reports WHERE id = ?1");
        let row = conn
            .query_row(&sql, params![id], row_to_report)
            .optional()?;
        Ok(row)
    }

    pub(crate) fn insert_report_sync(
        &self,
        request: &CreateReportRequest,
    ) -> StorageResult<Report> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "INSERT INTO reports (
                id, name, description, research_prompt,
                source_scope_tag_ids, source_scope_window, source_include_kinds,
                context_scope_mode, context_scope_tag_ids, context_scope_window,
                context_include_kinds, citation_policy,
                max_source_atoms, max_source_tokens, max_tool_iterations,
                schedule, schedule_tz, enabled, output_atom_tags,
                last_run_at, last_finding_atom_id, last_error,
                created_at, updated_at
             ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7,
                ?8, ?9, ?10,
                ?11, ?12,
                ?13, ?14, ?15,
                ?16, ?17, ?18, ?19,
                NULL, NULL, NULL,
                ?20, ?20
             )",
            params![
                id,
                request.name,
                request.description,
                request.research_prompt,
                dump_string_array(&request.source_scope_tag_ids),
                request
                    .source_scope_window
                    .as_ref()
                    .map(|w| w.to_storage_str()),
                dump_kind_array(&request.source_include_kinds),
                request.context_scope_mode.as_str(),
                dump_string_array(&request.context_scope_tag_ids),
                request
                    .context_scope_window
                    .as_ref()
                    .map(|w| w.to_storage_str()),
                dump_kind_array(&request.context_include_kinds),
                request.citation_policy.as_str(),
                request.max_source_atoms,
                request.max_source_tokens,
                request.max_tool_iterations,
                request.schedule,
                request.schedule_tz,
                if request.enabled { 1 } else { 0 },
                dump_string_array(&request.output_atom_tags),
                now,
            ],
        )?;
        drop(conn);
        self.get_report_sync(&id)?.ok_or_else(|| {
            AtomicCoreError::DatabaseOperation("Report vanished after insert".into())
        })
    }

    pub(crate) fn update_report_sync(
        &self,
        id: &str,
        request: &UpdateReportRequest,
    ) -> StorageResult<Report> {
        // Read-modify-write so partial updates compose cleanly with the
        // typed enum fields and JSON-encoded arrays without writing a
        // dozen conditional UPDATEs.
        let mut existing = self
            .get_report_sync(id)?
            .ok_or_else(|| AtomicCoreError::DatabaseOperation(format!("Report {id} not found")))?;
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

        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "UPDATE reports SET
                name = ?2, description = ?3, research_prompt = ?4,
                source_scope_tag_ids = ?5, source_scope_window = ?6, source_include_kinds = ?7,
                context_scope_mode = ?8, context_scope_tag_ids = ?9, context_scope_window = ?10,
                context_include_kinds = ?11, citation_policy = ?12,
                max_source_atoms = ?13, max_source_tokens = ?14, max_tool_iterations = ?15,
                schedule = ?16, schedule_tz = ?17, enabled = ?18, output_atom_tags = ?19,
                updated_at = ?20
              WHERE id = ?1",
            params![
                id,
                existing.name,
                existing.description,
                existing.research_prompt,
                dump_string_array(&existing.source_scope_tag_ids),
                existing
                    .source_scope_window
                    .as_ref()
                    .map(|w| w.to_storage_str()),
                dump_kind_array(&existing.source_include_kinds),
                existing.context_scope_mode.as_str(),
                dump_string_array(&existing.context_scope_tag_ids),
                existing
                    .context_scope_window
                    .as_ref()
                    .map(|w| w.to_storage_str()),
                dump_kind_array(&existing.context_include_kinds),
                existing.citation_policy.as_str(),
                existing.max_source_atoms,
                existing.max_source_tokens,
                existing.max_tool_iterations,
                existing.schedule,
                existing.schedule_tz,
                if existing.enabled { 1 } else { 0 },
                dump_string_array(&existing.output_atom_tags),
                existing.updated_at,
            ],
        )?;
        drop(conn);
        Ok(existing)
    }

    pub(crate) fn set_report_enabled_sync(&self, id: &str, enabled: bool) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE reports SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, if enabled { 1 } else { 0 }, now],
        )?;
        Ok(())
    }

    pub(crate) fn delete_report_sync(&self, id: &str) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute("DELETE FROM reports WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub(crate) fn update_report_cache_sync(
        &self,
        id: &str,
        last_run_at: Option<&str>,
        last_finding_atom_id: Option<Option<&str>>,
        last_error: Option<Option<&str>>,
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        // SQL `COALESCE`-style approach won't work here because we want to
        // distinguish "leave unchanged" (outer None) from "set to NULL"
        // (Some(None)). Do separate statements; the cache columns are
        // advisory so the small multi-statement cost isn't worth one
        // fused UPDATE. `updated_at` only advances when at least one
        // cache column was actually written.
        let now = chrono::Utc::now().to_rfc3339();
        if let Some(run_at) = last_run_at {
            conn.execute(
                "UPDATE reports SET last_run_at = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, run_at, now],
            )?;
        }
        if let Some(finding) = last_finding_atom_id {
            conn.execute(
                "UPDATE reports SET last_finding_atom_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, finding, now],
            )?;
        }
        if let Some(err) = last_error {
            conn.execute(
                "UPDATE reports SET last_error = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, err, now],
            )?;
        }
        Ok(())
    }

    pub(crate) fn list_findings_for_report_sync(
        &self,
        report_id: &str,
        limit: i32,
    ) -> StorageResult<Vec<(ReportFinding, AtomWithTags)>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT finding_atom_id, report_id, run_id, report_name_snapshot, created_at
             FROM report_findings
             WHERE report_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let findings: Vec<ReportFinding> = stmt
            .query_map(params![report_id, limit], row_to_finding)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);

        // Resolve each finding to its atom. Done sequentially because the
        // list is bounded (limit, default ~50) and the join would
        // duplicate the row-to-atom logic we already have on AtomStore.
        let mut out = Vec::with_capacity(findings.len());
        for f in findings {
            if let Some(atom) = self.get_atom_impl(&f.finding_atom_id)? {
                out.push((f, atom));
            }
        }
        Ok(out)
    }

    pub(crate) fn get_finding_provenance_sync(
        &self,
        finding_atom_id: &str,
    ) -> StorageResult<Option<ReportFinding>> {
        let conn = self.db.read_conn()?;
        let row = conn
            .query_row(
                "SELECT finding_atom_id, report_id, run_id, report_name_snapshot, created_at
                 FROM report_findings WHERE finding_atom_id = ?1",
                params![finding_atom_id],
                row_to_finding,
            )
            .optional()?;
        Ok(row)
    }

    pub(crate) fn list_citations_for_finding_sync(
        &self,
        finding_atom_id: &str,
    ) -> StorageResult<Vec<ReportFindingCitation>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT finding_atom_id, cited_atom_id, position, excerpt
             FROM report_finding_citations
             WHERE finding_atom_id = ?1
             ORDER BY position ASC",
        )?;
        let rows = stmt
            .query_map(params![finding_atom_id], |row| {
                Ok(ReportFindingCitation {
                    finding_atom_id: row.get(0)?,
                    cited_atom_id: row.get(1)?,
                    position: row.get(2)?,
                    excerpt: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn list_finding_atom_ids_for_report_sync(
        &self,
        report_id: &str,
    ) -> StorageResult<Vec<String>> {
        let conn = self.db.read_conn()?;
        let mut stmt =
            conn.prepare("SELECT finding_atom_id FROM report_findings WHERE report_id = ?1")?;
        let ids = stmt
            .query_map(params![report_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    pub(crate) fn write_finding_transactionally_sync(
        &self,
        atom_request: &CreateAtomRequest,
        atom_id: &str,
        atom_created_at: &str,
        provenance: &ReportFinding,
        citations: &[ReportFindingCitation],
    ) -> StorageResult<AtomWithTags> {
        use super::atoms::atoms_fts_insert;
        use crate::{extract_title_and_snippet, parse_source};
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = conn.transaction()?;
        // Atom insert mirrors `insert_atom_impl`'s shape but stamps
        // `kind = 'report'` and runs inside the report's transaction so
        // any failure below rolls the finding atom back.
        let (title, snippet) = extract_title_and_snippet(&atom_request.content, 300);
        let source = atom_request.source_url.as_deref().map(parse_source);
        // `tagging_status = 'skipped'` keeps the auto-tag pipeline off
        // finding atoms. They're already stamped with the report's
        // configured `output_atom_tags` deterministically; letting the
        // LLM tagger run on agent prose would create runaway category
        // bloat and defeat the deterministic intent.
        tx.execute(
            "INSERT INTO atoms
                (id, content, source_url, source, published_at, created_at, updated_at,
                 embedding_status, tagging_status, title, snippet, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 'pending', 'skipped', ?7, ?8, 'report')",
            params![
                atom_id,
                &atom_request.content,
                &atom_request.source_url,
                &source,
                &atom_request.published_at,
                atom_created_at,
                &title,
                &snippet,
            ],
        )?;
        atoms_fts_insert(&tx, atom_id)?;
        for tag_id in &atom_request.tag_ids {
            tx.execute(
                "INSERT INTO atom_tags (atom_id, tag_id, source) VALUES (?1, ?2, 'manual')",
                params![atom_id, tag_id],
            )?;
        }

        // Provenance row.
        tx.execute(
            "INSERT INTO report_findings
                (finding_atom_id, report_id, run_id, report_name_snapshot, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                provenance.finding_atom_id,
                provenance.report_id,
                provenance.run_id,
                provenance.report_name_snapshot,
                provenance.created_at,
            ],
        )?;

        // Citation rows. Composite PK (finding_atom_id, cited_atom_id,
        // position) prevents accidental duplicates within a run.
        {
            let mut stmt = tx.prepare(
                "INSERT INTO report_finding_citations
                    (finding_atom_id, cited_atom_id, position, excerpt)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for c in citations {
                stmt.execute(params![
                    c.finding_atom_id,
                    c.cited_atom_id,
                    c.position,
                    c.excerpt,
                ])?;
            }
        }

        tx.commit()?;
        drop(conn);

        self.get_atom_impl(atom_id)?.ok_or_else(|| {
            AtomicCoreError::DatabaseOperation(format!(
                "finding atom {atom_id} vanished after transactional write"
            ))
        })
    }
}

#[async_trait]
impl ReportStore for SqliteStorage {
    async fn list_reports(&self) -> StorageResult<Vec<Report>> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.list_reports_sync())
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_enabled_reports(&self) -> StorageResult<Vec<Report>> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.list_enabled_reports_sync())
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_report(&self, id: &str) -> StorageResult<Option<Report>> {
        let storage = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || storage.get_report_sync(&id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn insert_report(&self, request: &CreateReportRequest) -> StorageResult<Report> {
        let storage = self.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || storage.insert_report_sync(&request))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn update_report(
        &self,
        id: &str,
        request: &UpdateReportRequest,
    ) -> StorageResult<Report> {
        let storage = self.clone();
        let id = id.to_string();
        let request = request.clone();
        tokio::task::spawn_blocking(move || storage.update_report_sync(&id, &request))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn set_report_enabled(&self, id: &str, enabled: bool) -> StorageResult<()> {
        let storage = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || storage.set_report_enabled_sync(&id, enabled))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn delete_report(&self, id: &str) -> StorageResult<()> {
        let storage = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || storage.delete_report_sync(&id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn update_report_cache(
        &self,
        id: &str,
        last_run_at: Option<&str>,
        last_finding_atom_id: Option<Option<&str>>,
        last_error: Option<Option<&str>>,
    ) -> StorageResult<()> {
        let storage = self.clone();
        let id = id.to_string();
        let last_run_at = last_run_at.map(|s| s.to_string());
        let last_finding_atom_id = last_finding_atom_id.map(|inner| inner.map(|s| s.to_string()));
        let last_error = last_error.map(|inner| inner.map(|s| s.to_string()));
        tokio::task::spawn_blocking(move || {
            storage.update_report_cache_sync(
                &id,
                last_run_at.as_deref(),
                last_finding_atom_id.as_ref().map(|i| i.as_deref()),
                last_error.as_ref().map(|i| i.as_deref()),
            )
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_findings_for_report(
        &self,
        report_id: &str,
        limit: i32,
    ) -> StorageResult<Vec<(ReportFinding, AtomWithTags)>> {
        let storage = self.clone();
        let report_id = report_id.to_string();
        tokio::task::spawn_blocking(move || {
            storage.list_findings_for_report_sync(&report_id, limit)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_finding_provenance(
        &self,
        finding_atom_id: &str,
    ) -> StorageResult<Option<ReportFinding>> {
        let storage = self.clone();
        let id = finding_atom_id.to_string();
        tokio::task::spawn_blocking(move || storage.get_finding_provenance_sync(&id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_finding_atom_ids_for_report(
        &self,
        report_id: &str,
    ) -> StorageResult<Vec<String>> {
        let storage = self.clone();
        let report_id = report_id.to_string();
        tokio::task::spawn_blocking(move || {
            storage.list_finding_atom_ids_for_report_sync(&report_id)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_citations_for_finding(
        &self,
        finding_atom_id: &str,
    ) -> StorageResult<Vec<ReportFindingCitation>> {
        let storage = self.clone();
        let finding_atom_id = finding_atom_id.to_string();
        tokio::task::spawn_blocking(move || {
            storage.list_citations_for_finding_sync(&finding_atom_id)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn write_finding_transactionally(
        &self,
        atom_request: &CreateAtomRequest,
        atom_id: &str,
        atom_created_at: &str,
        provenance: &ReportFinding,
        citations: &[ReportFindingCitation],
    ) -> StorageResult<AtomWithTags> {
        let storage = self.clone();
        let atom_request = atom_request.clone();
        let atom_id = atom_id.to_string();
        let atom_created_at = atom_created_at.to_string();
        let provenance = provenance.clone();
        let citations = citations.to_vec();
        tokio::task::spawn_blocking(move || {
            storage.write_finding_transactionally_sync(
                &atom_request,
                &atom_id,
                &atom_created_at,
                &provenance,
                &citations,
            )
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }
}

// ==================== Phase-3 briefings → findings migration ====================

impl SqliteStorage {
    pub(crate) fn fetch_legacy_briefings_sync(
        &self,
    ) -> StorageResult<Vec<crate::reports::seed::LegacyBriefingRow>> {
        use crate::reports::seed::{LegacyBriefingCitation, LegacyBriefingRow};

        let conn = self.db.read_conn()?;

        // Table-missing is a valid pre-state: a fresh DB created post-V22
        // never had `briefings`. Returning empty matches the idempotent
        // contract.
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='briefings'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !exists {
            return Ok(Vec::new());
        }

        // Deterministic order so positions in the resulting findings line
        // up with how the user originally saw them.
        let mut briefings_stmt = conn.prepare(
            "SELECT id, content, created_at
             FROM briefings
             ORDER BY created_at ASC",
        )?;
        let briefings: Vec<(String, String, String)> = briefings_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(briefings_stmt);

        let mut citations_stmt = conn.prepare(
            "SELECT briefing_id, citation_index, atom_id, excerpt
             FROM briefing_citations
             ORDER BY briefing_id ASC, citation_index ASC",
        )?;
        let citation_rows: Vec<(String, i32, String, String)> = citations_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Bucket citations by briefing_id for an O(n) join.
        let mut by_briefing: std::collections::HashMap<String, Vec<LegacyBriefingCitation>> =
            std::collections::HashMap::with_capacity(briefings.len());
        for (briefing_id, citation_index, atom_id, excerpt) in citation_rows {
            by_briefing
                .entry(briefing_id)
                .or_default()
                .push(LegacyBriefingCitation {
                    citation_index,
                    atom_id,
                    excerpt,
                });
        }

        Ok(briefings
            .into_iter()
            .map(|(id, content, created_at)| {
                let citations = by_briefing.remove(&id).unwrap_or_default();
                LegacyBriefingRow {
                    id,
                    content,
                    created_at,
                    citations,
                }
            })
            .collect())
    }

    pub(crate) fn drop_legacy_briefing_tables_sync(&self) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        // `briefing_citations` first — it FKs to `briefings`.
        conn.execute_batch(
            "DROP TABLE IF EXISTS briefing_citations;
             DROP TABLE IF EXISTS briefings;
             DROP INDEX IF EXISTS idx_briefings_created;
             DROP INDEX IF EXISTS idx_briefing_citations_briefing;",
        )?;
        Ok(())
    }
}

#[async_trait]
impl crate::storage::traits::LegacyBriefingsMigrationStore for SqliteStorage {
    async fn fetch_legacy_briefings(
        &self,
    ) -> StorageResult<Vec<crate::reports::seed::LegacyBriefingRow>> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.fetch_legacy_briefings_sync())
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn drop_legacy_briefing_tables(&self) -> StorageResult<()> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.drop_legacy_briefing_tables_sync())
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }
}
