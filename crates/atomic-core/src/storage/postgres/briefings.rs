//! Postgres storage for daily briefings.
//!
//! Mirrors `sqlite/briefings.rs` but binds `db_id` everywhere so multiple
//! databases can share a Postgres pool without leaking briefings across them.
//! Citations are read with a JOIN on `atoms` so `source_url` is populated
//! consistently with the SQLite read path.

use super::PostgresStorage;
use crate::briefing::{Briefing, BriefingCitation, BriefingWithCitations};
use crate::error::AtomicCoreError;
use crate::models::{Atom, AtomWithTags, Tag};
use crate::storage::traits::{BriefingStore, StorageResult};
use async_trait::async_trait;
use std::collections::HashMap;

#[async_trait]
impl BriefingStore for PostgresStorage {
    async fn list_new_atoms_since(
        &self,
        since: &str,
        limit: i32,
    ) -> StorageResult<Vec<AtomWithTags>> {
        let atoms: Vec<Atom> = sqlx::query_as::<
            _,
            (
                String,
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
            ),
        >(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at, embedding_status, tagging_status,
                    embedding_error, tagging_error
             FROM atoms
             WHERE created_at > $1 AND db_id = $2
             ORDER BY created_at DESC
             LIMIT $3",
        )
        .bind(since)
        .bind(&self.db_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        .into_iter()
        .map(
            |(
                id,
                content,
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
            )| Atom {
                id,
                content,
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
                is_locked: false,
            },
        )
        .collect();

        if atoms.is_empty() {
            return Ok(Vec::new());
        }

        let atom_ids: Vec<String> = atoms.iter().map(|a| a.id.clone()).collect();
        let tag_rows =
            sqlx::query_as::<_, (String, String, String, Option<String>, String, bool, String)>(
                "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
             FROM atom_tags at
             INNER JOIN tags t ON t.id = at.tag_id
             WHERE at.atom_id = ANY($1) AND at.db_id = $2",
            )
            .bind(&atom_ids)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut tag_map: HashMap<String, Vec<Tag>> = HashMap::new();
        for (atom_id, id, name, parent_id, created_at, is_autotag_target, autotag_description) in
            tag_rows
        {
            tag_map.entry(atom_id).or_default().push(Tag {
                id,
                name,
                parent_id,
                created_at,
                is_autotag_target,
                autotag_description,
            });
        }

        Ok(atoms
            .into_iter()
            .map(|a| {
                let tags = tag_map.remove(&a.id).unwrap_or_default();
                AtomWithTags { atom: a, tags }
            })
            .collect())
    }

    async fn count_new_atoms_since(&self, since: &str) -> StorageResult<i32> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM atoms WHERE created_at > $1 AND db_id = $2")
                .bind(since)
                .bind(&self.db_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(count as i32)
    }

    async fn insert_briefing(
        &self,
        briefing: &Briefing,
        citations: &[BriefingCitation],
    ) -> StorageResult<BriefingWithCitations> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query(
            "INSERT INTO briefings (id, content, created_at, atom_count, last_run_at, db_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&briefing.id)
        .bind(&briefing.content)
        .bind(&briefing.created_at)
        .bind(briefing.atom_count)
        .bind(&briefing.last_run_at)
        .bind(&self.db_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        for c in citations {
            sqlx::query(
                "INSERT INTO briefing_citations (id, briefing_id, citation_index, atom_id, excerpt, db_id)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&c.id)
            .bind(&briefing.id)
            .bind(c.citation_index)
            .bind(&c.atom_id)
            .bind(&c.excerpt)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        tx.commit()
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        self.get_briefing(&briefing.id).await?.ok_or_else(|| {
            AtomicCoreError::DatabaseOperation(format!(
                "Briefing {} vanished after insert",
                briefing.id
            ))
        })
    }

    async fn get_latest_briefing(&self) -> StorageResult<Option<BriefingWithCitations>> {
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM briefings WHERE db_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        match id {
            Some(id) => self.get_briefing(&id).await,
            None => Ok(None),
        }
    }

    async fn get_briefing(&self, id: &str) -> StorageResult<Option<BriefingWithCitations>> {
        let row = sqlx::query_as::<_, (String, String, String, i32, String)>(
            "SELECT id, content, created_at, atom_count, last_run_at
             FROM briefings WHERE id = $1 AND db_id = $2",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let Some((row_id, content, created_at, atom_count, last_run_at)) = row else {
            return Ok(None);
        };

        let briefing = Briefing {
            id: row_id,
            content,
            created_at,
            atom_count,
            last_run_at,
        };

        let citations: Vec<BriefingCitation> = sqlx::query_as::<
            _,
            (String, String, i32, String, String, Option<String>),
        >(
            "SELECT bc.id, bc.briefing_id, bc.citation_index, bc.atom_id, bc.excerpt, a.source_url
             FROM briefing_citations bc
             LEFT JOIN atoms a ON a.id = bc.atom_id AND a.db_id = $2
             WHERE bc.briefing_id = $1 AND bc.db_id = $2
             ORDER BY bc.citation_index",
        )
        .bind(id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        .into_iter()
        .map(
            |(id, briefing_id, citation_index, atom_id, excerpt, source_url)| BriefingCitation {
                id,
                briefing_id,
                citation_index,
                atom_id,
                excerpt,
                source_url,
            },
        )
        .collect();

        Ok(Some(BriefingWithCitations {
            briefing,
            citations,
        }))
    }

    async fn list_briefings(&self, limit: i32) -> StorageResult<Vec<Briefing>> {
        let rows = sqlx::query_as::<_, (String, String, String, i32, String)>(
            "SELECT id, content, created_at, atom_count, last_run_at
             FROM briefings
             WHERE db_id = $1
             ORDER BY created_at DESC
             LIMIT $2",
        )
        .bind(&self.db_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(
                |(id, content, created_at, atom_count, last_run_at)| Briefing {
                    id,
                    content,
                    created_at,
                    atom_count,
                    last_run_at,
                },
            )
            .collect())
    }

    async fn delete_briefing(&self, id: &str) -> StorageResult<()> {
        sqlx::query("DELETE FROM briefings WHERE id = $1 AND db_id = $2")
            .bind(id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }
}
