use super::SqliteStorage;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use crate::wiki;
use async_trait::async_trait;

/// Sync helper methods for wiki operations.
impl SqliteStorage {
    pub(crate) fn get_wiki_sync(
        &self,
        tag_id: &str,
    ) -> StorageResult<Option<WikiArticleWithCitations>> {
        let conn = self.db.read_conn()?;
        wiki::load_wiki_article(&conn, tag_id).map_err(|e| AtomicCoreError::Wiki(e))
    }

    pub(crate) fn get_wiki_status_sync(&self, tag_id: &str) -> StorageResult<WikiArticleStatus> {
        let conn = self.db.read_conn()?;
        wiki::get_article_status(&conn, tag_id).map_err(|e| AtomicCoreError::Wiki(e))
    }

    pub(crate) fn save_wiki_sync(
        &self,
        tag_id: &str,
        content: &str,
        citations: &[WikiCitation],
        atom_count: i32,
    ) -> StorageResult<WikiArticleWithCitations> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();

        let article = WikiArticle {
            id: id.clone(),
            tag_id: tag_id.to_string(),
            content: content.to_string(),
            created_at: now.clone(),
            updated_at: now,
            atom_count,
        };

        // save_wiki_article expects WikiLink slice; when saving via the trait
        // we don't have link extraction context, so pass an empty slice.
        wiki::save_wiki_article(&conn, &article, citations, &[])
            .map_err(|e| AtomicCoreError::Wiki(e))?;
        conn.execute("DELETE FROM wiki_articles_fts WHERE tag_id = ?1", [tag_id])?;
        conn.execute(
            "INSERT INTO wiki_articles_fts(id, tag_id, tag_name, content)
             SELECT w.id, w.tag_id, t.name, w.content
             FROM wiki_articles w
             JOIN tags t ON t.id = w.tag_id
             WHERE w.tag_id = ?1",
            [tag_id],
        )?;

        Ok(WikiArticleWithCitations {
            article,
            citations: citations.to_vec(),
        })
    }

    pub(crate) fn save_wiki_with_links_sync(
        &self,
        article: &WikiArticle,
        citations: &[WikiCitation],
        links: &[WikiLink],
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        wiki::save_wiki_article(&conn, article, citations, links)
            .map_err(|e| AtomicCoreError::Wiki(e))?;
        conn.execute(
            "DELETE FROM wiki_articles_fts WHERE tag_id = ?1",
            [&article.tag_id],
        )?;
        conn.execute(
            "INSERT INTO wiki_articles_fts(id, tag_id, tag_name, content)
             SELECT w.id, w.tag_id, t.name, w.content
             FROM wiki_articles w
             JOIN tags t ON t.id = w.tag_id
             WHERE w.tag_id = ?1",
            [&article.tag_id],
        )?;
        Ok(())
    }

    pub(crate) fn delete_wiki_sync(&self, tag_id: &str) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        wiki::delete_article(&conn, tag_id).map_err(|e| AtomicCoreError::Wiki(e))?;
        conn.execute("DELETE FROM wiki_articles_fts WHERE tag_id = ?1", [tag_id])?;
        Ok(())
    }

    pub(crate) fn get_wiki_links_sync(&self, tag_id: &str) -> StorageResult<Vec<WikiLink>> {
        let conn = self.db.read_conn()?;
        wiki::load_wiki_links(&conn, tag_id).map_err(|e| AtomicCoreError::Wiki(e))
    }

    pub(crate) fn list_wiki_versions_sync(
        &self,
        tag_id: &str,
    ) -> StorageResult<Vec<WikiVersionSummary>> {
        let conn = self.db.read_conn()?;
        wiki::list_wiki_versions(&conn, tag_id).map_err(|e| AtomicCoreError::Wiki(e))
    }

    pub(crate) fn get_wiki_version_sync(
        &self,
        version_id: &str,
    ) -> StorageResult<Option<WikiArticleVersion>> {
        let conn = self.db.read_conn()?;
        wiki::get_wiki_version(&conn, version_id).map_err(|e| AtomicCoreError::Wiki(e))
    }

    pub(crate) fn get_all_wiki_articles_sync(&self) -> StorageResult<Vec<WikiArticleSummary>> {
        let conn = self.db.read_conn()?;
        wiki::load_all_wiki_articles(&conn).map_err(|e| AtomicCoreError::Wiki(e))
    }

    pub(crate) fn get_wiki_source_chunks_sync(
        &self,
        tag_id: &str,
        max_source_tokens: usize,
    ) -> StorageResult<(Vec<ChunkWithContext>, i32)> {
        let conn = self.db.read_conn()?;

        // Get all descendant tag IDs (including the tag itself)
        let all_tag_ids =
            wiki::get_tag_hierarchy(&conn, tag_id).map_err(|e| AtomicCoreError::Wiki(e))?;

        if all_tag_ids.is_empty() {
            return Err(AtomicCoreError::Wiki(
                "No content found for this tag".to_string(),
            ));
        }

        // Wiki context-assembly is captured-only — and the kind filter MUST
        // apply at scope-resolution, not just at the final atom count. If we
        // filter only the count, the centroid/unranked chunk selectors below
        // still see report-kind atoms in `scoped_atom_ids` and can hand their
        // chunks to the LLM as wiki sources. Filter once, at the source.
        let captured = crate::models::KindFilter::only(crate::models::AtomKind::Captured);
        let (kind_frag, kind_binds) = captured.sqlite_in_clause("a.kind");
        let tag_placeholders = all_tag_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let atom_ids_query = format!(
            "SELECT DISTINCT at.atom_id
             FROM atom_tags at
             INNER JOIN atoms a ON a.id = at.atom_id
             WHERE at.tag_id IN ({tag_placeholders})
               AND {kind_frag}"
        );
        let mut stmt = conn.prepare(&atom_ids_query).map_err(|e| {
            AtomicCoreError::Wiki(format!("Failed to prepare atom_ids query: {}", e))
        })?;
        let mut bind_refs: Vec<&dyn rusqlite::ToSql> = all_tag_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        for v in &kind_binds {
            bind_refs.push(v as &dyn rusqlite::ToSql);
        }
        let scoped_atom_ids: std::collections::HashSet<String> = stmt
            .query_map(rusqlite::params_from_iter(bind_refs.iter()), |row| {
                row.get(0)
            })
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to query atom_ids: {}", e)))?
            .collect::<Result<std::collections::HashSet<_>, _>>()
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to collect atom_ids: {}", e)))?;

        if scoped_atom_ids.is_empty() {
            return Err(AtomicCoreError::Wiki(
                "No content found for this tag".to_string(),
            ));
        }

        // Try centroid-ranked retrieval
        let centroid_blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT embedding FROM tag_embeddings WHERE tag_id = ?1",
                [tag_id],
                |row| row.get(0),
            )
            .ok();

        let chunks = if let Some(ref centroid) = centroid_blob {
            wiki::centroid::select_chunks_by_centroid(
                &conn,
                centroid,
                &scoped_atom_ids,
                max_source_tokens,
            )
            .map_err(|e| AtomicCoreError::Wiki(e))?
        } else {
            tracing::debug!(
                tag_id,
                "[wiki/storage] No centroid for tag, falling back to unranked chunk selection"
            );
            wiki::centroid::select_chunks_unranked(&conn, &scoped_atom_ids, max_source_tokens)
                .map_err(|e| AtomicCoreError::Wiki(e))?
        };

        if chunks.is_empty() {
            return Err(AtomicCoreError::Wiki(
                "No content found for this tag".to_string(),
            ));
        }

        // Count must match the scope used above — both kind-filtered.
        let atom_count = wiki::count_atoms_with_tags(&conn, &all_tag_ids, &captured)
            .map_err(|e| AtomicCoreError::Wiki(e))?;

        Ok((chunks, atom_count))
    }

    pub(crate) fn get_wiki_update_chunks_sync(
        &self,
        tag_id: &str,
        last_update: &str,
        max_source_tokens: usize,
    ) -> StorageResult<Option<(Vec<ChunkWithContext>, i32)>> {
        let conn = self.db.read_conn()?;

        // New atoms scope must also be kind-filtered. An update can otherwise
        // start feeding report-kind atoms produced *after* the last article
        // revision back into the wiki LLM as "new sources to incorporate."
        let mut new_atom_stmt = conn
            .prepare(
                "WITH RECURSIVE descendant_tags(id) AS (
                     SELECT ?1
                     UNION ALL
                     SELECT t.id FROM tags t
                     INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                 )
                 SELECT DISTINCT a.id FROM atoms a
                 INNER JOIN atom_tags at ON a.id = at.atom_id
                 WHERE at.tag_id IN (SELECT id FROM descendant_tags)
                   AND a.created_at > ?2
                   AND a.kind = 'captured'",
            )
            .map_err(|e| {
                AtomicCoreError::Wiki(format!("Failed to prepare new atoms query: {}", e))
            })?;

        let new_atom_ids: Vec<String> = new_atom_stmt
            .query_map(rusqlite::params![tag_id, last_update], |row| row.get(0))
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to query new atoms: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to collect new atom IDs: {}", e)))?;

        if new_atom_ids.is_empty() {
            return Ok(None);
        }

        let new_atom_id_set: std::collections::HashSet<String> = new_atom_ids.into_iter().collect();

        // Try centroid-ranked selection scoped to new atoms only
        let centroid_blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT embedding FROM tag_embeddings WHERE tag_id = ?1",
                [tag_id],
                |row| row.get(0),
            )
            .ok();

        let mut new_chunks = if let Some(ref centroid) = centroid_blob {
            wiki::centroid::select_chunks_by_centroid(
                &conn,
                centroid,
                &new_atom_id_set,
                max_source_tokens,
            )
            .map_err(|e| AtomicCoreError::Wiki(e))?
        } else {
            tracing::debug!(tag_id, "[wiki/storage] No centroid for tag, falling back to unranked update chunk selection");
            wiki::centroid::select_chunks_unranked(&conn, &new_atom_id_set, max_source_tokens)
                .map_err(|e| AtomicCoreError::Wiki(e))?
        };

        if new_chunks.is_empty() && centroid_blob.is_some() {
            tracing::debug!(
                tag_id,
                "[wiki/storage] No centroid-ranked update chunks found, falling back to unranked update chunk selection"
            );
            new_chunks =
                wiki::centroid::select_chunks_unranked(&conn, &new_atom_id_set, max_source_tokens)
                    .map_err(|e| AtomicCoreError::Wiki(e))?;
        }

        if new_chunks.is_empty() {
            return Err(AtomicCoreError::Wiki(
                "New atoms are not ready for wiki update yet; chunking or embedding is still pending"
                    .to_string(),
            ));
        }

        // Count must be consistent with the source-chunk path: captured-only
        // total under the tag hierarchy. Otherwise the stored `atom_count`
        // tracks a different population than what generation/update sourced.
        let all_tag_ids =
            wiki::get_tag_hierarchy(&conn, tag_id).map_err(|e| AtomicCoreError::Wiki(e))?;
        let atom_count = wiki::count_atoms_with_tags(
            &conn,
            &all_tag_ids,
            &crate::models::KindFilter::only(crate::models::AtomKind::Captured),
        )
        .map_err(|e| AtomicCoreError::Wiki(e))?;

        Ok(Some((new_chunks, atom_count)))
    }

    pub(crate) fn save_wiki_proposal_sync(&self, proposal: &WikiProposal) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let citations_json = serde_json::to_string(&proposal.citations)
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to serialize citations: {}", e)))?;
        let ops_json = serde_json::to_string(&proposal.ops)
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to serialize ops: {}", e)))?;

        conn.execute(
            "INSERT INTO wiki_proposals
                (id, tag_id, base_article_id, base_updated_at, content,
                 citations_json, ops_json, new_atom_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(tag_id) DO UPDATE SET
                id = excluded.id,
                base_article_id = excluded.base_article_id,
                base_updated_at = excluded.base_updated_at,
                content = excluded.content,
                citations_json = excluded.citations_json,
                ops_json = excluded.ops_json,
                new_atom_count = excluded.new_atom_count,
                created_at = excluded.created_at",
            rusqlite::params![
                proposal.id,
                proposal.tag_id,
                proposal.base_article_id,
                proposal.base_updated_at,
                proposal.content,
                citations_json,
                ops_json,
                proposal.new_atom_count,
                proposal.created_at,
            ],
        )
        .map_err(|e| AtomicCoreError::Wiki(format!("Failed to save wiki proposal: {}", e)))?;

        Ok(())
    }

    pub(crate) fn get_wiki_proposal_sync(
        &self,
        tag_id: &str,
    ) -> StorageResult<Option<WikiProposal>> {
        let conn = self.db.read_conn()?;

        let mut stmt = conn
            .prepare(
                "SELECT id, tag_id, base_article_id, base_updated_at, content,
                        citations_json, ops_json, new_atom_count, created_at
                 FROM wiki_proposals
                 WHERE tag_id = ?1",
            )
            .map_err(|e| {
                AtomicCoreError::Wiki(format!("Failed to prepare get_wiki_proposal query: {}", e))
            })?;

        let row = stmt
            .query_row([tag_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i32>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })
            .ok();

        let Some((
            id,
            tag_id,
            base_article_id,
            base_updated_at,
            content,
            citations_json,
            ops_json,
            new_atom_count,
            created_at,
        )) = row
        else {
            return Ok(None);
        };

        let citations: Vec<WikiCitation> = serde_json::from_str(&citations_json)
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to parse citations_json: {}", e)))?;
        let ops: Vec<crate::wiki::WikiSectionOp> = serde_json::from_str(&ops_json)
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to parse ops_json: {}", e)))?;

        Ok(Some(WikiProposal {
            id,
            tag_id,
            base_article_id,
            base_updated_at,
            content,
            citations,
            ops,
            new_atom_count,
            created_at,
        }))
    }

    pub(crate) fn delete_wiki_proposal_sync(&self, tag_id: &str) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute("DELETE FROM wiki_proposals WHERE tag_id = ?1", [tag_id])
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to delete wiki proposal: {}", e)))?;
        Ok(())
    }

    pub(crate) fn advance_wiki_baseline_sync(
        &self,
        tag_id: &str,
        max_current_count: Option<i32>,
    ) -> StorageResult<bool> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        // Use the same descendant CTE as get_article_status so the counts agree.
        let current_count: i32 = conn
            .query_row(
                "WITH RECURSIVE descendant_tags(id) AS (
                    SELECT ?1
                    UNION ALL
                    SELECT t.id FROM tags t
                    INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                )
                SELECT COUNT(DISTINCT atom_id) FROM atom_tags
                WHERE tag_id IN (SELECT id FROM descendant_tags)",
                [tag_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                AtomicCoreError::Wiki(format!("Failed to count atoms for baseline advance: {}", e))
            })?;

        if matches!(max_current_count, Some(max) if current_count > max) {
            return Ok(false);
        }

        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE wiki_articles SET atom_count = ?1, updated_at = ?2 WHERE tag_id = ?3",
            (current_count, &now, tag_id),
        )
        .map_err(|e| AtomicCoreError::Wiki(format!("Failed to advance wiki baseline: {}", e)))?;
        Ok(true)
    }
}

#[async_trait]
impl WikiStore for SqliteStorage {
    async fn get_wiki(&self, tag_id: &str) -> StorageResult<Option<WikiArticleWithCitations>> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || storage.get_wiki_sync(&tag_id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_wiki_status(&self, tag_id: &str) -> StorageResult<WikiArticleStatus> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || storage.get_wiki_status_sync(&tag_id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn save_wiki(
        &self,
        tag_id: &str,
        content: &str,
        citations: &[WikiCitation],
        atom_count: i32,
    ) -> StorageResult<WikiArticleWithCitations> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        let content = content.to_string();
        let citations = citations.to_vec();
        tokio::task::spawn_blocking(move || {
            storage.save_wiki_sync(&tag_id, &content, &citations, atom_count)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn save_wiki_with_links(
        &self,
        article: &WikiArticle,
        citations: &[WikiCitation],
        links: &[WikiLink],
    ) -> StorageResult<()> {
        let storage = self.clone();
        let article = article.clone();
        let citations = citations.to_vec();
        let links = links.to_vec();
        tokio::task::spawn_blocking(move || {
            storage.save_wiki_with_links_sync(&article, &citations, &links)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn delete_wiki(&self, tag_id: &str) -> StorageResult<()> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || storage.delete_wiki_sync(&tag_id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_wiki_links(&self, tag_id: &str) -> StorageResult<Vec<WikiLink>> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || storage.get_wiki_links_sync(&tag_id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn list_wiki_versions(&self, tag_id: &str) -> StorageResult<Vec<WikiVersionSummary>> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || storage.list_wiki_versions_sync(&tag_id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_wiki_version(
        &self,
        version_id: &str,
    ) -> StorageResult<Option<WikiArticleVersion>> {
        let storage = self.clone();
        let version_id = version_id.to_string();
        tokio::task::spawn_blocking(move || storage.get_wiki_version_sync(&version_id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_all_wiki_articles(&self) -> StorageResult<Vec<WikiArticleSummary>> {
        let storage = self.clone();
        tokio::task::spawn_blocking(move || storage.get_all_wiki_articles_sync())
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_wiki_source_chunks(
        &self,
        tag_id: &str,
        max_source_tokens: usize,
    ) -> StorageResult<(Vec<ChunkWithContext>, i32)> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || {
            storage.get_wiki_source_chunks_sync(&tag_id, max_source_tokens)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_wiki_update_chunks(
        &self,
        tag_id: &str,
        last_update: &str,
        max_source_tokens: usize,
    ) -> StorageResult<Option<(Vec<ChunkWithContext>, i32)>> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        let last_update = last_update.to_string();
        tokio::task::spawn_blocking(move || {
            storage.get_wiki_update_chunks_sync(&tag_id, &last_update, max_source_tokens)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn save_wiki_proposal(&self, proposal: &WikiProposal) -> StorageResult<()> {
        let storage = self.clone();
        let proposal = proposal.clone();
        tokio::task::spawn_blocking(move || storage.save_wiki_proposal_sync(&proposal))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn get_wiki_proposal(&self, tag_id: &str) -> StorageResult<Option<WikiProposal>> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || storage.get_wiki_proposal_sync(&tag_id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn delete_wiki_proposal(&self, tag_id: &str) -> StorageResult<()> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || storage.delete_wiki_proposal_sync(&tag_id))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn advance_wiki_baseline(
        &self,
        tag_id: &str,
        max_current_count: Option<i32>,
    ) -> StorageResult<bool> {
        let storage = self.clone();
        let tag_id = tag_id.to_string();
        tokio::task::spawn_blocking(move || {
            storage.advance_wiki_baseline_sync(&tag_id, max_current_count)
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }
}
