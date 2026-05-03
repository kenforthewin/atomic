//! SQLite-backed storage for health check raw data and the two health tables
//! (`health_reports`, `health_fix_log`).
//!
//! All methods here are synchronous (run inside `tokio::task::spawn_blocking`).

use crate::error::AtomicCoreError;
use crate::health::audit::{HealthFixLog, StoredHealthReport};
use crate::health::{DuplicatePair, WikiGap, WikiStaleEntry};
use crate::storage::sqlite::SqliteStorage;
use rusqlite::params;

// ==================== Raw health data ====================

/// All data needed by the health checks, fetched in a single blocking pass.
#[derive(Debug, Clone, Default)]
pub struct HealthRawData {
    // — totals —
    pub total_atoms: i32,

    // — embedding coverage —
    pub embedding_pending: i32,
    pub embedding_processing: i32,
    pub embedding_complete: i32,
    pub embedding_failed: i32,

    // — tagging coverage —
    pub tagging_pending: i32,
    pub tagging_processing: i32,
    pub tagging_complete: i32,
    pub tagging_failed: i32,
    pub tagging_skipped: i32,
    /// Atoms whose tagging_status = 'complete' but have 0 tags assigned.
    pub untagged_complete: i32,
    /// Atoms whose tagging_status = 'skipped' AND have 0 tags (invisible gap).
    pub skipped_untagged: i32,

    // — source uniqueness —
    /// `(source_url, [atom_id, ...])` for URLs that appear > 1 time.
    pub duplicate_sources: Vec<(String, Vec<String>)>,

    // — orphan tags —
    /// `(id, name)` for tags with 0 atoms and no children (excluding autotag targets).
    pub orphan_tags: Vec<(String, String)>,

    // — semantic graph freshness —
    pub newest_atom_updated_at: Option<String>,
    pub newest_edge_created_at: Option<String>,
    /// Count of atoms whose `updated_at` > `newest_edge_created_at`.
    pub atoms_since_edge_rebuild: i32,

    // — wiki coverage —
    pub wiki_eligible_count: i32,
    pub wiki_present_count: i32,
    pub wiki_stale_count: i32,
    pub wiki_gaps: Vec<WikiGap>,
    pub wiki_stale: Vec<WikiStaleEntry>,

    // — content quality —
    /// Atom IDs with content length < 100 chars.
    pub very_short_atoms: Vec<String>,
    /// Atom IDs with content length > 15 000 chars.
    pub very_long_atoms: Vec<String>,
    /// Atom IDs with no markdown heading (`#` at start of line).
    pub no_heading_atoms: Vec<String>,
    /// Atom IDs with null source_url and no "Source:" text in content.
    /// Atom IDs with null source_url and no "Source:" text in content.
    pub no_source_atoms: Vec<crate::health::AtomPreview>,

    // — tag health —
    pub single_atom_tags: i32,
    pub rootless_tags: i32,
    pub similar_name_pair_count: i32,
    /// Similar tag name pairs — (id_a, name_a, id_b, name_b).
    pub similar_name_pairs_list: Vec<(String, String, String, String)>,
    /// Single-atom tags (exactly 1 atom attached), up to 50.
    pub single_atom_tag_list: Vec<crate::health::SingleAtomTagEntry>,

    // — duplicate detection (similarity >= 0.92) —
    pub duplicate_pairs: Vec<DuplicatePair>,

    // — boilerplate pollution (atoms with >= 2 edges at similarity >= 0.99) —
    /// Atom IDs whose embeddings are dominated by shared template text.
    /// Atoms whose embeddings are dominated by shared template text.
    pub boilerplate_affected_atoms: Vec<crate::health::BoilerplateAtomEntry>,

    // — contradiction candidates (similarity 0.75..0.92) —
    pub contradiction_pairs_checked: i32,
    pub contradiction_candidate_count: i32,

    /// Pairs of high-similarity atoms for manual contradiction review (similarity 0.80–0.92).
    pub contradiction_pairs: Vec<crate::health::ContradictionPairEntry>,
    /// Rootless tags (parent_id IS NULL, not autotag targets) with atom counts.
    pub rootless_tag_list: Vec<crate::health::RootlessTagEntry>,
}

impl SqliteStorage {
    /// Gather all raw health-check data in a single blocking pass.
    pub(crate) fn health_check_data_impl(
        &self,
        thresholds: &crate::health::HealthThresholds,
    ) -> Result<HealthRawData, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let mut raw = HealthRawData::default();

        // ---- total atoms ----
        raw.total_atoms = conn.query_row("SELECT COUNT(*) FROM atoms", [], |r| r.get(0))?;

        if raw.total_atoms == 0 {
            return Ok(raw);
        }

        // ---- embedding coverage ----
        let mut stmt = conn.prepare(
            "SELECT embedding_status, COUNT(*) FROM atoms GROUP BY embedding_status",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            let count: i32 = row.get(1)?;
            match status.as_str() {
                "pending" => raw.embedding_pending = count,
                "processing" => raw.embedding_processing = count,
                "complete" => raw.embedding_complete = count,
                "failed" => raw.embedding_failed = count,
                _ => {}
            }
        }

        // ---- tagging coverage ----
        let mut stmt = conn.prepare(
            "SELECT tagging_status, COUNT(*) FROM atoms GROUP BY tagging_status",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            let count: i32 = row.get(1)?;
            match status.as_str() {
                "pending" => raw.tagging_pending = count,
                "processing" => raw.tagging_processing = count,
                "complete" => raw.tagging_complete = count,
                "failed" => raw.tagging_failed = count,
                "skipped" => raw.tagging_skipped = count,
                _ => {}
            }
        }

        // Atoms that completed tagging but have 0 tags
        raw.untagged_complete = conn.query_row(
            "SELECT COUNT(*) FROM atoms a
             WHERE a.tagging_status = 'complete'
             AND NOT EXISTS (SELECT 1 FROM atom_tags at WHERE at.atom_id = a.id)",
            [],
            |r| r.get(0),
        )?;

        // Atoms skipped by the tagger that also have 0 tags — invisible gap
        raw.skipped_untagged = conn.query_row(
            "SELECT COUNT(*) FROM atoms a
             WHERE a.tagging_status = 'skipped'
             AND NOT EXISTS (SELECT 1 FROM atom_tags at WHERE at.atom_id = a.id)",
            [],
            |r| r.get(0),
        )?;

        // ---- source uniqueness ----
        let mut stmt = conn.prepare(
            "SELECT source_url, COUNT(*) as cnt, GROUP_CONCAT(id)
             FROM atoms
             WHERE source_url IS NOT NULL
             GROUP BY source_url
             HAVING cnt > 1
             LIMIT 50",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let url: String = row.get(0)?;
            let ids_csv: String = row.get(2)?;
            let ids: Vec<String> = ids_csv.split(',').map(|s| s.to_string()).collect();
            raw.duplicate_sources.push((url, ids));
        }

        // ---- orphan tags ----
        let mut stmt = conn.prepare(
            "SELECT t.id, t.name
             FROM tags t
             LEFT JOIN atom_tags at ON t.id = at.tag_id
             LEFT JOIN tags children ON children.parent_id = t.id
             WHERE at.tag_id IS NULL
               AND children.id IS NULL
               AND t.is_autotag_target = 0",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            raw.orphan_tags.push((row.get(0)?, row.get(1)?));
        }

        // ---- semantic graph freshness ----
        raw.newest_atom_updated_at = conn
            .query_row("SELECT MAX(updated_at) FROM atoms", [], |r| {
                r.get::<_, Option<String>>(0)
            })
            .ok()
            .flatten();

        raw.newest_edge_created_at = conn
            .query_row(
                "SELECT MAX(created_at) FROM semantic_edges",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();

        if let Some(ref newest_edge) = raw.newest_edge_created_at {
            raw.atoms_since_edge_rebuild = conn.query_row(
                "SELECT COUNT(*) FROM atoms WHERE updated_at > ?1",
                params![newest_edge],
                |r| r.get(0),
            )?;
        } else if raw.total_atoms > 0 {
            // No edges at all
            raw.atoms_since_edge_rebuild = raw.total_atoms;
        }

        // ---- wiki coverage ----
        // Tags with >= thresholds.wiki_min_atoms_per_tag atoms
        let mut stmt = conn.prepare(
            "SELECT t.id, t.name,
                    COUNT(DISTINCT at.atom_id) as atom_count,
                    w.id IS NOT NULL as has_wiki,
                    w.updated_at,
                    (SELECT MAX(a.updated_at) FROM atoms a
                     JOIN atom_tags at2 ON a.id = at2.atom_id
                     WHERE at2.tag_id = t.id) as last_atom_update
             FROM tags t
             JOIN atom_tags at ON t.id = at.tag_id
             LEFT JOIN wiki_articles w ON t.id = w.tag_id
             GROUP BY t.id
             HAVING COUNT(DISTINCT at.atom_id) >= ?1
             ORDER BY COUNT(DISTINCT at.atom_id) DESC
             LIMIT 50",
        )?;
        let mut rows = stmt.query(params![thresholds.wiki_min_atoms_per_tag])?;
        while let Some(row) = rows.next()? {
            let tag_id: String = row.get(0)?;
            let tag_name: String = row.get(1)?;
            let atom_count: i32 = row.get(2)?;
            let has_wiki: bool = row.get(3)?;
            let wiki_updated_at: Option<String> = row.get(4)?;
            let last_atom_update: Option<String> = row.get(5)?;

            raw.wiki_eligible_count += 1;

            if has_wiki {
                raw.wiki_present_count += 1;
                // Stale if any atom updated after the wiki
                let is_stale = match (&wiki_updated_at, &last_atom_update) {
                    (Some(w), Some(a)) => a > w,
                    _ => false,
                };
                if is_stale {
                    raw.wiki_stale_count += 1;
                    raw.wiki_stale.push(WikiStaleEntry {
                        tag_id,
                        tag_name,
                        new_atom_count: atom_count,
                    });
                }
            } else {
                raw.wiki_gaps.push(WikiGap {
                    tag_id,
                    tag_name,
                    atom_count,
                });
            }
        }

        // ---- content quality ----
        const LIMIT: usize = 20;

        let mut stmt = conn.prepare(
            "SELECT id FROM atoms WHERE length(content) < ?1 LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![thresholds.content_quality_short_chars, LIMIT as i32])?;
        while let Some(row) = rows.next()? {
            raw.very_short_atoms.push(row.get(0)?);
        }

        let mut stmt = conn.prepare(
            "SELECT id FROM atoms WHERE length(content) > ?1 LIMIT ?2",
        )?;
        let mut rows = stmt.query(params![thresholds.content_quality_long_chars, LIMIT as i32])?;
        while let Some(row) = rows.next()? {
            raw.very_long_atoms.push(row.get(0)?);
        }

        // No heading: content doesn't start with '#' and doesn't have '\n#'
        let mut stmt = conn.prepare(
            "SELECT id FROM atoms
             WHERE content NOT LIKE '#%'
               AND content NOT LIKE '%' || char(10) || '#%'
             LIMIT ?1",
        )?;
        let mut rows = stmt.query(params![LIMIT as i32])?;
        while let Some(row) = rows.next()? {
            raw.no_heading_atoms.push(row.get(0)?);
        }

        // No source: null source_url and no http(s):// in content
        // Return title preview + created_at for better UX (no secondary fetch needed)
        let mut stmt = conn.prepare(
            "SELECT id, content, created_at FROM atoms
             WHERE source_url IS NULL
               AND content NOT LIKE '%http://%'
               AND content NOT LIKE '%https://%'
               AND content NOT LIKE '%Source:%'
             ORDER BY updated_at DESC
             LIMIT ?1",
        )?;
        let mut rows = stmt.query(params![LIMIT as i32])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            let created_at: String = row.get(2)?;
            let title = extract_title_preview(&content);
            raw.no_source_atoms.push(crate::health::AtomPreview { id, title, created_at });
        }

        // ---- tag health ----
        // Single-atom tags: fetch list (up to 50) and derive count from it.
        {
            let mut stmt = conn.prepare(
                "SELECT t.id, t.name, t.is_autotag_target
                 FROM tags t
                 JOIN atom_tags at ON t.id = at.tag_id
                 GROUP BY t.id HAVING COUNT(at.atom_id) = 1
                 ORDER BY t.name
                 LIMIT 51",
            )?;
            let mut rows = stmt.query([])?;
            let mut truncated = false;
            while let Some(row) = rows.next()? {
                if raw.single_atom_tag_list.len() == 50 {
                    truncated = true;
                    break;
                }
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let is_autotag: bool = row.get::<_, i32>(2)? != 0;
                raw.single_atom_tag_list.push(crate::health::SingleAtomTagEntry { id, name, is_autotag });
            }
            raw.single_atom_tags = if truncated {
                // Count exact total when list was truncated
                conn.query_row(
                    "SELECT COUNT(*) FROM (
                         SELECT t.id FROM tags t
                         JOIN atom_tags at ON t.id = at.tag_id
                         GROUP BY t.id HAVING COUNT(at.atom_id) = 1
                     )",
                    [],
                    |r| r.get(0),
                )?
            } else {
                raw.single_atom_tag_list.len() as i32
            };
        }

        // Rootless tags: user-created tags with no parent (excludes autotag category roots).
        // is_autotag_target = 1 marks system roots (Topics, People, etc.) — exclude them.
        {
            let mut stmt = conn.prepare(
                "SELECT t.id, t.name, COUNT(at.atom_id) as atom_count
                 FROM tags t
                 LEFT JOIN atom_tags at ON t.id = at.tag_id
                 WHERE t.parent_id IS NULL
                   AND t.is_autotag_target = 0
                 GROUP BY t.id
                 ORDER BY atom_count DESC
                 LIMIT 50",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let atom_count: i32 = row.get(2)?;
                raw.rootless_tag_list.push(crate::health::RootlessTagEntry { id, name, atom_count });
            }
            raw.rootless_tags = raw.rootless_tag_list.len() as i32;
        }

        // Similar name pairs: fetch all tag (id, name) and compare in Rust
        {
            let mut stmt = conn.prepare("SELECT id, name FROM tags WHERE atom_count > 0")?;
            let mut rows = stmt.query([])?;
            let mut id_names: Vec<(String, String)> = Vec::new();
            while let Some(row) = rows.next()? {
                id_names.push((row.get(0)?, row.get(1)?));
            }
            raw.similar_name_pairs_list = collect_similar_name_pairs(&id_names);
            raw.similar_name_pair_count = raw.similar_name_pairs_list.len() as i32;
        }

        // ---- content overlap detection (Tier 3) ----
        // Moderate similarity (0.55–0.85) + different source prefixes + >= 2 shared tags.
        // This surfaces semantically related atoms from different corpora that should be
        // reviewed for linking or merging — not template clones (those are boilerplate_pollution).
        {
            let mut stmt = conn.prepare(
                "SELECT
                     se.source_atom_id, se.target_atom_id, se.similarity_score,
                     a1.source_url, a1.content,
                     a2.source_url, a2.content,
                     COUNT(DISTINCT at_a.tag_id) as shared_tag_count,
                     a1.created_at, a2.created_at
                 FROM semantic_edges se
                 JOIN atoms a1 ON se.source_atom_id = a1.id
                 JOIN atoms a2 ON se.target_atom_id = a2.id
                 JOIN atom_tags at_a ON a1.id = at_a.atom_id
                 JOIN atom_tags at_b ON a2.id = at_b.atom_id AND at_a.tag_id = at_b.tag_id
                 WHERE se.similarity_score BETWEEN ?1 AND ?2
                 GROUP BY se.source_atom_id, se.target_atom_id
                 HAVING COUNT(DISTINCT at_a.tag_id) >= ?3
                 ORDER BY COUNT(DISTINCT at_a.tag_id) DESC, se.similarity_score DESC
                 LIMIT 20",
            )?;
            let mut rows = stmt.query(params![
                thresholds.content_overlap_similarity_min,
                thresholds.content_overlap_similarity_max,
                thresholds.content_overlap_shared_tags_min,
            ])?;
            while let Some(row) = rows.next()? {
                let a_id: String = row.get(0)?;
                let b_id: String = row.get(1)?;
                let similarity: f32 = row.get(2)?;
                let a_source: Option<String> = row.get(3)?;
                let a_content: String = row.get(4)?;
                let b_source: Option<String> = row.get(5)?;
                let b_content: String = row.get(6)?;
                let shared_tag_count: i32 = row.get(7)?;
                let a_created_at: Option<String> = row.get(8)?;
                let b_created_at: Option<String> = row.get(9)?;

                // Skip same-corpus pairs — those are template pollution, not content overlap.
                let prefix_a = source_prefix(&a_source);
                let prefix_b = source_prefix(&b_source);
                if prefix_a == prefix_b {
                    continue;
                }

                let a_title = extract_title_preview(&a_content);
                let b_title = extract_title_preview(&b_content);

                raw.duplicate_pairs.push(DuplicatePair {
                    pair_id: uuid::Uuid::new_v4().to_string(),
                    atom_a_id: a_id,
                    atom_a_title: a_title,
                    atom_a_source: a_source,
                    atom_b_id: b_id,
                    atom_b_title: b_title,
                    atom_b_source: b_source,
                    similarity,
                    shared_tag_count,
                    atom_a_created_at: a_created_at,
                    atom_b_created_at: b_created_at,
                });
            }
        }

        // ---- boilerplate pollution (atoms with >= thresholds.boilerplate_min_clones edges at similarity >= thresholds.boilerplate_similarity) ----
        // Return atom title + clone count so UI can show context and prioritise review.
        {
            let mut stmt = conn.prepare(
                "SELECT se.source_atom_id, a.content, COUNT(*) as clone_count
                 FROM semantic_edges se
                 JOIN atoms a ON se.source_atom_id = a.id
                 WHERE se.similarity_score >= ?1
                 GROUP BY se.source_atom_id
                 HAVING COUNT(*) >= ?2
                 ORDER BY clone_count DESC
                 LIMIT 50",
            )?;
            let mut rows = stmt.query(params![
                thresholds.boilerplate_similarity,
                thresholds.boilerplate_min_clones,
            ])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let content: String = row.get(1)?;
                let clone_count: i32 = row.get(2)?;
                let title = extract_title_preview(&content);
                raw.boilerplate_affected_atoms.push(crate::health::BoilerplateAtomEntry { id, title, clone_count });
            }
        }

        // ---- contradiction candidates (similarity thresholds.contradiction_similarity_min .. thresholds.contradiction_similarity_max) ----
        // Surface actual atom pairs for manual review.
        {
            let mut stmt = conn.prepare(
                "SELECT
                     se.source_atom_id, se.target_atom_id, se.similarity_score,
                     a1.source_url, a1.content,
                     a2.source_url, a2.content,
                     COUNT(DISTINCT at_a.tag_id) as shared_tag_count,
                     a1.created_at, a2.created_at
                 FROM semantic_edges se
                 JOIN atoms a1 ON se.source_atom_id = a1.id
                 JOIN atoms a2 ON se.target_atom_id = a2.id
                 LEFT JOIN atom_tags at_a ON a1.id = at_a.atom_id
                 LEFT JOIN atom_tags at_b ON a2.id = at_b.atom_id AND at_a.tag_id = at_b.tag_id
                 WHERE se.similarity_score >= ?1 AND se.similarity_score < ?2
                 GROUP BY se.source_atom_id, se.target_atom_id
                 HAVING COUNT(DISTINCT at_a.tag_id) >= ?3
                 ORDER BY se.similarity_score DESC
                 LIMIT 20",
            )?;
            let mut rows = stmt.query(params![
                thresholds.contradiction_similarity_min,
                thresholds.contradiction_similarity_max,
                thresholds.contradiction_shared_tags_min,
            ])?;
            while let Some(row) = rows.next()? {
                let a_id: String = row.get(0)?;
                let b_id: String = row.get(1)?;
                let similarity: f32 = row.get(2)?;
                let a_source: Option<String> = row.get(3)?;
                let a_content: String = row.get(4)?;
                let b_source: Option<String> = row.get(5)?;
                let b_content: String = row.get(6)?;
                let shared_tag_count: i32 = row.get(7)?;
                let a_created_at: Option<String> = row.get(8)?;
                let b_created_at: Option<String> = row.get(9)?;
                let a_title = extract_title_preview(&a_content);
                let b_title = extract_title_preview(&b_content);
                raw.contradiction_pairs.push(crate::health::ContradictionPairEntry {
                    pair_id: uuid::Uuid::new_v4().to_string(),
                    atom_a: crate::health::ContradictionAtom { id: a_id, title: a_title, source: a_source, created_at: a_created_at },
                    atom_b: crate::health::ContradictionAtom { id: b_id, title: b_title, source: b_source, created_at: b_created_at },
                    similarity,
                    shared_tag_count,
                });
            }
            raw.contradiction_pairs_checked = conn.query_row(
                "SELECT COUNT(*) FROM semantic_edges
                 WHERE similarity_score >= ?1 AND similarity_score < ?2",
                params![
                    thresholds.contradiction_similarity_min,
                    thresholds.contradiction_similarity_max,
                ],
                |r| r.get(0),
            )?;
            raw.contradiction_candidate_count = raw.contradiction_pairs.len() as i32;
        }

        Ok(raw)
    }

    /// Reset atoms with `tagging_status = 'skipped'` AND 0 tags back to `pending`
    /// so the tagger pipeline will process them on the next run.
    /// Returns the number of atoms reset.
    pub(crate) fn reset_skipped_untagged_to_pending_impl(
        &self,
    ) -> Result<i32, AtomicCoreError> {
        let conn = self.db.conn.lock().map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("lock error: {e}"))
        })?;
        let n = conn.execute(
            "UPDATE atoms
             SET tagging_status = 'pending'
             WHERE tagging_status = 'skipped'
             AND NOT EXISTS (
                 SELECT 1 FROM atom_tags at WHERE at.atom_id = atoms.id
             )",
            [],
        )? as i32;
        Ok(n)
    }

    // ==================== Health report storage ====================

    pub(crate) fn store_health_report_impl(
        &self,
        report: &StoredHealthReport,
    ) -> Result<(), AtomicCoreError> {
        let conn = self.db.conn.lock().map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("lock error: {e}"))
        })?;
        conn.execute(
            "INSERT OR REPLACE INTO health_reports
             (id, computed_at, overall_score, check_scores, atom_count, auto_fixes_applied, report_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                report.id,
                report.computed_at,
                report.overall_score,
                report.check_scores,
                report.atom_count,
                report.auto_fixes_applied,
                report.report_json,
            ],
        )?;
        // Prune reports older than 90 days
        conn.execute(
            "DELETE FROM health_reports WHERE computed_at < datetime('now', '-90 days')",
            [],
        )?;
        Ok(())
    }

    pub(crate) fn get_latest_health_report_impl(
        &self,
    ) -> Result<Option<crate::health::HealthReport>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let result: rusqlite::Result<String> = conn.query_row(
            "SELECT report_json FROM health_reports ORDER BY computed_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        );
        match result {
            Ok(json) => {
                let report: crate::health::HealthReport =
                    serde_json::from_str(&json).map_err(|e| {
                        AtomicCoreError::DatabaseOperation(format!(
                            "failed to deserialize health report: {e}"
                        ))
                    })?;
                Ok(Some(report))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn get_health_reports_impl(
        &self,
        limit: i32,
    ) -> Result<Vec<StoredHealthReport>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, computed_at, overall_score, check_scores, atom_count, auto_fixes_applied, report_json
             FROM health_reports
             ORDER BY computed_at DESC
             LIMIT ?1",
        )?;
        let reports = stmt
            .query_map(params![limit], |r| {
                Ok(StoredHealthReport {
                    id: r.get(0)?,
                    computed_at: r.get(1)?,
                    overall_score: r.get::<_, i32>(2)? as u32,
                    check_scores: r.get(3)?,
                    atom_count: r.get(4)?,
                    auto_fixes_applied: r.get(5)?,
                    report_json: r.get(6)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(reports)
    }

    // ==================== Fix log storage ====================

    pub(crate) fn log_fix_action_impl(
        &self,
        log: &HealthFixLog,
    ) -> Result<(), AtomicCoreError> {
        let conn = self.db.conn.lock().map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("lock error: {e}"))
        })?;
        let atom_ids_json = log
            .atom_ids
            .as_ref()
            .map(|ids| serde_json::to_string(ids).unwrap_or_default());
        let tag_ids_json = log
            .tag_ids
            .as_ref()
            .map(|ids| serde_json::to_string(ids).unwrap_or_default());
        conn.execute(
            "INSERT INTO health_fix_log
             (id, check_name, action, tier, atom_ids, tag_ids,
              before_state, after_state, llm_prompt, llm_response, executed_at, undone_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                log.id,
                log.check_name,
                log.action,
                log.tier,
                atom_ids_json,
                tag_ids_json,
                log.before_state,
                log.after_state,
                log.llm_prompt,
                log.llm_response,
                log.executed_at,
                log.undone_at,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn get_fix_log_impl(
        &self,
        fix_id: &str,
    ) -> Result<Option<HealthFixLog>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let result = conn.query_row(
            "SELECT id, check_name, action, tier, atom_ids, tag_ids,
                    before_state, after_state, llm_prompt, llm_response, executed_at, undone_at
             FROM health_fix_log WHERE id = ?1",
            params![fix_id],
            |r| {
                Ok(HealthFixLog {
                    id: r.get(0)?,
                    check_name: r.get(1)?,
                    action: r.get(2)?,
                    tier: r.get(3)?,
                    atom_ids: r
                        .get::<_, Option<String>>(4)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    tag_ids: r
                        .get::<_, Option<String>>(5)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    before_state: r.get(6)?,
                    after_state: r.get(7)?,
                    llm_prompt: r.get(8)?,
                    llm_response: r.get(9)?,
                    executed_at: r.get(10)?,
                    undone_at: r.get(11)?,
                })
            },
        );
        match result {
            Ok(log) => Ok(Some(log)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn get_recent_fixes_impl(
        &self,
        limit: i32,
    ) -> Result<Vec<HealthFixLog>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, check_name, action, tier, atom_ids, tag_ids,
                    before_state, after_state, llm_prompt, llm_response, executed_at, undone_at
             FROM health_fix_log
             ORDER BY executed_at DESC
             LIMIT ?1",
        )?;
        let logs = stmt
            .query_map(params![limit], |r| {
                Ok(HealthFixLog {
                    id: r.get(0)?,
                    check_name: r.get(1)?,
                    action: r.get(2)?,
                    tier: r.get(3)?,
                    atom_ids: r
                        .get::<_, Option<String>>(4)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    tag_ids: r
                        .get::<_, Option<String>>(5)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    before_state: r.get(6)?,
                    after_state: r.get(7)?,
                    llm_prompt: r.get(8)?,
                    llm_response: r.get(9)?,
                    executed_at: r.get(10)?,
                    undone_at: r.get(11)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(logs)
    }

    pub(crate) fn mark_fix_undone_impl(&self, fix_id: &str) -> Result<(), AtomicCoreError> {
        let conn = self.db.conn.lock().map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("lock error: {e}"))
        })?;
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE health_fix_log SET undone_at = ?1 WHERE id = ?2",
            params![now, fix_id],
        )?;
        Ok(())
    }

    // ==================== Link resolution storage ====================

    /// Fetch atoms that likely contain internal links (first-pass SQL filter).
    /// Returns (id, content, source_url).
    /// The exact link extraction happens in Rust using `link_resolution::extract_internal_links`.
    pub(crate) fn get_link_candidate_atoms_impl(
        &self,
    ) -> Result<Vec<(String, String, Option<String>)>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, content, source_url FROM atoms
             WHERE content LIKE '%](.%.md%'
             OR content LIKE '%](./%'
             OR content LIKE '%](../%'
             OR (content LIKE '%[[%' AND content LIKE '%]]%')",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Batch lookup: given a list of candidate source URLs, return a map of
    /// source_url → atom_id for those that exist in the database.
    pub(crate) fn find_atoms_by_source_urls_impl(
        &self,
        urls: &[String],
    ) -> Result<std::collections::HashMap<String, String>, AtomicCoreError> {
        if urls.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let conn = self.db.read_conn()?;
        let mut map = std::collections::HashMap::new();
        // SQLite doesn't support binding a variable-length IN list, so we query one by one.
        // For the typical link count (<50 per atom), this is fast enough.
        let mut stmt = conn.prepare("SELECT id FROM atoms WHERE source_url = ?1")?;
        for url in urls {
            if let Ok(id) = stmt.query_row(params![url], |r| r.get::<_, String>(0)) {
                map.insert(url.clone(), id);
            }
        }
        Ok(map)
    }

    /// Wikilink fallback: find an atom whose source_url ends with `/<name>.md`
    /// (case-insensitive on the name stem) anywhere in the vault.
    /// Returns the first match as (atom_id, title_preview).
    pub(crate) fn find_atom_by_wikilink_name_impl(
        &self,
        name: &str,
        vault_prefix: &str,
    ) -> Result<Option<(String, String)>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        // Try exact stem match under the vault (case-insensitive)
        let like_pattern = format!("%/{}%.md", name.to_lowercase().replace(' ', "-"));
        let alt_pattern  = format!("%/{}%.md", name.to_lowercase().replace(' ', "_"));
        let result = conn.query_row(
            "SELECT id, content FROM atoms
             WHERE source_url LIKE ?1 || ?3
                OR LOWER(source_url) LIKE ?2
                OR LOWER(source_url) LIKE ?4",
            params![vault_prefix, like_pattern, name.replace(' ', "-") + ".md", alt_pattern],
            |r| {
                let id: String = r.get(0)?;
                let content: String = r.get(1)?;
                Ok((id, content))
            },
        );
        match result {
            Ok((id, content)) => {
                let title = content
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or(&id)
                    .trim_start_matches('#')
                    .trim()
                    .chars()
                    .take(80)
                    .collect::<String>();
                Ok(Some((id, title)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Suggest atom candidates for a broken link query.
    /// Searches by source_url suffix, title prefix/contains, and content LIKE.
    /// Returns vec of (atom_id, title, source_url, score) sorted by score desc.
    pub(crate) fn suggest_atoms_by_query_impl(
        &self,
        q: &str,
        limit: i32,
    ) -> Result<Vec<(String, String, Option<String>, f32)>, AtomicCoreError> {
        if q.trim().is_empty() {
            return Ok(vec![]);
        }
        let conn = self.db.read_conn()?;
        let mut results: Vec<(String, String, Option<String>, f32)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Normalize query: strip extension, directory prefixes, replace hyphens/underscores with spaces.
        let normalized = {
            let no_ext = if let Some(pos) = q.rfind('.') {
                if pos > 0 && !q[..pos].contains('/') || q[..pos].contains('/') {
                    &q[..pos]
                } else {
                    q
                }
            } else {
                q
            };
            let no_dir = if let Some(pos) = no_ext.rfind('/') {
                &no_ext[pos + 1..]
            } else {
                no_ext
            };
            let no_prefix = no_dir.trim_start_matches('.').trim_start_matches('/');
            no_prefix.replace(['-', '_'], " ").to_lowercase()
        };
        let nq = normalized.as_str();

        // 1. Exact source_url suffix match (score 1.0)
        {
            let suffix_pat = format!("%{}", q);
            let mut stmt = conn.prepare(
                "SELECT id, content, source_url FROM atoms WHERE source_url LIKE ?1 ESCAPE '\\' LIMIT 20",
            )?;
            let rows = stmt.query_map(rusqlite::params![suffix_pat], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
            })?;
            for row in rows.flatten() {
                let (id, content, src) = row;
                if seen.insert(id.clone()) {
                    let title = extract_title_preview(&content);
                    results.push((id, title, src, 1.0f32));
                }
            }
        }

        // 2a. Title prefix match (score 0.8) — first non-empty line starts with nq
        {
            let prefix_pat = format!("{}%", nq);
            let prefix_pat_hash = format!("# {}%", nq);
            let mut stmt = conn.prepare(
                "SELECT id, content, source_url FROM atoms
                  WHERE LOWER(SUBSTR(TRIM(content), 1, 80)) LIKE ?1 ESCAPE '\\'
                     OR LOWER(SUBSTR(TRIM(content), 1, 80)) LIKE ?2 ESCAPE '\\'
                  LIMIT 40",
            )?;
            let rows = stmt.query_map(rusqlite::params![prefix_pat, prefix_pat_hash], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
            })?;
            for row in rows.flatten() {
                let (id, content, src) = row;
                if seen.insert(id.clone()) {
                    let title = extract_title_preview(&content);
                    results.push((id, title, src, 0.8f32));
                }
            }
        }

        // 2b. Title contains match (score 0.6)
        {
            let contains_pat = format!("%{}%", nq);
            let mut stmt = conn.prepare(
                "SELECT id, content, source_url FROM atoms
                  WHERE LOWER(SUBSTR(TRIM(content), 1, 80)) LIKE ?1 ESCAPE '\\'
                  LIMIT 40",
            )?;
            let rows = stmt.query_map(rusqlite::params![contains_pat], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
            })?;
            for row in rows.flatten() {
                let (id, content, src) = row;
                if seen.insert(id.clone()) {
                    let title = extract_title_preview(&content);
                    results.push((id, title, src, 0.6f32));
                }
            }
        }

        // 3. Fuzzy content LIKE on first 80 chars (score 0.4)
        {
            let contains_pat = format!("%{}%", nq);
            let mut stmt = conn.prepare(
                "SELECT id, content, source_url FROM atoms
                  WHERE LOWER(SUBSTR(content, 1, 80)) LIKE ?1 ESCAPE '\\'
                  LIMIT 40",
            )?;
            let rows = stmt.query_map(rusqlite::params![contains_pat], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
            })?;
            for row in rows.flatten() {
                let (id, content, src) = row;
                if seen.insert(id.clone()) {
                    let title = extract_title_preview(&content);
                    results.push((id, title, src, 0.4f32));
                }
            }
        }

        // Sort by score desc, truncate to limit.
        results.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit.max(1).min(20) as usize);
        Ok(results)
    }
}


// ==================== Helpers ====================

/// Collect similar tag name pairs — one is a prefix/substring of the other.
fn collect_similar_name_pairs(tags: &[(String, String)]) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    for (i, (a_id, a_name)) in tags.iter().enumerate() {
        for (b_id, b_name) in tags.iter().skip(i + 1) {
            let la = a_name.to_lowercase();
            let lb = b_name.to_lowercase();
            if la == lb {
                continue; // exact duplicate (already handled elsewhere)
            }
            if la.contains(lb.as_str()) || lb.contains(la.as_str()) {
                // canonical order: smaller id first, for stable pair_id
                if a_id <= b_id {
                    out.push((a_id.clone(), a_name.clone(), b_id.clone(), b_name.clone()));
                } else {
                    out.push((b_id.clone(), b_name.clone(), a_id.clone(), a_name.clone()));
                }
            }
        }
    }
    out
}

/// Extract first ~60 chars as a title preview.
fn extract_title_preview(content: &str) -> String {
    let first_line = content.lines().next().unwrap_or("").trim();
    let clean = first_line.trim_start_matches('#').trim();
    if clean.len() > 60 {
        format!("{}\u{2026}", &clean[..60])
    } else if clean.is_empty() {
        content.chars().take(60).collect()
    } else {
        clean.to_string()
    }
}

/// Extract the source prefix: scheme + authority (everything up to the path).
/// Examples:
///   `https://tylertech.atlassian.net/wiki/...` → `https://tylertech.atlassian.net`
///   `obsidian://ar-playbook/path/to/file`       → `obsidian://ar-playbook`
///   `None`                                      → `manual`
pub(crate) fn source_prefix(url: &Option<String>) -> String {
    let Some(u) = url else {
        return "manual".to_string();
    };
    // Find "://" then the next "/" after it
    if let Some(scheme_end) = u.find("://") {
        let after_scheme = &u[scheme_end + 3..];
        if let Some(slash) = after_scheme.find('/') {
            return u[..scheme_end + 3 + slash].to_string();
        }
    } else if let Some(slash) = u.find('/') {
        return u[..slash].to_string();
    }
    u.clone()
}

// ==================== Dismissal methods ====================

impl SqliteStorage {
    /// Get a tag by ID. Returns (name, parent_id).
    pub(crate) fn get_tag_by_id_impl(
        &self,
        tag_id: &str,
    ) -> Result<Option<(String, Option<String>)>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, parent_id FROM tags WHERE id = ?1",
        )?;
        let result = stmt
            .query_map(params![tag_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .next()
            .transpose()?;
        Ok(result)
    }

    /// List currently active dismissals for a check. Returns (item_key, reason) pairs.
    pub(crate) fn list_dismissed_keys_impl(
        &self,
        check_name: &str,
    ) -> Result<Vec<(String, String)>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT item_key, reason FROM health_dismissals
             WHERE check_name = ?1
               AND (expires_at IS NULL OR expires_at > ?2)",
        )?;
        let rows = stmt
            .query_map(params![check_name, now], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Insert or update a dismissal (upsert on unique index).
    pub(crate) fn dismiss_health_item_impl(
        &self,
        check_name: &str,
        item_key: &str,
        reason: &str,
        expires_at: Option<&str>,
    ) -> Result<(), AtomicCoreError> {
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let tx = conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        tx.execute(
            "INSERT INTO health_dismissals (id, check_name, item_key, reason, dismissed_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(check_name, item_key) DO UPDATE SET
                 reason = excluded.reason,
                 dismissed_at = excluded.dismissed_at,
                 expires_at = excluded.expires_at",
            params![id, check_name, item_key, reason, now, expires_at],
        )?;
        tx.commit()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    pub(crate) fn undismiss_health_item_impl(
        &self,
        check_name: &str,
        item_key: &str,
    ) -> Result<(), AtomicCoreError> {
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        tx.execute(
            "DELETE FROM health_dismissals WHERE check_name = ?1 AND item_key = ?2",
            params![check_name, item_key],
        )?;
        tx.commit()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    /// Delete stale dismissal rows: expired TTL, orphaned atom refs, orphaned tag refs.
    pub(crate) fn gc_dismissals_impl(&self) -> Result<u64, AtomicCoreError> {
        let mut conn = self.db.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();
        let tx = conn.transaction().map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let mut total: u64 = 0;

        // 1. Expired dismissals (defer TTL passed)
        total += tx.execute(
            "DELETE FROM health_dismissals WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now],
)? as u64;

        // 2a. Per-atom checks
        total += tx.execute(
            "DELETE FROM health_dismissals
             WHERE check_name IN ('boilerplate_pollution', 'content_quality')
               AND item_key NOT IN (SELECT id FROM atoms)",
            [],
)? as u64;

        // 2b. tag_health dismissals pointing at deleted tags
        total += tx.execute(
            "DELETE FROM health_dismissals
             WHERE check_name = 'tag_health'
               AND item_key NOT IN (SELECT id FROM tags)",
            [],
)? as u64;

        // 2c. Pair-keyed checks — delete if either half atom is gone
        total += tx.execute(
            "DELETE FROM health_dismissals
             WHERE check_name IN ('content_overlap', 'contradiction_detection')
               AND (
                 instr(item_key, '__') = 0
                 OR substr(item_key, 1, instr(item_key, '__') - 1) NOT IN (SELECT id FROM atoms)
                 OR substr(item_key, instr(item_key, '__') + 2) NOT IN (SELECT id FROM atoms)
               )",
            [],
)? as u64;

        tx.commit().map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(total)
    }
    /// Persist a new tag proposal.
    pub(crate) fn save_tag_proposal_impl(
        &self,
        proposal: &crate::health::TagProposal,
    ) -> Result<(), AtomicCoreError> {
        let actions_json = serde_json::to_string(&proposal.actions)
            .map_err(|e| AtomicCoreError::Validation(e.to_string()))?;
        let mut conn = self.db.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = conn.transaction().map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        tx.execute(
            "INSERT INTO tag_proposals (id, summary, actions_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![proposal.id, proposal.summary, actions_json, proposal.generated_at],
        )?;
        tx.commit().map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    /// Load a proposal by ID.
    pub(crate) fn get_tag_proposal_impl(
        &self,
        id: &str,
    ) -> Result<Option<crate::health::TagProposal>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let result = conn.query_row(
            "SELECT id, summary, actions_json, created_at FROM tag_proposals WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        );
        match result {
            Ok((id, summary, actions_json, generated_at)) => {
                let actions: Vec<crate::health::TagProposalAction> =
                    serde_json::from_str(&actions_json)
                        .map_err(|e| AtomicCoreError::Validation(e.to_string()))?;
                Ok(Some(crate::health::TagProposal { id, summary, actions, generated_at }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::DatabaseOperation(e.to_string())),
        }
    }

    /// Load the latest un-applied proposal.
    pub(crate) fn get_latest_tag_proposal_impl(
        &self,
    ) -> Result<Option<crate::health::TagProposal>, AtomicCoreError> {
        let conn = self.db.read_conn()?;
        let result = conn.query_row(
            "SELECT id, summary, actions_json, created_at FROM tag_proposals WHERE applied_at IS NULL ORDER BY created_at DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        );
        match result {
            Ok((id, summary, actions_json, generated_at)) => {
                let actions: Vec<crate::health::TagProposalAction> =
                    serde_json::from_str(&actions_json)
                        .map_err(|e| AtomicCoreError::Validation(e.to_string()))?;
                Ok(Some(crate::health::TagProposal { id, summary, actions, generated_at }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::DatabaseOperation(e.to_string())),
        }
    }

    /// Mark a proposal as applied.
    pub(crate) fn mark_tag_proposal_applied_impl(
        &self,
        id: &str,
    ) -> Result<(), AtomicCoreError> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.db.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = conn.transaction().map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        tx.execute(
            "UPDATE tag_proposals SET applied_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        tx.commit().map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }
}