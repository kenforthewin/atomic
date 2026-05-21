use std::collections::HashSet;

use super::SqliteStorage;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use crate::{
    atom_from_row, atom_links, extract_title_and_snippet, get_all_atom_tags_map,
    get_all_average_embeddings, get_atom_tags_map_for_ids, get_tags_for_atom, parse_source,
    CreateAtomRequest, ListAtomsParams, UpdateAtomRequest, ATOM_COLUMNS, ATOM_COLUMNS_A,
};
use async_trait::async_trait;
use rusqlite::OptionalExtension;

fn escape_like_pattern(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Insert the FTS row for an atom. Call after the corresponding row has been
/// inserted into `atoms` so the external-content select finds the current state.
pub(super) fn atoms_fts_insert(conn: &rusqlite::Connection, atom_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO atoms_fts(rowid, id, content)
         SELECT rowid, id, content FROM atoms WHERE id = ?1",
        [atom_id],
    )?;
    Ok(())
}

/// Remove the FTS row for an atom. Call BEFORE updating or deleting the atoms
/// row so FTS5's external-content delete sees the *current* indexed content.
fn atoms_fts_delete(conn: &rusqlite::Connection, atom_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO atoms_fts(atoms_fts, rowid, id, content)
         SELECT 'delete', rowid, id, content FROM atoms WHERE id = ?1",
        [atom_id],
    )?;
    Ok(())
}

fn replace_atom_links_for_content(
    conn: &rusqlite::Connection,
    source_atom_id: &str,
    content: &str,
    now: &str,
) -> StorageResult<()> {
    conn.execute(
        "DELETE FROM atom_links WHERE source_atom_id = ?1",
        [source_atom_id],
    )?;

    for token in atom_links::extract_atom_link_tokens(content) {
        let is_atom_id = atom_links::is_uuid_target(&token.raw_target);
        let target_exists = if is_atom_id {
            conn.query_row(
                "SELECT 1 FROM atoms WHERE id = ?1",
                [&token.raw_target],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        } else {
            false
        };
        let target_atom_id = target_exists.then(|| token.raw_target.clone());
        let target_kind = if is_atom_id { "atom_id" } else { "text" };
        let status = if target_exists {
            "resolved"
        } else if is_atom_id {
            "missing"
        } else {
            "unresolved"
        };
        let link_id = uuid::Uuid::new_v4().to_string();

        conn.execute(
            "INSERT INTO atom_links (
                id, source_atom_id, target_atom_id, raw_target, label,
                target_kind, status, start_offset, end_offset, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            (
                &link_id,
                source_atom_id,
                &target_atom_id,
                &token.raw_target,
                &token.label,
                target_kind,
                status,
                token.start_offset as i32,
                token.end_offset as i32,
                now,
                now,
            ),
        )?;
    }

    Ok(())
}

fn mark_incoming_atom_links_missing(
    conn: &rusqlite::Connection,
    target_atom_id: &str,
    now: &str,
) -> StorageResult<()> {
    conn.execute(
        "UPDATE atom_links
         SET target_atom_id = NULL, status = 'missing', updated_at = ?1
         WHERE target_atom_id = ?2",
        (now, target_atom_id),
    )?;
    Ok(())
}

impl SqliteStorage {
    pub(crate) fn count_atoms_impl(&self) -> StorageResult<i32> {
        let conn = self.db.read_conn()?;
        let count: i32 = conn.query_row("SELECT COUNT(*) FROM atoms", [], |row| row.get(0))?;
        Ok(count)
    }

    pub(crate) fn get_all_atoms_impl(&self) -> StorageResult<Vec<AtomWithTags>> {
        let conn = self.db.read_conn()?;

        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM atoms ORDER BY updated_at DESC",
            ATOM_COLUMNS
        ))?;

        let atoms: Vec<Atom> = stmt
            .query_map([], atom_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let tag_map = get_all_atom_tags_map(&conn)?;

        let result: Vec<AtomWithTags> = atoms
            .into_iter()
            .map(|atom| {
                let tags = tag_map.get(&atom.id).cloned().unwrap_or_default();
                AtomWithTags { atom, tags }
            })
            .collect();

        Ok(result)
    }

    pub(crate) fn get_atom_impl(&self, id: &str) -> StorageResult<Option<AtomWithTags>> {
        let conn = self.db.read_conn()?;

        let atom_result = conn.query_row(
            &format!("SELECT {} FROM atoms WHERE id = ?1", ATOM_COLUMNS),
            [id],
            atom_from_row,
        );

        match atom_result {
            Ok(atom) => {
                let tags = get_tags_for_atom(&conn, id)?;
                Ok(Some(AtomWithTags { atom, tags }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::Database(e)),
        }
    }

    pub(crate) fn insert_atom_impl(
        &self,
        id: &str,
        request: &CreateAtomRequest,
        created_at: &str,
    ) -> StorageResult<AtomWithTags> {
        let embedding_status = "pending";
        let (title, snippet) = extract_title_and_snippet(&request.content, 300);
        let source = request.source_url.as_deref().map(parse_source);

        {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

            conn.execute_batch("BEGIN")?;

            if let Err(e) = (|| -> Result<(), AtomicCoreError> {
                conn.execute(
                    "INSERT INTO atoms (id, content, source_url, source, published_at, created_at, updated_at, embedding_status, title, snippet)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    (
                        id,
                        &request.content,
                        &request.source_url,
                        &source,
                        &request.published_at,
                        created_at,
                        created_at,
                        &embedding_status,
                        &title,
                        &snippet,
                    ),
                )?;

                atoms_fts_insert(&conn, id)?;
                replace_atom_links_for_content(&conn, id, &request.content, created_at)?;

                for tag_id in &request.tag_ids {
                    conn.execute(
                        "INSERT INTO atom_tags (atom_id, tag_id, source) VALUES (?1, ?2, 'manual')",
                        (id, tag_id),
                    )?;
                }
                Ok(())
            })() {
                conn.execute_batch("ROLLBACK").ok();
                return Err(e);
            }

            conn.execute_batch("COMMIT")?;
        }

        let atom = Atom {
            id: id.to_string(),
            content: request.content.clone(),
            title,
            snippet,
            source_url: request.source_url.clone(),
            source,
            published_at: request.published_at.clone(),
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            embedding_status: embedding_status.to_string(),
            tagging_status: "pending".to_string(),
            embedding_error: None,
            tagging_error: None,
            // User-facing inserts only ever produce captured atoms. The
            // column default in V18 matches; this is the explicit form.
            kind: crate::models::AtomKind::Captured,
        };

        let tags = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            get_tags_for_atom(&conn, id)?
        };

        Ok(AtomWithTags { atom, tags })
    }

    pub(crate) fn insert_atoms_bulk_impl(
        &self,
        atoms: &[(String, CreateAtomRequest, String)],
    ) -> StorageResult<Vec<AtomWithTags>> {
        let mut atoms_with_tags: Vec<AtomWithTags> = Vec::with_capacity(atoms.len());

        {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

            conn.execute_batch("BEGIN")?;

            for (id, request, created_at) in atoms {
                let (title, snippet) = extract_title_and_snippet(&request.content, 300);
                let source = request.source_url.as_deref().map(parse_source);

                if let Err(e) = conn.execute(
                    "INSERT INTO atoms (id, content, source_url, source, published_at, created_at, updated_at, embedding_status, title, snippet)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    (
                        id,
                        &request.content,
                        &request.source_url,
                        &source,
                        &request.published_at,
                        created_at,
                        created_at,
                        &"pending",
                        &title,
                        &snippet,
                    ),
                ) {
                    conn.execute_batch("ROLLBACK")?;
                    return Err(AtomicCoreError::Database(e));
                }

                if let Err(e) = atoms_fts_insert(&conn, id) {
                    conn.execute_batch("ROLLBACK")?;
                    return Err(AtomicCoreError::Database(e));
                }

                for tag_id in &request.tag_ids {
                    if let Err(e) = conn.execute(
                        "INSERT INTO atom_tags (atom_id, tag_id, source) VALUES (?1, ?2, 'manual')",
                        (id, tag_id),
                    ) {
                        conn.execute_batch("ROLLBACK")?;
                        return Err(AtomicCoreError::Database(e));
                    }
                }

                let atom = Atom {
                    id: id.clone(),
                    content: request.content.clone(),
                    title,
                    snippet,
                    source_url: request.source_url.clone(),
                    source,
                    published_at: request.published_at.clone(),
                    created_at: created_at.clone(),
                    updated_at: created_at.clone(),
                    embedding_status: "pending".to_string(),
                    tagging_status: "pending".to_string(),
                    embedding_error: None,
                    tagging_error: None,
                    kind: crate::models::AtomKind::Captured,
                };

                atoms_with_tags.push(AtomWithTags { atom, tags: vec![] });
            }

            for (id, request, created_at) in atoms {
                if let Err(e) =
                    replace_atom_links_for_content(&conn, id, &request.content, created_at)
                {
                    conn.execute_batch("ROLLBACK")?;
                    return Err(e);
                }
            }

            conn.execute_batch("COMMIT")?;

            // Batch-resolve tags for all created atoms
            let atom_ids: Vec<String> = atoms_with_tags.iter().map(|a| a.atom.id.clone()).collect();
            let tag_map = get_atom_tags_map_for_ids(&conn, &atom_ids)?;
            for atom_with_tags in &mut atoms_with_tags {
                atom_with_tags.tags = tag_map
                    .get(&atom_with_tags.atom.id)
                    .cloned()
                    .unwrap_or_default();
            }
        }

        Ok(atoms_with_tags)
    }

    pub(crate) fn update_atom_impl(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        self.update_atom_inner(id, request, updated_at, true, None)
    }

    pub(crate) fn update_atom_if_unchanged_impl(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
        expected_updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        self.update_atom_inner(id, request, updated_at, true, Some(expected_updated_at))
    }

    /// Content-only update: saves content/metadata but does NOT reset embedding_status.
    /// Used by auto-save during inline editing.
    pub(crate) fn update_atom_content_only_impl(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        self.update_atom_inner(id, request, updated_at, false, None)
    }

    fn update_atom_inner(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
        reset_embedding_status: bool,
        expected_updated_at: Option<&str>,
    ) -> StorageResult<AtomWithTags> {
        let (title, snippet) = extract_title_and_snippet(&request.content, 300);
        let source = request.source_url.as_deref().map(parse_source);

        {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

            conn.execute_batch("BEGIN")?;

            if let Err(e) = (|| -> Result<(), AtomicCoreError> {
                if let Some(expected) = expected_updated_at {
                    let current: Option<String> = conn
                        .query_row("SELECT updated_at FROM atoms WHERE id = ?1", [id], |row| {
                            row.get(0)
                        })
                        .optional()?;

                    match current {
                        Some(current) if current == expected => {}
                        Some(_) => {
                            return Err(AtomicCoreError::Conflict(format!(
                                "Atom {} changed before edits could be saved; reload the atom and retry",
                                id
                            )));
                        }
                        None => return Err(AtomicCoreError::NotFound(format!("Atom {}", id))),
                    }
                }

                let content_changed = if reset_embedding_status {
                    true
                } else {
                    let existing: Option<String> = conn
                        .query_row("SELECT content FROM atoms WHERE id = ?1", [id], |row| {
                            row.get(0)
                        })
                        .optional()?;
                    existing.as_deref() != Some(request.content.as_str())
                };

                if reset_embedding_status {
                    atoms_fts_delete(&conn, id)?;
                    let changed = if let Some(expected) = expected_updated_at {
                        conn.execute(
                            "UPDATE atoms
                             SET content = ?1,
                                 source_url = ?2,
                                 source = ?3,
                                 published_at = ?4,
                                 updated_at = ?5,
                                 embedding_status = ?6,
                                 tagging_status = ?7,
                                 embedding_error = NULL,
                                 tagging_error = NULL,
                                 title = ?8,
                                 snippet = ?9
                             WHERE id = ?10 AND updated_at = ?11",
                            (
                                &request.content,
                                &request.source_url,
                                &source,
                                &request.published_at,
                                updated_at,
                                "pending",
                                "pending",
                                &title,
                                &snippet,
                                id,
                                expected,
                            ),
                        )?
                    } else {
                        conn.execute(
                            "UPDATE atoms
                             SET content = ?1,
                                 source_url = ?2,
                                 source = ?3,
                                 published_at = ?4,
                                 updated_at = ?5,
                                 embedding_status = ?6,
                                 tagging_status = ?7,
                                 embedding_error = NULL,
                                 tagging_error = NULL,
                                 title = ?8,
                                 snippet = ?9
                             WHERE id = ?10",
                            (
                                &request.content,
                                &request.source_url,
                                &source,
                                &request.published_at,
                                updated_at,
                                "pending",
                                "pending",
                                &title,
                                &snippet,
                                id,
                            ),
                        )?
                    };
                    if changed == 0 {
                        return match expected_updated_at {
                            Some(_) => Err(AtomicCoreError::Conflict(format!(
                                "Atom {} changed before edits could be saved; reload the atom and retry",
                                id
                            ))),
                            None => Err(AtomicCoreError::NotFound(format!("Atom {}", id))),
                        };
                    }
                    atoms_fts_insert(&conn, id)?;
                    replace_atom_links_for_content(&conn, id, &request.content, updated_at)?;
                } else {
                    if content_changed {
                        atoms_fts_delete(&conn, id)?;
                        conn.execute(
                            "UPDATE atoms
                             SET content = ?1,
                                 source_url = ?2,
                                 source = ?3,
                                 published_at = ?4,
                                 updated_at = ?5,
                                 embedding_status = ?6,
                                 tagging_status = ?7,
                                 embedding_error = NULL,
                                 tagging_error = NULL,
                                 title = ?8,
                                 snippet = ?9
                             WHERE id = ?10",
                            (
                                &request.content,
                                &request.source_url,
                                &source,
                                &request.published_at,
                                updated_at,
                                "pending",
                                "pending",
                                &title,
                                &snippet,
                                id,
                            ),
                        )?;
                        atoms_fts_insert(&conn, id)?;
                        replace_atom_links_for_content(&conn, id, &request.content, updated_at)?;
                    } else {
                        // Content unchanged — FTS stays in sync without a resync.
                        conn.execute(
                            "UPDATE atoms SET content = ?1, source_url = ?2, source = ?3, published_at = ?4, updated_at = ?5,
                             title = ?6, snippet = ?7
                             WHERE id = ?8",
                            (
                                &request.content,
                                &request.source_url,
                                &source,
                                &request.published_at,
                                updated_at,
                                &title,
                                &snippet,
                                id,
                            ),
                        )?;
                        replace_atom_links_for_content(&conn, id, &request.content, updated_at)?;
                    }
                }

                if let Some(ref tag_ids) = request.tag_ids {
                    conn.execute("DELETE FROM atom_tags WHERE atom_id = ?1", [id])?;
                    for tag_id in tag_ids {
                        conn.execute(
                            "INSERT INTO atom_tags (atom_id, tag_id, source) VALUES (?1, ?2, 'manual')",
                            (id, tag_id),
                        )?;
                    }
                }
                Ok(())
            })() {
                conn.execute_batch("ROLLBACK").ok();
                return Err(e);
            }

            conn.execute_batch("COMMIT")?;
        }

        // Get the updated atom
        let atom = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            conn.query_row(
                &format!("SELECT {} FROM atoms WHERE id = ?1", ATOM_COLUMNS),
                [id],
                atom_from_row,
            )?
        };

        let tags = {
            let conn = self
                .db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            get_tags_for_atom(&conn, id)?
        };

        Ok(AtomWithTags { atom, tags })
    }

    pub(crate) fn delete_atom_impl(&self, id: &str) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        // Explicit delete from atom_tags so the trigger decrements tags.atom_count.
        // (FK CASCADE is off, so this won't happen automatically.)
        conn.execute("DELETE FROM atom_tags WHERE atom_id = ?1", [id])?;
        conn.execute("DELETE FROM atom_links WHERE source_atom_id = ?1", [id])?;
        mark_incoming_atom_links_missing(&conn, id, &chrono::Utc::now().to_rfc3339())?;
        // Remove FTS entry *before* deleting the atoms row so external-content
        // delete sees the live content.
        atoms_fts_delete(&conn, id)?;
        conn.execute("DELETE FROM atoms WHERE id = ?1", [id])?;

        Ok(())
    }

    pub(crate) fn get_atoms_by_tag_impl(
        &self,
        tag_id: &str,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<AtomWithTags>> {
        let conn = self.db.read_conn()?;

        let (kind_frag, kind_binds) = kinds.sqlite_in_clause("a.kind");
        let mut stmt = conn.prepare(&format!(
            "WITH RECURSIVE descendant_tags(id) AS (
                SELECT ?1
                UNION ALL
                SELECT t.id FROM tags t
                INNER JOIN descendant_tags dt ON t.parent_id = dt.id
            )
            SELECT {ATOM_COLUMNS_A}
            FROM atom_tags at
            INNER JOIN atoms a ON a.id = at.atom_id
            WHERE at.tag_id IN (SELECT id FROM descendant_tags)
              AND {kind_frag}
            GROUP BY a.id
            ORDER BY a.updated_at DESC",
        ))?;

        // tag_id first, then the kind bind values.
        let mut bind_values: Vec<&dyn rusqlite::ToSql> = vec![&tag_id];
        for v in &kind_binds {
            bind_values.push(v as &dyn rusqlite::ToSql);
        }
        let atoms: Vec<Atom> = stmt
            .query_map(
                rusqlite::params_from_iter(bind_values.iter()),
                atom_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;

        // Batch load tags for the fetched atoms
        let atom_ids: Vec<String> = atoms.iter().map(|a| a.id.clone()).collect();
        let tag_map = get_atom_tags_map_for_ids(&conn, &atom_ids)?;

        let result: Vec<AtomWithTags> = atoms
            .into_iter()
            .map(|atom| {
                let tags = tag_map.get(&atom.id).cloned().unwrap_or_default();
                AtomWithTags { atom, tags }
            })
            .collect();

        Ok(result)
    }

    /// Resolve a report's source-scope atom set in a single recursive CTE.
    ///
    /// Three branches by `tag_ids`:
    /// - empty: skip the CTE entirely (linear scan with optional time + kind
    ///   filters).
    /// - non-empty: recursive subtree expansion over multiple roots.
    ///
    /// Centralizes the scope query so the reports runner and any future
    /// scope-preview UI use exactly the same predicate the runner does.
    pub(crate) fn list_atoms_for_report_scope_sync(
        &self,
        tag_ids: &[String],
        since: Option<&str>,
        kinds: &crate::models::KindFilter,
        limit: Option<i32>,
    ) -> StorageResult<Vec<AtomWithTags>> {
        let conn = self.db.read_conn()?;
        let (kind_frag, kind_binds) = kinds.sqlite_in_clause("a.kind");

        // Build SQL based on whether we have a tag scope. The shapes differ
        // enough that splitting is clearer than threading conditional
        // clauses through one big format!.
        let sql = if tag_ids.is_empty() {
            let since_pred = if since.is_some() {
                "AND a.created_at > ?"
            } else {
                ""
            };
            let limit_pred = if limit.is_some() { "LIMIT ?" } else { "" };
            format!(
                "SELECT {ATOM_COLUMNS_A}
                 FROM atoms a
                 WHERE {kind_frag}
                   {since_pred}
                 ORDER BY a.created_at DESC
                 {limit_pred}"
            )
        } else {
            let tag_placeholders = (0..tag_ids.len())
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let since_pred = if since.is_some() {
                "AND a.created_at > ?"
            } else {
                ""
            };
            let limit_pred = if limit.is_some() { "LIMIT ?" } else { "" };
            format!(
                "WITH RECURSIVE descendant_tags(id) AS (
                    SELECT id FROM tags WHERE id IN ({tag_placeholders})
                    UNION ALL
                    SELECT t.id FROM tags t
                    INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                 )
                 SELECT {ATOM_COLUMNS_A}
                 FROM atoms a
                 WHERE EXISTS (
                     SELECT 1 FROM atom_tags at
                     WHERE at.atom_id = a.id
                       AND at.tag_id IN (SELECT id FROM descendant_tags)
                 )
                   AND {kind_frag}
                   {since_pred}
                 GROUP BY a.id
                 ORDER BY a.created_at DESC
                 {limit_pred}"
            )
        };

        let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::new();
        // tag roots first so they match the placeholders in the CTE.
        for t in tag_ids {
            binds.push(t as &dyn rusqlite::ToSql);
        }
        for v in &kind_binds {
            binds.push(v as &dyn rusqlite::ToSql);
        }
        if let Some(s) = &since {
            binds.push(s as &dyn rusqlite::ToSql);
        }
        if let Some(l) = &limit {
            binds.push(l as &dyn rusqlite::ToSql);
        }

        let mut stmt = conn.prepare(&sql)?;
        let atoms: Vec<Atom> = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), atom_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let atom_ids: Vec<String> = atoms.iter().map(|a| a.id.clone()).collect();
        let tag_map = get_atom_tags_map_for_ids(&conn, &atom_ids)?;
        Ok(atoms
            .into_iter()
            .map(|atom| {
                let tags = tag_map.get(&atom.id).cloned().unwrap_or_default();
                AtomWithTags { atom, tags }
            })
            .collect())
    }

    pub(crate) fn count_atoms_for_report_scope_sync(
        &self,
        tag_ids: &[String],
        since: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<i32> {
        let conn = self.db.read_conn()?;
        let (kind_frag, kind_binds) = kinds.sqlite_in_clause("a.kind");

        let sql = if tag_ids.is_empty() {
            let since_pred = if since.is_some() {
                "AND a.created_at > ?"
            } else {
                ""
            };
            format!("SELECT COUNT(*) FROM atoms a WHERE {kind_frag} {since_pred}")
        } else {
            let tag_placeholders = (0..tag_ids.len())
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let since_pred = if since.is_some() {
                "AND a.created_at > ?"
            } else {
                ""
            };
            format!(
                "WITH RECURSIVE descendant_tags(id) AS (
                    SELECT id FROM tags WHERE id IN ({tag_placeholders})
                    UNION ALL
                    SELECT t.id FROM tags t
                    INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                 )
                 SELECT COUNT(DISTINCT a.id) FROM atoms a
                 WHERE EXISTS (
                     SELECT 1 FROM atom_tags at
                     WHERE at.atom_id = a.id
                       AND at.tag_id IN (SELECT id FROM descendant_tags)
                 )
                   AND {kind_frag}
                   {since_pred}"
            )
        };

        let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::new();
        for t in tag_ids {
            binds.push(t as &dyn rusqlite::ToSql);
        }
        for v in &kind_binds {
            binds.push(v as &dyn rusqlite::ToSql);
        }
        if let Some(s) = &since {
            binds.push(s as &dyn rusqlite::ToSql);
        }

        let count: i32 = conn.query_row(&sql, rusqlite::params_from_iter(binds.iter()), |row| {
            row.get(0)
        })?;
        Ok(count)
    }

    pub(crate) fn list_atoms_impl(
        &self,
        params: &ListAtomsParams,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<PaginatedAtoms> {
        let conn = self.db.read_conn()?;
        let use_cursor = params.cursor.is_some() && params.cursor_id.is_some();

        // A non-All kind filter forces the slow count path because the
        // denormalized `tags.atom_count` is kind-blind. Source-value and
        // source-filter behave the same way; treat kinds as part of that
        // "extra filter" set.
        let has_kind_filter = !matches!(kinds, crate::models::KindFilter::All);
        let has_extra_filters = !matches!(params.source_filter, SourceFilter::All)
            || params.source_value.is_some()
            || has_kind_filter;

        // --- Build ORDER BY ---
        let sort_col = match params.sort_by {
            SortField::Updated => "a.updated_at",
            SortField::Created => "a.created_at",
            SortField::Published => "COALESCE(a.published_at, a.created_at)",
            SortField::Title => "a.title",
        };
        let sort_dir = match params.sort_order {
            SortOrder::Desc => "DESC",
            SortOrder::Asc => "ASC",
        };
        let cursor_cmp = match params.sort_order {
            SortOrder::Desc => "<",
            SortOrder::Asc => ">",
        };

        // --- Build WHERE clauses + bind values ---
        let mut where_clauses: Vec<String> = Vec::new();
        let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        // Tag filter — recursive CTE to include full descendant subtree
        if let Some(ref tid) = params.tag_id {
            where_clauses.push(format!(
                "EXISTS (SELECT 1 FROM atom_tags at WHERE at.atom_id = a.id AND at.tag_id IN (\
                 WITH RECURSIVE descendant_tags(id) AS (\
                   SELECT ?{p} \
                   UNION ALL \
                   SELECT t.id FROM tags t INNER JOIN descendant_tags dt ON t.parent_id = dt.id\
                 ) SELECT id FROM descendant_tags))",
                p = param_idx
            ));
            bind_values.push(Box::new(tid.clone()));
            param_idx += 1;
        }

        // Source filter
        match params.source_filter {
            SourceFilter::All => {}
            SourceFilter::Manual => {
                where_clauses.push("a.source IS NULL".to_string());
            }
            SourceFilter::External => {
                where_clauses.push("a.source IS NOT NULL".to_string());
            }
        }

        // Source value filter (specific source like "nytimes.com")
        if let Some(ref sv) = params.source_value {
            where_clauses.push(format!("a.source = ?{}", param_idx));
            bind_values.push(Box::new(sv.clone()));
            param_idx += 1;
        }

        // Cursor
        if use_cursor {
            where_clauses.push(format!(
                "({sort_col}, a.id) {cursor_cmp} (?{p1}, ?{p2})",
                sort_col = sort_col,
                cursor_cmp = cursor_cmp,
                p1 = param_idx,
                p2 = param_idx + 1,
            ));
            bind_values.push(Box::new(params.cursor.clone().unwrap()));
            bind_values.push(Box::new(params.cursor_id.clone().unwrap()));
            param_idx += 2;
        }

        // Kind filter (numbered placeholders to match this function's bind style)
        if let crate::models::KindFilter::Only(kind_vec) = kinds {
            if kind_vec.is_empty() {
                where_clauses.push("1 = 0".to_string());
            } else {
                let placeholders: Vec<String> = (0..kind_vec.len())
                    .map(|i| format!("?{}", param_idx + i))
                    .collect();
                where_clauses.push(format!("a.kind IN ({})", placeholders.join(", ")));
                for k in kind_vec {
                    bind_values.push(Box::new(k.as_str().to_string()));
                }
                param_idx += kind_vec.len();
            }
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        // --- Count query ---
        let total_count: i32 = if !has_extra_filters && params.tag_id.is_some() {
            // Fast path: use denormalized atom_count for tag-only filters
            let tid = params.tag_id.as_ref().unwrap();
            let has_children: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM tags WHERE parent_id = ?1)",
                rusqlite::params![tid],
                |row| row.get(0),
            )?;
            if has_children {
                conn.query_row(
                    "WITH RECURSIVE descendant_tags(id) AS (
                       SELECT ?1
                       UNION ALL
                       SELECT t.id FROM tags t INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                     )
                     SELECT COUNT(DISTINCT at.atom_id)
                     FROM atom_tags at
                     WHERE at.tag_id IN (SELECT id FROM descendant_tags)",
                    rusqlite::params![tid],
                    |row| row.get(0),
                )?
            } else {
                conn.query_row(
                    "SELECT atom_count FROM tags WHERE id = ?1",
                    rusqlite::params![tid],
                    |row| row.get(0),
                )?
            }
        } else if has_extra_filters || params.tag_id.is_some() {
            // Build count query with filters (no cursor/limit)
            let mut count_wheres: Vec<String> = Vec::new();
            let mut count_binds: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            let mut ci = 1;

            if let Some(ref tid) = params.tag_id {
                count_wheres.push(format!(
                    "EXISTS (SELECT 1 FROM atom_tags at WHERE at.atom_id = a.id AND at.tag_id IN (\
                     WITH RECURSIVE descendant_tags(id) AS (\
                       SELECT ?{p} \
                       UNION ALL \
                       SELECT t.id FROM tags t INNER JOIN descendant_tags dt ON t.parent_id = dt.id\
                     ) SELECT id FROM descendant_tags))",
                    p = ci
                ));
                count_binds.push(Box::new(tid.clone()));
                ci += 1;
            }
            match params.source_filter {
                SourceFilter::All => {}
                SourceFilter::Manual => count_wheres.push("a.source IS NULL".to_string()),
                SourceFilter::External => count_wheres.push("a.source IS NOT NULL".to_string()),
            }
            if let Some(ref sv) = params.source_value {
                count_wheres.push(format!("a.source = ?{}", ci));
                count_binds.push(Box::new(sv.clone()));
                ci += 1;
            }
            if let crate::models::KindFilter::Only(kind_vec) = kinds {
                if kind_vec.is_empty() {
                    count_wheres.push("1 = 0".to_string());
                } else {
                    let placeholders: Vec<String> = (0..kind_vec.len())
                        .map(|i| format!("?{}", ci + i))
                        .collect();
                    count_wheres.push(format!("a.kind IN ({})", placeholders.join(", ")));
                    for k in kind_vec {
                        count_binds.push(Box::new(k.as_str().to_string()));
                    }
                    // ci no longer used after this point in this branch
                }
            }
            let count_where = if count_wheres.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", count_wheres.join(" AND "))
            };
            let count_sql = format!("SELECT COUNT(*) FROM atoms a {}", count_where);
            let count_refs: Vec<&dyn rusqlite::types::ToSql> =
                count_binds.iter().map(|b| b.as_ref()).collect();
            conn.query_row(&count_sql, count_refs.as_slice(), |row| row.get(0))?
        } else {
            // No filters at all -- plain count
            conn.query_row("SELECT COUNT(*) FROM atoms", [], |row| row.get(0))?
        };

        // --- Data query ---
        // Bind values for LIMIT/OFFSET after cursor
        let limit_param = param_idx;
        bind_values.push(Box::new(params.limit));
        param_idx += 1;

        let data_sql = if use_cursor {
            format!(
                "SELECT a.id, a.title, a.snippet, a.source_url, a.source, a.published_at,
                        a.created_at, a.updated_at,
                        COALESCE(a.embedding_status, 'pending'), COALESCE(a.tagging_status, 'pending'),
                        a.embedding_error, a.tagging_error
                 FROM atoms a
                 {where_sql}
                 ORDER BY {sort_col} {sort_dir}, a.id {sort_dir}
                 LIMIT ?{limit_param}",
            )
        } else {
            let offset_param = param_idx;
            bind_values.push(Box::new(params.offset));
            // param_idx += 1;
            format!(
                "SELECT a.id, a.title, a.snippet, a.source_url, a.source, a.published_at,
                        a.created_at, a.updated_at,
                        COALESCE(a.embedding_status, 'pending'), COALESCE(a.tagging_status, 'pending'),
                        a.embedding_error, a.tagging_error
                 FROM atoms a
                 {where_sql}
                 ORDER BY {sort_col} {sort_dir}, a.id {sort_dir}
                 LIMIT ?{limit_param} OFFSET ?{offset_param}",
            )
        };

        let bind_refs: Vec<&dyn rusqlite::types::ToSql> =
            bind_values.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&data_sql)?;
        type AtomRow = (
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        );
        let atoms: Vec<AtomRow> = stmt
            .query_map(bind_refs.as_slice(), |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Batch load tags for the page
        let atom_ids: Vec<String> = atoms.iter().map(|a| a.0.clone()).collect();
        let tag_map = get_atom_tags_map_for_ids(&conn, &atom_ids)?;

        // Extract cursor from the last result for keyset pagination.
        // The cursor value must correspond to the active sort column.
        let (next_cursor, next_cursor_id) = atoms
            .last()
            .map(|last| {
                let cursor_val = match params.sort_by {
                    SortField::Updated => last.7.clone(), // updated_at
                    SortField::Created => last.6.clone(), // created_at
                    SortField::Published => last.5.clone().unwrap_or_else(|| last.6.clone()), // COALESCE(published_at, created_at)
                    SortField::Title => last.1.clone(), // title
                };
                (Some(cursor_val), Some(last.0.clone()))
            })
            .unwrap_or((None, None));

        let summaries: Vec<AtomSummary> = atoms
            .into_iter()
            .map(
                |(
                    id,
                    title,
                    snippet,
                    source_url,
                    source,
                    published_at,
                    created_at,
                    updated_at,
                    embedding_status,
                    tagging_status,
                    embedding_error,
                    tagging_error,
                )| {
                    let tags = tag_map.get(&id).cloned().unwrap_or_default();
                    AtomSummary {
                        id,
                        title,
                        snippet,
                        source_url,
                        source,
                        published_at,
                        created_at,
                        updated_at,
                        embedding_status,
                        tagging_status,
                        embedding_error,
                        tagging_error,
                        tags,
                    }
                },
            )
            .collect();

        Ok(PaginatedAtoms {
            atoms: summaries,
            total_count,
            limit: params.limit,
            offset: params.offset,
            next_cursor,
            next_cursor_id,
        })
    }

    pub(crate) fn get_source_list_impl(&self) -> StorageResult<Vec<SourceInfo>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT source, COUNT(*) as cnt FROM atoms WHERE source IS NOT NULL GROUP BY source ORDER BY cnt DESC",
        )?;
        let results = stmt
            .query_map([], |row| {
                Ok(SourceInfo {
                    source: row.get(0)?,
                    atom_count: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    pub(crate) fn get_atom_links_impl(&self, atom_id: &str) -> StorageResult<Vec<AtomLink>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT al.id,
                    al.source_atom_id,
                    al.target_atom_id,
                    target.title,
                    al.raw_target,
                    al.label,
                    al.target_kind,
                    al.status,
                    al.start_offset,
                    al.end_offset,
                    al.created_at,
                    al.updated_at
             FROM atom_links al
             LEFT JOIN atoms target ON target.id = al.target_atom_id
             WHERE al.source_atom_id = ?1
             ORDER BY al.start_offset ASC, al.created_at ASC",
        )?;
        let links = stmt
            .query_map([atom_id], |row| {
                Ok(AtomLink {
                    id: row.get(0)?,
                    source_atom_id: row.get(1)?,
                    target_atom_id: row.get(2)?,
                    target_title: row.get(3)?,
                    raw_target: row.get(4)?,
                    label: row.get(5)?,
                    target_kind: row.get(6)?,
                    status: row.get(7)?,
                    start_offset: row.get(8)?,
                    end_offset: row.get(9)?,
                    created_at: row.get(10)?,
                    updated_at: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(links)
    }

    pub(crate) fn suggest_atom_links_impl(
        &self,
        query: &str,
        limit: i32,
    ) -> StorageResult<Vec<AtomLinkSuggestion>> {
        let conn = self.db.read_conn()?;
        let query = query.trim();

        if query.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT id, title, snippet, updated_at
                 FROM atoms
                 WHERE TRIM(title) <> ''
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?1",
            )?;
            let suggestions = stmt
                .query_map([limit], |row| {
                    Ok(AtomLinkSuggestion {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        snippet: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(suggestions);
        }

        let escaped = escape_like_pattern(query);
        let contains = format!("%{}%", escaped);
        let prefix = format!("{}%", escaped);
        let exact = query.to_lowercase();
        let mut stmt = conn.prepare(
            "SELECT id, title, snippet, updated_at
             FROM atoms
             WHERE TRIM(title) <> ''
               AND title LIKE ?1 ESCAPE '\\'
             ORDER BY
               CASE
                 WHEN LOWER(title) = ?2 THEN 0
                 WHEN title LIKE ?3 ESCAPE '\\' THEN 1
                 ELSE 2
               END,
               updated_at DESC,
               id DESC
             LIMIT ?4",
        )?;
        let suggestions = stmt
            .query_map((&contains, &exact, &prefix, limit), |row| {
                Ok(AtomLinkSuggestion {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    snippet: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(suggestions)
    }

    pub(crate) fn get_embedding_status_impl(&self, atom_id: &str) -> StorageResult<String> {
        let conn = self.db.read_conn()?;

        let status: String = conn.query_row(
            "SELECT COALESCE(embedding_status, 'pending') FROM atoms WHERE id = ?1",
            [atom_id],
            |row| row.get(0),
        )?;

        Ok(status)
    }

    pub(crate) fn get_tagging_status_impl(&self, atom_id: &str) -> StorageResult<String> {
        let conn = self.db.read_conn()?;

        let status: String = conn.query_row(
            "SELECT COALESCE(tagging_status, 'pending') FROM atoms WHERE id = ?1",
            [atom_id],
            |row| row.get(0),
        )?;

        Ok(status)
    }

    pub(crate) fn get_atom_positions_impl(&self) -> StorageResult<Vec<AtomPosition>> {
        let conn = self.db.read_conn()?;

        let mut stmt = conn.prepare("SELECT atom_id, x, y FROM atom_positions")?;

        let positions = stmt
            .query_map([], |row| {
                Ok(AtomPosition {
                    atom_id: row.get(0)?,
                    x: row.get(1)?,
                    y: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(positions)
    }

    pub(crate) fn save_atom_positions_impl(&self, positions: &[AtomPosition]) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();

        let tx = conn.unchecked_transaction()?;
        for pos in positions {
            tx.execute(
                "INSERT OR REPLACE INTO atom_positions (atom_id, x, y, updated_at) VALUES (?1, ?2, ?3, ?4)",
                (&pos.atom_id, &pos.x, &pos.y, &now),
            )?;
        }
        tx.commit()?;

        Ok(())
    }

    pub(crate) fn get_atom_tag_ids_impl(&self, atom_id: &str) -> StorageResult<Vec<String>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare("SELECT tag_id FROM atom_tags WHERE atom_id = ?1")?;
        let ids = stmt
            .query_map([atom_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    /// Get distinct tag IDs for a batch of atoms in a single query.
    pub(crate) fn get_tag_ids_for_atoms_batch_impl(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<String>> {
        if atom_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.db.read_conn()?;
        let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT DISTINCT tag_id FROM atom_tags WHERE atom_id IN ({})",
            placeholders
        );
        let mut stmt = conn.prepare(&sql)?;
        let ids = stmt
            .query_map(rusqlite::params_from_iter(atom_ids.iter()), |row| {
                row.get(0)
            })?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    pub(crate) fn get_atom_content_impl(&self, atom_id: &str) -> StorageResult<Option<String>> {
        let conn = self.db.read_conn()?;
        match conn.query_row(
            "SELECT content FROM atoms WHERE id = ?1",
            [atom_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(content) => Ok(Some(content)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::Database(e)),
        }
    }

    pub(crate) fn get_atom_contents_batch_impl(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<(String, String)>> {
        if atom_ids.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.db.read_conn()?;
        let placeholders: String = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT id, content FROM atoms WHERE id IN ({})",
            placeholders
        );
        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(atom_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut results = Vec::with_capacity(atom_ids.len());
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub(crate) fn get_atoms_with_embeddings_impl(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<AtomWithEmbedding>> {
        let conn = self.db.read_conn()?;

        let (kind_frag, kind_binds) = kinds.sqlite_in_clause("kind");
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM atoms WHERE {kind_frag} ORDER BY updated_at DESC",
            ATOM_COLUMNS
        ))?;

        let bind_refs: Vec<&dyn rusqlite::ToSql> = kind_binds
            .iter()
            .map(|v| v as &dyn rusqlite::ToSql)
            .collect();
        let atoms: Vec<Atom> = stmt
            .query_map(rusqlite::params_from_iter(bind_refs.iter()), atom_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let tag_map = get_all_atom_tags_map(&conn)?;

        // Batch-load all embeddings in a single query
        let embedding_map = get_all_average_embeddings(&conn)?;

        let result = atoms
            .into_iter()
            .map(|atom| {
                let tags = tag_map.get(&atom.id).cloned().unwrap_or_default();
                let embedding = embedding_map.get(&atom.id).cloned();
                AtomWithEmbedding {
                    atom: AtomWithTags { atom, tags },
                    embedding,
                }
            })
            .collect();

        Ok(result)
    }

    pub(crate) fn check_existing_source_urls_sync(
        &self,
        urls: &[String],
    ) -> StorageResult<HashSet<String>> {
        if urls.is_empty() {
            return Ok(HashSet::new());
        }
        let conn = self.db.read_conn()?;
        let placeholders: String = urls.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT source_url FROM atoms WHERE source_url IN ({})",
            placeholders
        );
        let mut stmt = conn.prepare(&query)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(urls.iter()), |row| {
            row.get::<_, String>(0)
        })?;
        let mut result = HashSet::new();
        for row in rows {
            result.insert(row?);
        }
        Ok(result)
    }

    pub(crate) fn source_url_exists_sync(&self, url: &str) -> StorageResult<bool> {
        let conn = self.db.read_conn()?;
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM atoms WHERE source_url = ?1)",
                [url],
                |row| row.get(0),
            )
            .unwrap_or(false);
        Ok(exists)
    }

    pub(crate) fn get_atom_by_source_url_sync(
        &self,
        url: &str,
    ) -> StorageResult<Option<AtomWithTags>> {
        let conn = self.db.read_conn()?;

        let atom_result = conn.query_row(
            &format!("SELECT {} FROM atoms WHERE source_url = ?1", ATOM_COLUMNS),
            [url],
            atom_from_row,
        );

        match atom_result {
            Ok(atom) => {
                let tags = get_tags_for_atom(&conn, &atom.id)?;
                Ok(Some(AtomWithTags { atom, tags }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::Database(e)),
        }
    }

    pub(crate) fn count_pending_embeddings_sync(&self) -> StorageResult<i32> {
        let conn = self.db.read_conn()?;
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM atoms WHERE embedding_status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(count)
    }

    pub(crate) fn get_all_embedding_pairs_sync(&self) -> StorageResult<Vec<(String, Vec<f32>)>> {
        let conn = self.db.read_conn()?;
        let map = get_all_average_embeddings(&conn)?;
        Ok(map.into_iter().collect())
    }

    pub(crate) fn get_top_k_canvas_edges_sync(
        &self,
        top_k: usize,
    ) -> StorageResult<Vec<CanvasEdgeData>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT source_atom_id, target_atom_id, similarity_score
             FROM semantic_edges
             WHERE similarity_score >= 0.5
             ORDER BY similarity_score DESC",
        )?;

        let all_edges: Vec<(String, String, f32)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut per_atom: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut kept: Vec<(String, String, f32)> = Vec::new();

        for (src, tgt, score) in all_edges {
            let src_count = per_atom.get(&src).copied().unwrap_or(0);
            let tgt_count = per_atom.get(&tgt).copied().unwrap_or(0);
            if src_count >= top_k && tgt_count >= top_k {
                continue;
            }
            *per_atom.entry(src.clone()).or_insert(0) += 1;
            *per_atom.entry(tgt.clone()).or_insert(0) += 1;
            kept.push((src, tgt, score));
        }

        let min_w = kept.iter().map(|(_, _, w)| *w).fold(f32::MAX, f32::min);
        let max_w = kept.iter().map(|(_, _, w)| *w).fold(f32::MIN, f32::max);
        let range = (max_w - min_w).max(0.001);

        Ok(kept
            .into_iter()
            .map(|(src, tgt, score)| CanvasEdgeData {
                source: src,
                target: tgt,
                weight: (score - min_w) / range,
            })
            .collect())
    }

    pub(crate) fn get_all_atom_tag_ids_sync(
        &self,
    ) -> StorageResult<std::collections::HashMap<String, Vec<String>>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare("SELECT atom_id, tag_id FROM atom_tags")?;
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (atom_id, tag_id) = row?;
            map.entry(atom_id).or_default().push(tag_id);
        }
        Ok(map)
    }

    /// Lightweight canvas metadata: (atom_id, title, primary_tag_name, tag_count).
    /// No full content, no embedding blobs — just what the canvas needs.
    ///
    /// Uses a single LEFT JOIN + GROUP BY instead of correlated subqueries so
    /// the query is O(atoms + tag_links) rather than O(atoms × 2) index lookups.
    /// `primary_tag` is MIN(name) — alphabetically stable, unlike the previous
    /// LIMIT-1 nondeterministic pick.
    pub(crate) fn get_canvas_atom_metadata_light_sync(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<(String, String, Option<String>, i32, Option<String>)>> {
        let conn = self.db.read_conn()?;
        let (kind_frag, kind_binds) = kinds.sqlite_in_clause("a.kind");
        let mut stmt = conn.prepare(&format!(
            "SELECT a.id, a.title, MIN(t.name) AS primary_tag, COUNT(at.tag_id) AS tag_count, a.source_url
             FROM atoms a
             LEFT JOIN atom_tags at ON at.atom_id = a.id
             LEFT JOIN tags t ON t.id = at.tag_id
             WHERE a.embedding_status = 'complete'
               AND {kind_frag}
             GROUP BY a.id, a.title, a.source_url"
        ))?;

        let bind_refs: Vec<&dyn rusqlite::ToSql> = kind_binds
            .iter()
            .map(|v| v as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind_refs.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i32>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub(crate) fn get_canvas_atom_metadata_sync(&self) -> StorageResult<Vec<CanvasAtomPosition>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT ap.atom_id, ap.x, ap.y,
                    SUBSTR(a.content, 1, 80) as title,
                    (SELECT t.name FROM atom_tags at JOIN tags t ON at.tag_id = t.id WHERE at.atom_id = ap.atom_id LIMIT 1) as primary_tag,
                    (SELECT COUNT(*) FROM atom_tags at WHERE at.atom_id = ap.atom_id) as tag_count
             FROM atom_positions ap
             JOIN atoms a ON ap.atom_id = a.id"
        )?;

        let atoms = stmt
            .query_map([], |row| {
                let content: String = row.get(3)?;
                let (title, _) = extract_title_and_snippet(&content, 60);
                Ok(CanvasAtomPosition {
                    atom_id: row.get(0)?,
                    x: row.get(1)?,
                    y: row.get(2)?,
                    title,
                    primary_tag: row.get(4)?,
                    tag_count: row.get(5)?,
                    tag_ids: vec![],
                    source_url: None,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(atoms)
    }
}

#[async_trait]
impl AtomStore for SqliteStorage {
    async fn get_all_atoms(&self) -> StorageResult<Vec<AtomWithTags>> {
        self.get_all_atoms_impl()
    }

    async fn count_atoms(&self) -> StorageResult<i32> {
        self.count_atoms_impl()
    }

    async fn get_atom(&self, id: &str) -> StorageResult<Option<AtomWithTags>> {
        self.get_atom_impl(id)
    }

    async fn insert_atom(
        &self,
        id: &str,
        request: &CreateAtomRequest,
        created_at: &str,
    ) -> StorageResult<AtomWithTags> {
        self.insert_atom_impl(id, request, created_at)
    }

    async fn insert_atoms_bulk(
        &self,
        atoms: &[(String, CreateAtomRequest, String)],
    ) -> StorageResult<Vec<AtomWithTags>> {
        self.insert_atoms_bulk_impl(atoms)
    }

    async fn update_atom(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        self.update_atom_impl(id, request, updated_at)
    }

    async fn update_atom_if_unchanged(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
        expected_updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        self.update_atom_if_unchanged_impl(id, request, updated_at, expected_updated_at)
    }

    async fn update_atom_content_only(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        self.update_atom_content_only_impl(id, request, updated_at)
    }

    async fn delete_atom(&self, id: &str) -> StorageResult<()> {
        self.delete_atom_impl(id)
    }

    async fn get_atoms_by_tag(
        &self,
        tag_id: &str,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<AtomWithTags>> {
        self.get_atoms_by_tag_impl(tag_id, kinds)
    }

    async fn get_atom_links(&self, atom_id: &str) -> StorageResult<Vec<AtomLink>> {
        self.get_atom_links_impl(atom_id)
    }

    async fn suggest_atom_links(
        &self,
        query: &str,
        limit: i32,
    ) -> StorageResult<Vec<AtomLinkSuggestion>> {
        self.suggest_atom_links_impl(query, limit)
    }

    async fn list_atoms(
        &self,
        params: &ListAtomsParams,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<PaginatedAtoms> {
        self.list_atoms_impl(params, kinds)
    }

    async fn get_source_list(&self) -> StorageResult<Vec<SourceInfo>> {
        self.get_source_list_impl()
    }

    async fn get_embedding_status(&self, atom_id: &str) -> StorageResult<String> {
        self.get_embedding_status_impl(atom_id)
    }

    async fn get_tagging_status(&self, atom_id: &str) -> StorageResult<String> {
        self.get_tagging_status_impl(atom_id)
    }

    async fn get_atom_positions(&self) -> StorageResult<Vec<AtomPosition>> {
        self.get_atom_positions_impl()
    }

    async fn save_atom_positions(&self, positions: &[AtomPosition]) -> StorageResult<()> {
        self.save_atom_positions_impl(positions)
    }

    async fn get_atoms_with_embeddings(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<AtomWithEmbedding>> {
        self.get_atoms_with_embeddings_impl(kinds)
    }

    async fn get_atom_tag_ids(&self, atom_id: &str) -> StorageResult<Vec<String>> {
        self.get_atom_tag_ids_impl(atom_id)
    }

    async fn get_atom_content(&self, atom_id: &str) -> StorageResult<Option<String>> {
        self.get_atom_content_impl(atom_id)
    }

    async fn get_atom_contents_batch(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<(String, String)>> {
        self.get_atom_contents_batch_impl(atom_ids)
    }

    async fn check_existing_source_urls(&self, urls: &[String]) -> StorageResult<HashSet<String>> {
        self.check_existing_source_urls_sync(urls)
    }

    async fn source_url_exists(&self, url: &str) -> StorageResult<bool> {
        self.source_url_exists_sync(url)
    }

    async fn get_atom_by_source_url(&self, url: &str) -> StorageResult<Option<AtomWithTags>> {
        self.get_atom_by_source_url_sync(url)
    }

    async fn count_pending_embeddings(&self) -> StorageResult<i32> {
        self.count_pending_embeddings_sync()
    }

    async fn get_all_embedding_pairs(&self) -> StorageResult<Vec<(String, Vec<f32>)>> {
        self.get_all_embedding_pairs_sync()
    }

    async fn get_top_k_canvas_edges(&self, top_k: usize) -> StorageResult<Vec<CanvasEdgeData>> {
        self.get_top_k_canvas_edges_sync(top_k)
    }

    async fn get_all_atom_tag_ids(
        &self,
    ) -> StorageResult<std::collections::HashMap<String, Vec<String>>> {
        self.get_all_atom_tag_ids_sync()
    }

    async fn get_canvas_atom_metadata(&self) -> StorageResult<Vec<CanvasAtomPosition>> {
        self.get_canvas_atom_metadata_sync()
    }

    async fn get_canvas_atom_metadata_light(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<(String, String, Option<String>, i32, Option<String>)>> {
        self.get_canvas_atom_metadata_light_sync(kinds)
    }

    async fn list_atoms_for_report_scope(
        &self,
        tag_ids: &[String],
        since: Option<&str>,
        kinds: &crate::models::KindFilter,
        limit: Option<i32>,
    ) -> StorageResult<Vec<AtomWithTags>> {
        let storage = self.clone();
        let tag_ids = tag_ids.to_vec();
        let since = since.map(|s| s.to_string());
        let kinds = kinds.clone();
        tokio::task::spawn_blocking(move || {
            storage.list_atoms_for_report_scope_sync(&tag_ids, since.as_deref(), &kinds, limit)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn count_atoms_for_report_scope(
        &self,
        tag_ids: &[String],
        since: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<i32> {
        let storage = self.clone();
        let tag_ids = tag_ids.to_vec();
        let since = since.map(|s| s.to_string());
        let kinds = kinds.clone();
        tokio::task::spawn_blocking(move || {
            storage.count_atoms_for_report_scope_sync(&tag_ids, since.as_deref(), &kinds)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }
}
