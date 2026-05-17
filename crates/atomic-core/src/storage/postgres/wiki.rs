use super::PostgresStorage;
use crate::chunking::count_tokens;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;

#[async_trait]
impl WikiStore for PostgresStorage {
    async fn get_wiki(&self, tag_id: &str) -> StorageResult<Option<WikiArticleWithCitations>> {
        // Get article
        let article_row = sqlx::query_as::<_, (String, String, String, String, String, i32)>(
            "SELECT id, tag_id, content, created_at, updated_at, atom_count
             FROM wiki_articles WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let article = match article_row {
            Some((id, tag_id, content, created_at, updated_at, atom_count)) => WikiArticle {
                id,
                tag_id,
                content,
                created_at,
                updated_at,
                atom_count,
            },
            None => return Ok(None),
        };

        // Get citations, joining atoms for source_url so clients can render
        // citations differently based on the cited atom's origin (e.g. Obsidian
        // plugin rewriting them as wikilinks).
        let citation_rows =
            sqlx::query_as::<_, (String, i32, String, Option<i32>, String, Option<String>)>(
                "SELECT c.id, c.citation_index, c.atom_id, c.chunk_index, c.excerpt, a.source_url
             FROM wiki_citations c
             LEFT JOIN atoms a ON a.id = c.atom_id AND a.db_id = c.db_id
             WHERE c.wiki_article_id = $1 AND c.db_id = $2
             ORDER BY c.citation_index",
            )
            .bind(&article.id)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let citations: Vec<WikiCitation> = citation_rows
            .into_iter()
            .map(
                |(id, citation_index, atom_id, chunk_index, excerpt, source_url)| WikiCitation {
                    id,
                    citation_index,
                    atom_id,
                    chunk_index,
                    excerpt,
                    source_url,
                },
            )
            .collect();

        Ok(Some(WikiArticleWithCitations { article, citations }))
    }

    async fn get_wiki_status(&self, tag_id: &str) -> StorageResult<WikiArticleStatus> {
        // Count distinct atoms across this tag and all descendants using recursive CTE
        let current_atom_count: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
            "WITH RECURSIVE descendant_tags(id) AS (
                SELECT $1::text
                UNION ALL
                SELECT t.id FROM tags t
                INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                WHERE t.db_id = $2
            )
            SELECT COUNT(DISTINCT atom_id) FROM atom_tags
            WHERE tag_id IN (SELECT id FROM descendant_tags) AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let current_atom_count = current_atom_count.unwrap_or(0);

        // Get article info if exists
        let article_info = sqlx::query_as::<_, (i32, String)>(
            "SELECT atom_count, updated_at FROM wiki_articles WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        match article_info {
            Some((article_atom_count, updated_at)) => {
                let new_atoms = (current_atom_count as i32 - article_atom_count).max(0);
                Ok(WikiArticleStatus {
                    has_article: true,
                    article_atom_count,
                    current_atom_count: current_atom_count as i32,
                    new_atoms_available: new_atoms,
                    updated_at: Some(updated_at),
                })
            }
            None => Ok(WikiArticleStatus {
                has_article: false,
                article_atom_count: 0,
                current_atom_count: current_atom_count as i32,
                new_atoms_available: 0,
                updated_at: None,
            }),
        }
    }

    async fn save_wiki(
        &self,
        tag_id: &str,
        content: &str,
        citations: &[WikiCitation],
        atom_count: i32,
    ) -> StorageResult<WikiArticleWithCitations> {
        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();

        // Archive existing article before replacing
        self.archive_existing_article(tag_id).await?;

        // Delete existing article for this tag (cascade deletes citations + links)
        sqlx::query("DELETE FROM wiki_articles WHERE tag_id = $1 AND db_id = $2")
            .bind(tag_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Insert new article
        sqlx::query(
            "INSERT INTO wiki_articles (id, tag_id, content, created_at, updated_at, atom_count, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(tag_id)
        .bind(content)
        .bind(&now)
        .bind(&now)
        .bind(atom_count)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Insert citations
        for citation in citations {
            sqlx::query(
                "INSERT INTO wiki_citations (id, wiki_article_id, citation_index, atom_id, chunk_index, excerpt, db_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(&citation.id)
            .bind(&id)
            .bind(citation.citation_index)
            .bind(&citation.atom_id)
            .bind(citation.chunk_index)
            .bind(&citation.excerpt)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        let article = WikiArticle {
            id,
            tag_id: tag_id.to_string(),
            content: content.to_string(),
            created_at: now.clone(),
            updated_at: now,
            atom_count,
        };

        Ok(WikiArticleWithCitations {
            article,
            citations: citations.to_vec(),
        })
    }

    async fn save_wiki_with_links(
        &self,
        article: &WikiArticle,
        citations: &[WikiCitation],
        links: &[WikiLink],
    ) -> StorageResult<()> {
        // Archive existing article before replacing
        self.archive_existing_article(&article.tag_id).await?;

        // Delete existing article for this tag (cascade deletes citations + links)
        sqlx::query("DELETE FROM wiki_articles WHERE tag_id = $1 AND db_id = $2")
            .bind(&article.tag_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Insert new article
        sqlx::query(
            "INSERT INTO wiki_articles (id, tag_id, content, created_at, updated_at, atom_count, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&article.id)
        .bind(&article.tag_id)
        .bind(&article.content)
        .bind(&article.created_at)
        .bind(&article.updated_at)
        .bind(article.atom_count)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Insert citations
        for citation in citations {
            sqlx::query(
                "INSERT INTO wiki_citations (id, wiki_article_id, citation_index, atom_id, chunk_index, excerpt, db_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(&citation.id)
            .bind(&article.id)
            .bind(citation.citation_index)
            .bind(&citation.atom_id)
            .bind(citation.chunk_index)
            .bind(&citation.excerpt)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        // Insert wiki links (skip links with no resolved target_tag_id)
        for link in links {
            let target_tag_id = match &link.target_tag_id {
                Some(id) => id,
                None => continue, // Can't insert NULL into NOT NULL column
            };
            sqlx::query(
                "INSERT INTO wiki_links (id, source_article_id, link_text, target_tag_id, db_id)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&link.id)
            .bind(&article.id)
            .bind(&link.target_tag_name)
            .bind(target_tag_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }

        Ok(())
    }

    async fn delete_wiki(&self, tag_id: &str) -> StorageResult<()> {
        sqlx::query("DELETE FROM wiki_articles WHERE tag_id = $1 AND db_id = $2")
            .bind(tag_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    async fn get_wiki_links(&self, tag_id: &str) -> StorageResult<Vec<WikiLink>> {
        // The Postgres schema stores target_tag_id and link_text (not target_tag_name).
        // We adapt to produce WikiLink with target_tag_name resolved from tags table.
        let rows = sqlx::query_as::<_, (String, String, String, Option<String>)>(
            "SELECT wl.id, wl.source_article_id, COALESCE(t.name, wl.link_text),
                    wl.target_tag_id
             FROM wiki_links wl
             LEFT JOIN tags t ON t.id = wl.target_tag_id AND t.db_id = $2
             WHERE wl.source_article_id = (SELECT id FROM wiki_articles WHERE tag_id = $1 AND db_id = $2)
             AND wl.db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut links = Vec::new();
        for (id, source_article_id, target_tag_name, target_tag_id) in rows {
            // Check if target tag has an article
            let has_article = if let Some(ref ttid) = target_tag_id {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM wiki_articles WHERE tag_id = $1 AND db_id = $2)",
                )
                .bind(ttid)
                .bind(&self.db_id)
                .fetch_one(&self.pool)
                .await
                .unwrap_or(false)
            } else {
                false
            };

            links.push(WikiLink {
                id,
                source_article_id,
                target_tag_name,
                target_tag_id,
                has_article,
            });
        }

        Ok(links)
    }

    async fn list_wiki_versions(&self, tag_id: &str) -> StorageResult<Vec<WikiVersionSummary>> {
        let rows = sqlx::query_as::<_, (String, i32, i32, String)>(
            "SELECT id, version_number, atom_count, created_at
             FROM wiki_article_versions
             WHERE tag_id = $1 AND db_id = $2
             ORDER BY version_number DESC",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(
                |(id, version_number, atom_count, created_at)| WikiVersionSummary {
                    id,
                    version_number,
                    atom_count,
                    created_at,
                },
            )
            .collect())
    }

    async fn get_wiki_version(
        &self,
        version_id: &str,
    ) -> StorageResult<Option<WikiArticleVersion>> {
        let row = sqlx::query_as::<_, (String, String, String, i32, i32, String)>(
            "SELECT id, tag_id, content, atom_count, version_number, created_at
             FROM wiki_article_versions WHERE id = $1 AND db_id = $2",
        )
        .bind(version_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        match row {
            Some((id, tag_id, content, atom_count, version_number, created_at)) => {
                // Postgres schema doesn't store citations_json in versions;
                // return empty citations for historical versions.
                Ok(Some(WikiArticleVersion {
                    id,
                    tag_id,
                    content,
                    citations: Vec::new(),
                    atom_count,
                    version_number,
                    created_at,
                }))
            }
            None => Ok(None),
        }
    }

    async fn get_all_wiki_articles(&self) -> StorageResult<Vec<WikiArticleSummary>> {
        let rows = sqlx::query_as::<_, (String, String, String, String, i32, i64)>(
            "SELECT w.id, w.tag_id, t.name, w.updated_at, w.atom_count,
                    (SELECT COUNT(*) FROM wiki_links wl WHERE wl.target_tag_id = w.tag_id AND wl.db_id = $1)
             FROM wiki_articles w
             JOIN tags t ON w.tag_id = t.id AND t.db_id = $1
             WHERE w.db_id = $1
             ORDER BY (SELECT COUNT(*) FROM wiki_links wl WHERE wl.target_tag_id = w.tag_id AND wl.db_id = $1) DESC,
                      w.atom_count DESC, w.updated_at DESC",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(
                |(id, tag_id, tag_name, updated_at, atom_count, inbound_links)| {
                    WikiArticleSummary {
                        id,
                        tag_id,
                        tag_name,
                        updated_at,
                        atom_count,
                        inbound_links: inbound_links as i32,
                    }
                },
            )
            .collect())
    }

    async fn get_wiki_source_chunks(
        &self,
        tag_id: &str,
        max_source_tokens: usize,
    ) -> StorageResult<(Vec<ChunkWithContext>, i32)> {
        use crate::storage::TagStore;

        // Get all descendant tag IDs
        let all_tag_ids = self.get_tag_hierarchy(tag_id).await?;
        if all_tag_ids.is_empty() {
            return Err(AtomicCoreError::Wiki(
                "No content found for this tag".to_string(),
            ));
        }

        // Wiki context-assembly is captured-only — apply the kind filter at
        // scope-resolution so the centroid / unranked chunk selectors below
        // can never see report-kind atoms. Filtering only the final count
        // (the previous shape) silently fed report chunks to the LLM.
        let captured_kinds = crate::models::KindFilter::only(crate::models::AtomKind::Captured);
        let scoped_atom_ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT at.atom_id
             FROM atom_tags at
             INNER JOIN atoms a ON a.id = at.atom_id AND a.db_id = at.db_id
             WHERE at.tag_id = ANY($1) AND at.db_id = $2 AND a.kind = ANY($3)",
        )
        .bind(&all_tag_ids)
        .bind(&self.db_id)
        .bind(captured_kinds.kind_strings())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

        if scoped_atom_ids.is_empty() {
            return Err(AtomicCoreError::Wiki(
                "No content found for this tag".to_string(),
            ));
        }

        // Try centroid-ranked retrieval using pgvector
        let centroid: Option<Vec<f32>> = sqlx::query_scalar(
            "SELECT embedding::real[] FROM tag_embeddings WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

        let chunks = if let Some(ref centroid_vec) = centroid {
            // Ranked path: query chunks by cosine similarity to centroid, scoped to atoms
            let rows: Vec<(String, i32, String, f64)> = sqlx::query_as(
                "SELECT ac.atom_id, ac.chunk_index, ac.content,
                        1 - (e.embedding <=> $1::vector) as similarity
                 FROM atom_chunk_embeddings e
                 JOIN atom_chunks ac ON e.chunk_id = ac.id
                 WHERE ac.atom_id = ANY($2) AND ac.db_id = $3
                 ORDER BY e.embedding <=> $1::vector
                 LIMIT 3000",
            )
            .bind(centroid_vec.as_slice())
            .bind(&scoped_atom_ids)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

            let mut chunks = Vec::new();
            let mut total_tokens = 0;
            for (atom_id, chunk_index, content, similarity) in rows {
                let tokens = count_tokens(&content);
                if total_tokens + tokens > max_source_tokens && !chunks.is_empty() {
                    break;
                }
                total_tokens += tokens;
                chunks.push(ChunkWithContext {
                    atom_id,
                    chunk_index,
                    content,
                    similarity_score: similarity as f32,
                });
            }
            chunks
        } else {
            // Fallback: fetch by insertion order — scoped through
            // `scoped_atom_ids` (kind-filtered above) instead of re-joining
            // through atom_tags, so both paths see the same atom set.
            tracing::debug!(
                tag_id,
                "[wiki/postgres] No centroid for tag, falling back to unranked"
            );
            let rows: Vec<(String, i32, String)> = sqlx::query_as(
                "SELECT atom_id, chunk_index, content FROM atom_chunks
                 WHERE atom_id = ANY($1) AND db_id = $2
                 ORDER BY atom_id, chunk_index",
            )
            .bind(&scoped_atom_ids)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

            let mut chunks = Vec::new();
            let mut total_tokens = 0;
            for (atom_id, chunk_index, content) in rows {
                let tokens = count_tokens(&content);
                if total_tokens + tokens > max_source_tokens && !chunks.is_empty() {
                    break;
                }
                total_tokens += tokens;
                chunks.push(ChunkWithContext {
                    atom_id,
                    chunk_index,
                    content,
                    similarity_score: 1.0,
                });
            }
            chunks
        };

        if chunks.is_empty() {
            return Err(AtomicCoreError::Wiki(
                "No content found for this tag".to_string(),
            ));
        }

        // Count matches the scope used above — both kind-filtered.
        let atom_count = self
            .count_atoms_with_tags(&all_tag_ids, &captured_kinds)
            .await?;
        Ok((chunks, atom_count))
    }

    async fn get_wiki_update_chunks(
        &self,
        tag_id: &str,
        last_update: &str,
        max_source_tokens: usize,
    ) -> StorageResult<Option<(Vec<ChunkWithContext>, i32)>> {
        use crate::storage::TagStore;

        // Captured-only — see get_wiki_source_chunks for why the kind filter
        // must apply at scope-resolution.
        let captured_kinds = crate::models::KindFilter::only(crate::models::AtomKind::Captured);
        let new_atom_ids: Vec<String> = sqlx::query_scalar(
            "WITH RECURSIVE descendant_tags(id) AS (
                 SELECT $1
                 UNION ALL
                 SELECT t.id FROM tags t
                 INNER JOIN descendant_tags dt ON t.parent_id = dt.id
             )
             SELECT DISTINCT a.id FROM atoms a
             INNER JOIN atom_tags at ON a.id = at.atom_id
             WHERE at.tag_id IN (SELECT id FROM descendant_tags)
               AND a.created_at > $2 AND a.db_id = $3 AND at.db_id = $3
               AND a.kind = ANY($4)",
        )
        .bind(tag_id)
        .bind(last_update)
        .bind(&self.db_id)
        .bind(captured_kinds.kind_strings())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

        if new_atom_ids.is_empty() {
            return Ok(None);
        }

        // Try centroid-ranked selection scoped to new atoms only
        let centroid: Option<Vec<f32>> = sqlx::query_scalar(
            "SELECT embedding::real[] FROM tag_embeddings WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

        let mut new_chunks = if let Some(ref centroid_vec) = centroid {
            let rows: Vec<(String, i32, String, f64)> = sqlx::query_as(
                "SELECT ac.atom_id, ac.chunk_index, ac.content,
                        1 - (e.embedding <=> $1::vector) as similarity
                 FROM atom_chunk_embeddings e
                 JOIN atom_chunks ac ON e.chunk_id = ac.id
                 WHERE ac.atom_id = ANY($2) AND ac.db_id = $3
                 ORDER BY e.embedding <=> $1::vector
                 LIMIT 3000",
            )
            .bind(centroid_vec.as_slice())
            .bind(&new_atom_ids)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

            let mut chunks = Vec::new();
            let mut total_tokens = 0;
            for (atom_id, chunk_index, content, similarity) in rows {
                let tokens = count_tokens(&content);
                if total_tokens + tokens > max_source_tokens && !chunks.is_empty() {
                    break;
                }
                total_tokens += tokens;
                chunks.push(ChunkWithContext {
                    atom_id,
                    chunk_index,
                    content,
                    similarity_score: similarity as f32,
                });
            }
            chunks
        } else {
            let rows: Vec<(String, i32, String)> = sqlx::query_as(
                "SELECT atom_id, chunk_index, content FROM atom_chunks
                 WHERE atom_id = ANY($1) AND db_id = $2 ORDER BY atom_id, chunk_index",
            )
            .bind(&new_atom_ids)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

            let mut chunks = Vec::new();
            let mut total_tokens = 0;
            for (atom_id, chunk_index, content) in rows {
                let tokens = count_tokens(&content);
                if total_tokens + tokens > max_source_tokens && !chunks.is_empty() {
                    break;
                }
                total_tokens += tokens;
                chunks.push(ChunkWithContext {
                    atom_id,
                    chunk_index,
                    content,
                    similarity_score: 1.0,
                });
            }
            chunks
        };

        if new_chunks.is_empty() && centroid.is_some() {
            let rows: Vec<(String, i32, String)> = sqlx::query_as(
                "SELECT atom_id, chunk_index, content FROM atom_chunks
                 WHERE atom_id = ANY($1) AND db_id = $2 ORDER BY atom_id, chunk_index",
            )
            .bind(&new_atom_ids)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;

            let mut chunks = Vec::new();
            let mut total_tokens = 0;
            for (atom_id, chunk_index, content) in rows {
                let tokens = count_tokens(&content);
                if total_tokens + tokens > max_source_tokens && !chunks.is_empty() {
                    break;
                }
                total_tokens += tokens;
                chunks.push(ChunkWithContext {
                    atom_id,
                    chunk_index,
                    content,
                    similarity_score: 1.0,
                });
            }
            new_chunks = chunks;
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
        let all_tag_ids = self.get_tag_hierarchy(tag_id).await?;
        let atom_count = self
            .count_atoms_with_tags(&all_tag_ids, &captured_kinds)
            .await?;

        Ok(Some((new_chunks, atom_count)))
    }

    async fn save_wiki_proposal(&self, proposal: &WikiProposal) -> StorageResult<()> {
        let citations_json = serde_json::to_string(&proposal.citations)
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to serialize citations: {}", e)))?;
        let ops_json = serde_json::to_string(&proposal.ops)
            .map_err(|e| AtomicCoreError::Wiki(format!("Failed to serialize ops: {}", e)))?;

        sqlx::query(
            "INSERT INTO wiki_proposals
                (id, db_id, tag_id, base_article_id, base_updated_at, content,
                 citations_json, ops_json, new_atom_count, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (db_id, tag_id) DO UPDATE SET
                id = excluded.id,
                base_article_id = excluded.base_article_id,
                base_updated_at = excluded.base_updated_at,
                content = excluded.content,
                citations_json = excluded.citations_json,
                ops_json = excluded.ops_json,
                new_atom_count = excluded.new_atom_count,
                created_at = excluded.created_at",
        )
        .bind(&proposal.id)
        .bind(&self.db_id)
        .bind(&proposal.tag_id)
        .bind(&proposal.base_article_id)
        .bind(&proposal.base_updated_at)
        .bind(&proposal.content)
        .bind(&citations_json)
        .bind(&ops_json)
        .bind(proposal.new_atom_count)
        .bind(&proposal.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn get_wiki_proposal(&self, tag_id: &str) -> StorageResult<Option<WikiProposal>> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                i32,
                String,
            ),
        >(
            "SELECT id, tag_id, base_article_id, base_updated_at, content,
                    citations_json, ops_json, new_atom_count, created_at
             FROM wiki_proposals
             WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

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

    async fn delete_wiki_proposal(&self, tag_id: &str) -> StorageResult<()> {
        sqlx::query("DELETE FROM wiki_proposals WHERE tag_id = $1 AND db_id = $2")
            .bind(tag_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    async fn advance_wiki_baseline(
        &self,
        tag_id: &str,
        max_current_count: Option<i32>,
    ) -> StorageResult<bool> {
        let now = chrono::Utc::now().to_rfc3339();
        let advanced = sqlx::query_scalar::<_, bool>(
            "WITH RECURSIVE descendant_tags(id) AS (
                SELECT $1::text
                UNION ALL
                SELECT t.id FROM tags t
                INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                WHERE t.db_id = $2
            ),
            current_total(atom_count) AS (
                SELECT COUNT(DISTINCT atom_id)::int FROM atom_tags
                WHERE tag_id IN (SELECT id FROM descendant_tags) AND db_id = $2
            ),
            updated AS (
                UPDATE wiki_articles
                SET atom_count = current_total.atom_count, updated_at = $3
                FROM current_total
                WHERE wiki_articles.tag_id = $1
                  AND wiki_articles.db_id = $2
                  AND ($4::int IS NULL OR current_total.atom_count <= $4)
                RETURNING 1
            )
            SELECT EXISTS(SELECT 1 FROM updated)",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .bind(&now)
        .bind(max_current_count)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::Wiki(e.to_string()))?;
        Ok(advanced)
    }
}

// Private helper methods
impl PostgresStorage {
    /// Archive the current wiki article (if any) into wiki_article_versions.
    async fn archive_existing_article(&self, tag_id: &str) -> StorageResult<()> {
        // Load existing article
        let existing = sqlx::query_as::<_, (String, String, i32, String)>(
            "SELECT id, content, atom_count, created_at FROM wiki_articles WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let (_article_id, content, atom_count, created_at) = match existing {
            Some(e) => e,
            None => return Ok(()),
        };

        // Compute next version number
        let next_version: Option<i32> = sqlx::query_scalar::<_, Option<i32>>(
            "SELECT COALESCE(MAX(version_number), 0) + 1 FROM wiki_article_versions WHERE tag_id = $1 AND db_id = $2",
        )
        .bind(tag_id)
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let next_version = next_version.unwrap_or(1);

        // Insert version (Postgres schema doesn't have citations_json column)
        sqlx::query(
            "INSERT INTO wiki_article_versions (id, tag_id, content, atom_count, version_number, created_at, db_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(tag_id)
        .bind(&content)
        .bind(atom_count)
        .bind(next_version as i32)
        .bind(&created_at)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }
}
