//! SQLite storage for daily briefings and briefing citations.
//!
//! Mirrors the wiki storage layout: a briefing row plus a set of citation
//! rows, with source_url joined in from the atoms table on read so clients
//! can render citations consistently.

use super::SqliteStorage;
use crate::briefing::{Briefing, BriefingCitation, BriefingWithCitations};
use crate::error::AtomicCoreError;
use crate::models::AtomWithTags;
use crate::storage::traits::{BriefingStore, StorageResult};
use async_trait::async_trait;

impl SqliteStorage {
    // ==================== Atom fetch helpers for briefing ====================

    /// Fetch up to `limit` atoms with `created_at > since`, newest first.
    /// Returns full `AtomWithTags` rows so the agent prompt can include tags.
    pub(crate) fn list_new_atoms_since_sync(
        &self,
        since: &str,
        limit: i32,
    ) -> StorageResult<Vec<AtomWithTags>> {
        use crate::models::{Atom, Tag};
        let conn = self.db.read_conn()?;

        let mut stmt = conn.prepare(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at, embedding_status, tagging_status,
                    embedding_error, tagging_error
             FROM atoms
             WHERE created_at > ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;

        let atoms: Vec<Atom> = stmt
            .query_map(rusqlite::params![since, limit], |row| {
                Ok(Atom {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    title: row.get(2)?,
                    snippet: row.get(3)?,
                    source_url: row.get(4)?,
                    source: row.get(5)?,
                    published_at: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                    embedding_status: row.get(9)?,
                    tagging_status: row.get(10)?,
                    embedding_error: row.get(11)?,
                    tagging_error: row.get(12)?,
                    is_locked: false,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if atoms.is_empty() {
            return Ok(Vec::new());
        }

        // Batch fetch tags for all atoms
        let atom_ids: Vec<&str> = atoms.iter().map(|a| a.id.as_str()).collect();
        let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut tag_map: std::collections::HashMap<String, Vec<Tag>> =
            std::collections::HashMap::new();
        let tag_sql = format!(
            "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
             FROM atom_tags at
             INNER JOIN tags t ON t.id = at.tag_id
             WHERE at.atom_id IN ({})",
            placeholders
        );
        let mut tag_stmt = conn.prepare(&tag_sql)?;
        let rows = tag_stmt.query_map(rusqlite::params_from_iter(atom_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                Tag {
                    id: row.get(1)?,
                    name: row.get(2)?,
                    parent_id: row.get(3)?,
                    created_at: row.get(4)?,
                    is_autotag_target: row.get::<_, i64>(5).unwrap_or(0) != 0,
                    autotag_description: row.get(6)?,
                },
            ))
        })?;
        for row in rows {
            let (atom_id, tag) = row?;
            tag_map.entry(atom_id).or_default().push(tag);
        }

        Ok(atoms
            .into_iter()
            .map(|a| {
                let tags = tag_map.remove(&a.id).unwrap_or_default();
                AtomWithTags { atom: a, tags }
            })
            .collect())
    }

    /// Count atoms with `created_at > since`. Reported to the agent so it
    /// knows whether it's seeing all the new material or a truncated slice.
    pub(crate) fn count_new_atoms_since_sync(&self, since: &str) -> StorageResult<i32> {
        let conn = self.db.read_conn()?;
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE created_at > ?1",
            rusqlite::params![since],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    // ==================== Briefing CRUD ====================

    pub(crate) fn insert_briefing_sync(
        &self,
        briefing: &Briefing,
        citations: &[BriefingCitation],
    ) -> StorageResult<BriefingWithCitations> {
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO briefings (id, content, created_at, atom_count, last_run_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                briefing.id,
                briefing.content,
                briefing.created_at,
                briefing.atom_count,
                briefing.last_run_at,
            ],
        )?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO briefing_citations (id, briefing_id, citation_index, atom_id, excerpt)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for c in citations {
                stmt.execute(rusqlite::params![
                    c.id,
                    briefing.id,
                    c.citation_index,
                    c.atom_id,
                    c.excerpt,
                ])?;
            }
        }

        tx.commit()?;
        drop(conn);

        // Read back with JOINed source_url
        self.get_briefing_sync(&briefing.id)?.ok_or_else(|| {
            AtomicCoreError::DatabaseOperation(format!(
                "Briefing {} vanished after insert",
                briefing.id
            ))
        })
    }

    /// Fetch a single briefing by id, joining citations with source_url.
    pub(crate) fn get_briefing_sync(
        &self,
        id: &str,
    ) -> StorageResult<Option<BriefingWithCitations>> {
        let conn = self.db.read_conn()?;

        let briefing = conn
            .query_row(
                "SELECT id, content, created_at, atom_count, last_run_at
                 FROM briefings WHERE id = ?1",
                rusqlite::params![id],
                |row| {
                    Ok(Briefing {
                        id: row.get(0)?,
                        content: row.get(1)?,
                        created_at: row.get(2)?,
                        atom_count: row.get(3)?,
                        last_run_at: row.get(4)?,
                    })
                },
            )
            .ok();

        let Some(briefing) = briefing else {
            return Ok(None);
        };

        let mut stmt = conn.prepare(
            "SELECT bc.id, bc.briefing_id, bc.citation_index, bc.atom_id, bc.excerpt, a.source_url
             FROM briefing_citations bc
             LEFT JOIN atoms a ON a.id = bc.atom_id
             WHERE bc.briefing_id = ?1
             ORDER BY bc.citation_index",
        )?;
        let citations: Vec<BriefingCitation> = stmt
            .query_map(rusqlite::params![id], |row| {
                Ok(BriefingCitation {
                    id: row.get(0)?,
                    briefing_id: row.get(1)?,
                    citation_index: row.get(2)?,
                    atom_id: row.get(3)?,
                    excerpt: row.get(4)?,
                    source_url: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(BriefingWithCitations {
            briefing,
            citations,
        }))
    }

    /// Fetch the most recent briefing (by `created_at`), joined with citations.
    pub(crate) fn get_latest_briefing_sync(&self) -> StorageResult<Option<BriefingWithCitations>> {
        let conn = self.db.read_conn()?;

        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM briefings ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        drop(conn);

        let Some(id) = id else {
            return Ok(None);
        };
        self.get_briefing_sync(&id)
    }

    /// List recent briefings (without citations) for a lightweight history view.
    pub(crate) fn list_briefings_sync(&self, limit: i32) -> StorageResult<Vec<Briefing>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, content, created_at, atom_count, last_run_at
             FROM briefings
             ORDER BY created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![limit], |row| {
                Ok(Briefing {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    created_at: row.get(2)?,
                    atom_count: row.get(3)?,
                    last_run_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete a briefing and its citations (FK cascade handles the citations).
    pub(crate) fn delete_briefing_sync(&self, id: &str) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute("DELETE FROM briefings WHERE id = ?1", rusqlite::params![id])?;
        Ok(())
    }
}

#[async_trait]
impl BriefingStore for SqliteStorage {
    async fn list_new_atoms_since(
        &self,
        since: &str,
        limit: i32,
    ) -> StorageResult<Vec<AtomWithTags>> {
        let storage = self.clone();
        let since = since.to_string();
        tokio::task::spawn_blocking(move || storage.list_new_atoms_since_sync(&since, limit))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn count_new_atoms_since(&self, since: &str) -> StorageResult<i32> {
        let storage = self.clone();
        let since = since.to_string();
        tokio::task::spawn_blocking(move || storage.count_new_atoms_since_sync(&since))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn insert_briefing(
        &self,
        briefing: &Briefing,
        citations: &[BriefingCitation],
    ) -> StorageResult<BriefingWithCitations> {
        let storage = self.clone();
        let briefing = briefing.clone();
        let citations = citations.to_vec();
        tokio::task::spawn_blocking(move || storage.insert_briefing_sync(&briefing, &citations))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_latest_briefing(&self) -> StorageResult<Option<BriefingWithCitations>> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.get_latest_briefing_sync())
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_briefing(&self, id: &str) -> StorageResult<Option<BriefingWithCitations>> {
        let storage = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || storage.get_briefing_sync(&id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_briefings(&self, limit: i32) -> StorageResult<Vec<Briefing>> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.list_briefings_sync(limit))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn delete_briefing(&self, id: &str) -> StorageResult<()> {
        let storage = self.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || storage.delete_briefing_sync(&id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }
}
