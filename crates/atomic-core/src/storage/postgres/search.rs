use std::collections::HashMap;

use super::PostgresStorage;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;
use pgvector::Vector;

impl PostgresStorage {
    pub async fn global_keyword_search(
        &self,
        query: &str,
        section_limit: i32,
    ) -> StorageResult<GlobalSearchResponse> {
        let query_trimmed = query.trim();
        if query_trimmed.is_empty() {
            return Ok(GlobalSearchResponse {
                atoms: Vec::new(),
                wiki: Vec::new(),
                chats: Vec::new(),
                tags: Vec::new(),
            });
        }

        // Global search is the in-app palette — show every kind, mirroring
        // the SQLite path and the UI list/canvas surfaces.
        let atoms = self
            .keyword_search(
                query_trimmed,
                section_limit,
                None,
                None,
                &crate::models::KindFilter::All,
            )
            .await?;
        let wiki =
            pg_keyword_search_wiki(&self.pool, query_trimmed, section_limit, &self.db_id).await?;
        let chats =
            pg_keyword_search_chats(&self.pool, query_trimmed, section_limit, &self.db_id).await?;
        let tags =
            pg_keyword_search_tags(&self.pool, query_trimmed, section_limit, &self.db_id).await?;

        Ok(GlobalSearchResponse {
            atoms,
            wiki,
            chats,
            tags,
        })
    }
}

#[async_trait]
impl SearchStore for PostgresStorage {
    async fn vector_search(
        &self,
        query_embedding: &[f32],
        limit: i32,
        threshold: f32,
        tag_id: Option<&str>,
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<SemanticSearchResult>> {
        let embedding_vec = Vector::from(query_embedding.to_vec());
        let fetch_limit = limit * 10;

        let rows: Vec<(String, String, String, i32, f64)> = if let Some(cutoff) = created_after {
            sqlx::query_as(
                "SELECT ac.id, ac.atom_id, ac.content, ac.chunk_index,
                        (ac.embedding <=> $1::vector) AS distance
                 FROM atom_chunks ac
                 INNER JOIN atoms a ON a.id = ac.atom_id AND a.db_id = ac.db_id
                 WHERE ac.embedding IS NOT NULL AND ac.db_id = $3 AND a.created_at >= $4
                 ORDER BY ac.embedding <=> $1::vector
                 LIMIT $2",
            )
            .bind(&embedding_vec)
            .bind(fetch_limit)
            .bind(&self.db_id)
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as(
                "SELECT ac.id, ac.atom_id, ac.content, ac.chunk_index,
                        (ac.embedding <=> $1::vector) AS distance
                 FROM atom_chunks ac
                 WHERE ac.embedding IS NOT NULL AND ac.db_id = $3
                 ORDER BY ac.embedding <=> $1::vector
                 LIMIT $2",
            )
            .bind(&embedding_vec)
            .bind(fetch_limit)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| AtomicCoreError::Search(format!("Vector search failed: {}", e)))?;

        // Filter by threshold: pgvector cosine distance is 0-2, similarity = 1.0 - distance
        let filtered: Vec<(String, String, String, i32, f32)> = rows
            .into_iter()
            .map(|(chunk_id, atom_id, content, chunk_index, distance)| {
                let similarity = 1.0 - distance as f32;
                (chunk_id, atom_id, content, chunk_index, similarity)
            })
            .filter(|(_, _, _, _, similarity)| *similarity >= threshold)
            .collect();

        // Scope filtering by tag if specified
        let scope_atom_ids: std::collections::HashSet<String> = if let Some(tid) = tag_id {
            let candidate_atom_ids: Vec<&str> = filtered
                .iter()
                .map(|(_, aid, _, _, _)| aid.as_str())
                .collect();
            pg_batch_atoms_with_scope_tags(
                &self.pool,
                &candidate_atom_ids,
                &[tid.to_string()],
                &self.db_id,
            )
            .await?
        } else {
            std::collections::HashSet::new()
        };
        // Kind filtering. `None` = `KindFilter::All` (no filter applied).
        let kind_allowed: Option<std::collections::HashSet<String>> =
            if matches!(kinds, crate::models::KindFilter::All) {
                None
            } else {
                let candidate_atom_ids: Vec<&str> = filtered
                    .iter()
                    .map(|(_, aid, _, _, _)| aid.as_str())
                    .collect();
                Some(
                    pg_batch_atoms_matching_kind(
                        &self.pool,
                        &candidate_atom_ids,
                        kinds,
                        &self.db_id,
                    )
                    .await?,
                )
            };

        // Deduplicate by atom_id, keeping best score
        let mut atom_best: HashMap<String, (f32, String, i32)> = HashMap::new();
        for (_chunk_id, atom_id, content, chunk_index, similarity) in &filtered {
            if tag_id.is_some() && !scope_atom_ids.contains(atom_id) {
                continue;
            }
            if let Some(ref allowed) = kind_allowed {
                if !allowed.contains(atom_id) {
                    continue;
                }
            }
            let entry = atom_best.entry(atom_id.clone());
            match entry {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if *similarity > e.get().0 {
                        e.insert((*similarity, content.clone(), *chunk_index));
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((*similarity, content.clone(), *chunk_index));
                }
            }
        }

        // Sort and limit
        let mut deduped: Vec<(String, f32, String, i32)> = atom_best
            .into_iter()
            .map(|(atom_id, (sim, content, idx))| (atom_id, sim, content, idx))
            .collect();
        deduped.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        deduped.truncate(limit as usize);

        // Batch fetch atom data
        let atom_ids: Vec<String> = deduped.iter().map(|(id, _, _, _)| id.clone()).collect();
        let atom_map = pg_batch_fetch_atoms(&self.pool, &atom_ids, &self.db_id).await?;
        let tag_map = pg_batch_fetch_tags(&self.pool, &atom_ids, &self.db_id).await?;

        let mut results = Vec::with_capacity(deduped.len());
        for (atom_id, similarity, content, chunk_index) in deduped {
            if let Some(atom) = atom_map.get(&atom_id) {
                let tags = tag_map.get(&atom_id).cloned().unwrap_or_default();
                results.push(SemanticSearchResult {
                    atom: AtomWithTags {
                        atom: atom.clone(),
                        tags,
                    },
                    similarity_score: similarity,
                    matching_chunk_content: content,
                    matching_chunk_index: chunk_index,
                    match_snippet: None,
                    match_offsets: None,
                    match_count: None,
                });
            }
        }

        Ok(results)
    }

    async fn keyword_search(
        &self,
        query: &str,
        limit: i32,
        tag_id: Option<&str>,
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<SemanticSearchResult>> {
        let query_trimmed = query.trim();
        if query_trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let fetch_limit = limit * 5;

        let rows: Vec<(String, String, String, i32, f32)> = if let Some(cutoff) = created_after {
            sqlx::query_as(
                "SELECT ac.id, ac.atom_id, ac.content, ac.chunk_index,
                        ts_rank(ac.content_tsv, plainto_tsquery('english', $1)) AS rank
                 FROM atom_chunks ac
                 INNER JOIN atoms a ON a.id = ac.atom_id AND a.db_id = ac.db_id
                 WHERE ac.content_tsv @@ plainto_tsquery('english', $1)
                   AND ac.db_id = $3
                   AND a.created_at >= $4
                 ORDER BY ts_rank(ac.content_tsv, plainto_tsquery('english', $1)) DESC
                 LIMIT $2",
            )
            .bind(query_trimmed)
            .bind(fetch_limit)
            .bind(&self.db_id)
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as(
                "SELECT ac.id, ac.atom_id, ac.content, ac.chunk_index,
                        ts_rank(ac.content_tsv, plainto_tsquery('english', $1)) AS rank
                 FROM atom_chunks ac
                 WHERE ac.content_tsv @@ plainto_tsquery('english', $1) AND ac.db_id = $3
                 ORDER BY ts_rank(ac.content_tsv, plainto_tsquery('english', $1)) DESC
                 LIMIT $2",
            )
            .bind(query_trimmed)
            .bind(fetch_limit)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| AtomicCoreError::Search(format!("Keyword search failed: {}", e)))?;

        // Apply tag scope filter if specified
        let filtered = if let Some(tid) = tag_id {
            let candidate_atom_ids: Vec<&str> =
                rows.iter().map(|(_, aid, _, _, _)| aid.as_str()).collect();
            let matching = pg_batch_atoms_with_scope_tags(
                &self.pool,
                &candidate_atom_ids,
                &[tid.to_string()],
                &self.db_id,
            )
            .await?;
            rows.into_iter()
                .filter(|(_, aid, _, _, _)| matching.contains(aid.as_str()))
                .collect()
        } else {
            rows
        };

        // Apply kind filter post-pass (FTS index is kind-agnostic).
        let filtered: Vec<(String, String, String, i32, f32)> =
            if matches!(kinds, crate::models::KindFilter::All) {
                filtered
            } else {
                let candidate_atom_ids: Vec<&str> = filtered
                    .iter()
                    .map(|(_, aid, _, _, _)| aid.as_str())
                    .collect();
                let allowed = pg_batch_atoms_matching_kind(
                    &self.pool,
                    &candidate_atom_ids,
                    kinds,
                    &self.db_id,
                )
                .await?;
                filtered
                    .into_iter()
                    .filter(|(_, aid, _, _, _)| allowed.contains(aid.as_str()))
                    .collect()
            };

        // Deduplicate by atom_id, keeping best score
        let mut atom_best: HashMap<String, (f32, String, i32)> = HashMap::new();
        for (_chunk_id, atom_id, content, chunk_index, rank) in &filtered {
            // Normalize ts_rank to 0-1 range; ts_rank typically returns values 0-1 already
            let score = rank.clamp(0.0, 1.0);
            let entry = atom_best.entry(atom_id.clone());
            match entry {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if score > e.get().0 {
                        e.insert((score, content.clone(), *chunk_index));
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((score, content.clone(), *chunk_index));
                }
            }
        }

        // Sort and limit
        let mut deduped: Vec<(String, f32, String, i32)> = atom_best
            .into_iter()
            .map(|(atom_id, (score, content, idx))| (atom_id, score, content, idx))
            .collect();
        deduped.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        deduped.truncate(limit as usize);

        // Batch fetch atom data
        let atom_ids: Vec<String> = deduped.iter().map(|(id, _, _, _)| id.clone()).collect();
        let atom_map = pg_batch_fetch_atoms(&self.pool, &atom_ids, &self.db_id).await?;
        let tag_map = pg_batch_fetch_tags(&self.pool, &atom_ids, &self.db_id).await?;

        let mut results = Vec::with_capacity(deduped.len());
        for (atom_id, score, content, chunk_index) in deduped {
            if let Some(atom) = atom_map.get(&atom_id) {
                let tags = tag_map.get(&atom_id).cloned().unwrap_or_default();
                results.push(SemanticSearchResult {
                    atom: AtomWithTags {
                        atom: atom.clone(),
                        tags,
                    },
                    similarity_score: score,
                    matching_chunk_content: content,
                    matching_chunk_index: chunk_index,
                    match_snippet: None,
                    match_offsets: None,
                    match_count: None,
                });
            }
        }

        Ok(results)
    }

    async fn keyword_search_chunks(
        &self,
        query: &str,
        limit: i32,
        scope_tag_ids: &[String],
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<ChunkSearchResult>> {
        let query_trimmed = query.trim();
        if query_trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let fetch_limit = limit * 3;
        let rows: Vec<(String, String, String, i32, f32)> = if let Some(cutoff) = created_after {
            sqlx::query_as(
                "SELECT ac.id, ac.atom_id, ac.content, ac.chunk_index,
                        ts_rank(ac.content_tsv, plainto_tsquery('english', $1)) AS rank
                 FROM atom_chunks ac
                 INNER JOIN atoms a ON a.id = ac.atom_id AND a.db_id = ac.db_id
                 WHERE ac.content_tsv @@ plainto_tsquery('english', $1)
                   AND ac.db_id = $3
                   AND a.created_at >= $4
                 ORDER BY ts_rank(ac.content_tsv, plainto_tsquery('english', $1)) DESC
                 LIMIT $2",
            )
            .bind(query_trimmed)
            .bind(fetch_limit)
            .bind(&self.db_id)
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as(
                "SELECT ac.id, ac.atom_id, ac.content, ac.chunk_index,
                        ts_rank(ac.content_tsv, plainto_tsquery('english', $1)) AS rank
                 FROM atom_chunks ac
                 WHERE ac.content_tsv @@ plainto_tsquery('english', $1) AND ac.db_id = $3
                 ORDER BY ts_rank(ac.content_tsv, plainto_tsquery('english', $1)) DESC
                 LIMIT $2",
            )
            .bind(query_trimmed)
            .bind(fetch_limit)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| AtomicCoreError::Search(format!("Keyword chunk search failed: {}", e)))?;

        // Apply scope filter
        let filtered = if scope_tag_ids.is_empty() {
            rows
        } else {
            let candidate_atom_ids: Vec<&str> =
                rows.iter().map(|(_, aid, _, _, _)| aid.as_str()).collect();
            let matching = pg_batch_atoms_with_scope_tags(
                &self.pool,
                &candidate_atom_ids,
                scope_tag_ids,
                &self.db_id,
            )
            .await?;
            rows.into_iter()
                .filter(|(_, aid, _, _, _)| matching.contains(aid.as_str()))
                .collect()
        };

        // Apply kind filter (post-pass)
        let filtered = if matches!(kinds, crate::models::KindFilter::All) {
            filtered
        } else {
            let candidate_atom_ids: Vec<&str> = filtered
                .iter()
                .map(|(_, aid, _, _, _)| aid.as_str())
                .collect();
            let allowed =
                pg_batch_atoms_matching_kind(&self.pool, &candidate_atom_ids, kinds, &self.db_id)
                    .await?;
            filtered
                .into_iter()
                .filter(|(_, aid, _, _, _)| allowed.contains(aid.as_str()))
                .collect()
        };

        Ok(filtered
            .into_iter()
            .take(limit as usize)
            .map(
                |(chunk_id, atom_id, content, chunk_index, rank)| ChunkSearchResult {
                    chunk_id,
                    atom_id,
                    content,
                    chunk_index,
                    score: rank.clamp(0.0, 1.0),
                },
            )
            .collect())
    }

    async fn vector_search_chunks(
        &self,
        query_embedding: &[f32],
        limit: i32,
        threshold: f32,
        scope_tag_ids: &[String],
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<ChunkSearchResult>> {
        let embedding_vec = Vector::from(query_embedding.to_vec());
        let fetch_limit = limit * 5;

        let rows: Vec<(String, String, String, i32, f64)> = if let Some(cutoff) = created_after {
            sqlx::query_as(
                "SELECT ac.id, ac.atom_id, ac.content, ac.chunk_index,
                        (ac.embedding <=> $1::vector) AS distance
                 FROM atom_chunks ac
                 INNER JOIN atoms a ON a.id = ac.atom_id AND a.db_id = ac.db_id
                 WHERE ac.embedding IS NOT NULL
                   AND ac.db_id = $3
                   AND a.created_at >= $4
                 ORDER BY ac.embedding <=> $1::vector
                 LIMIT $2",
            )
            .bind(&embedding_vec)
            .bind(fetch_limit)
            .bind(&self.db_id)
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as(
                "SELECT ac.id, ac.atom_id, ac.content, ac.chunk_index,
                        (ac.embedding <=> $1::vector) AS distance
                 FROM atom_chunks ac
                 WHERE ac.embedding IS NOT NULL AND ac.db_id = $3
                 ORDER BY ac.embedding <=> $1::vector
                 LIMIT $2",
            )
            .bind(&embedding_vec)
            .bind(fetch_limit)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| AtomicCoreError::Search(format!("Vector chunk search failed: {}", e)))?;

        let filtered: Vec<(String, String, String, i32, f32)> = rows
            .into_iter()
            .map(|(id, aid, content, idx, distance)| (id, aid, content, idx, 1.0 - distance as f32))
            .filter(|(_, _, _, _, similarity)| *similarity >= threshold)
            .collect();

        // Apply scope filter
        let scoped = if scope_tag_ids.is_empty() {
            filtered
        } else {
            let candidate_atom_ids: Vec<&str> = filtered
                .iter()
                .map(|(_, aid, _, _, _)| aid.as_str())
                .collect();
            let matching = pg_batch_atoms_with_scope_tags(
                &self.pool,
                &candidate_atom_ids,
                scope_tag_ids,
                &self.db_id,
            )
            .await?;
            filtered
                .into_iter()
                .filter(|(_, aid, _, _, _)| matching.contains(aid.as_str()))
                .collect()
        };

        // Apply kind filter
        let scoped = if matches!(kinds, crate::models::KindFilter::All) {
            scoped
        } else {
            let candidate_atom_ids: Vec<&str> = scoped
                .iter()
                .map(|(_, aid, _, _, _)| aid.as_str())
                .collect();
            let allowed =
                pg_batch_atoms_matching_kind(&self.pool, &candidate_atom_ids, kinds, &self.db_id)
                    .await?;
            scoped
                .into_iter()
                .filter(|(_, aid, _, _, _)| allowed.contains(aid.as_str()))
                .collect()
        };

        Ok(scoped
            .into_iter()
            .take(limit as usize)
            .map(
                |(chunk_id, atom_id, content, chunk_index, score)| ChunkSearchResult {
                    chunk_id,
                    atom_id,
                    content,
                    chunk_index,
                    score,
                },
            )
            .collect())
    }

    async fn find_similar(
        &self,
        atom_id: &str,
        limit: i32,
        threshold: f32,
    ) -> StorageResult<Vec<SimilarAtomResult>> {
        // Get all chunk embeddings for the source atom
        let source_embeddings: Vec<(i32, Vector)> = sqlx::query_as(
            "SELECT chunk_index, embedding FROM atom_chunks
             WHERE atom_id = $1 AND embedding IS NOT NULL AND db_id = $2",
        )
        .bind(atom_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::Search(format!("Failed to get source chunks: {}", e)))?;

        if source_embeddings.is_empty() {
            return Ok(Vec::new());
        }

        // For each source chunk, find similar chunks from other atoms
        let mut atom_similarities: HashMap<String, (f32, String, i32)> = HashMap::new();
        let per_chunk_limit = limit * 10;

        for (_source_chunk_index, embedding) in &source_embeddings {
            let similar: Vec<(String, String, i32, f64)> = sqlx::query_as(
                "SELECT ac.atom_id, ac.content, ac.chunk_index,
                        (ac.embedding <=> $1::vector) AS distance
                 FROM atom_chunks ac
                 WHERE ac.embedding IS NOT NULL AND ac.atom_id != $2 AND ac.db_id = $4
                 ORDER BY ac.embedding <=> $1::vector
                 LIMIT $3",
            )
            .bind(embedding)
            .bind(atom_id)
            .bind(per_chunk_limit)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to find similar chunks: {}", e))
            })?;

            for (target_atom_id, content, chunk_index, distance) in similar {
                let similarity = 1.0 - distance as f32;
                if similarity < threshold {
                    continue;
                }

                let entry = atom_similarities.entry(target_atom_id);
                match entry {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if similarity > e.get().0 {
                            e.insert((similarity, content, chunk_index));
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert((similarity, content, chunk_index));
                    }
                }
            }
        }

        // Sort and limit
        let mut results: Vec<(String, f32, String, i32)> = atom_similarities
            .into_iter()
            .map(|(id, (sim, content, idx))| (id, sim, content, idx))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit as usize);

        // Batch fetch atom data
        let atom_ids: Vec<String> = results.iter().map(|(id, _, _, _)| id.clone()).collect();
        let atom_map = pg_batch_fetch_atoms(&self.pool, &atom_ids, &self.db_id).await?;
        let tag_map = pg_batch_fetch_tags(&self.pool, &atom_ids, &self.db_id).await?;

        let mut final_results = Vec::new();
        for (result_atom_id, similarity, chunk_content, chunk_index) in results {
            if let Some(atom) = atom_map.get(&result_atom_id) {
                let tags = tag_map.get(&result_atom_id).cloned().unwrap_or_default();
                final_results.push(SimilarAtomResult {
                    atom: AtomWithTags {
                        atom: atom.clone(),
                        tags,
                    },
                    similarity_score: similarity,
                    matching_chunk_content: chunk_content,
                    matching_chunk_index: chunk_index,
                });
            }
        }

        Ok(final_results)
    }
}

// ==================== Helper Functions ====================

/// Batch fetch atoms by IDs using Postgres ANY($1).
async fn pg_batch_fetch_atoms(
    pool: &sqlx::PgPool,
    atom_ids: &[String],
    db_id: &str,
) -> Result<HashMap<String, Atom>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows: Vec<(
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
        String,
    )> = sqlx::query_as(
        "SELECT id, content, title, snippet, source_url, source, published_at,
                created_at, updated_at,
                COALESCE(embedding_status, 'pending'),
                COALESCE(tagging_status, 'pending'),
                embedding_error, tagging_error,
                COALESCE(kind, 'captured')
         FROM atoms WHERE id = ANY($1) AND db_id = $2",
    )
    .bind(atom_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::Search(format!("Failed to batch fetch atoms: {}", e)))?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let kind =
                r.13.parse::<crate::models::AtomKind>()
                    .unwrap_or(crate::models::AtomKind::Captured);
            let atom = Atom {
                id: r.0.clone(),
                content: r.1,
                title: r.2,
                snippet: r.3,
                source_url: r.4,
                source: r.5,
                published_at: r.6,
                created_at: r.7,
                updated_at: r.8,
                embedding_status: r.9,
                tagging_status: r.10,
                embedding_error: r.11,
                tagging_error: r.12,
                kind,
            };
            (r.0, atom)
        })
        .collect())
}

/// Batch fetch tags for multiple atoms using Postgres ANY($1).
async fn pg_batch_fetch_tags(
    pool: &sqlx::PgPool,
    atom_ids: &[String],
    db_id: &str,
) -> Result<HashMap<String, Vec<Tag>>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows: Vec<(String, String, String, Option<String>, String, bool, String)> = sqlx::query_as(
        "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM atom_tags at
         INNER JOIN tags t ON at.tag_id = t.id
         WHERE at.atom_id = ANY($1) AND at.db_id = $2",
    )
    .bind(atom_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::Search(format!("Failed to batch fetch tags: {}", e)))?;

    let mut map: HashMap<String, Vec<Tag>> = HashMap::new();
    for (atom_id, tag_id, name, parent_id, created_at, is_autotag_target, autotag_description) in
        rows
    {
        map.entry(atom_id).or_default().push(Tag {
            id: tag_id,
            name,
            parent_id,
            created_at,
            is_autotag_target,
            autotag_description,
        });
    }
    Ok(map)
}

/// Batch check which atom_ids have at least one of the specified scope tags.
async fn pg_batch_atoms_with_scope_tags(
    pool: &sqlx::PgPool,
    atom_ids: &[&str],
    scope_tag_ids: &[String],
    db_id: &str,
) -> Result<std::collections::HashSet<String>, AtomicCoreError> {
    if atom_ids.is_empty() || scope_tag_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    let atom_id_strings: Vec<String> = atom_ids.iter().map(|s| s.to_string()).collect();

    // Use recursive CTE to include atoms tagged with descendants of the scope tags
    let rows: Vec<(String,)> = sqlx::query_as(
        "WITH RECURSIVE scope_tags(id) AS (
            SELECT id FROM tags WHERE id = ANY($2) AND db_id = $3
            UNION ALL
            SELECT t.id FROM tags t
            INNER JOIN scope_tags st ON t.parent_id = st.id
            WHERE t.db_id = $3
         )
         SELECT DISTINCT atom_id FROM atom_tags
         WHERE atom_id = ANY($1) AND tag_id IN (SELECT id FROM scope_tags) AND db_id = $3",
    )
    .bind(&atom_id_strings)
    .bind(scope_tag_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::Search(format!("Failed to check scope tags: {}", e)))?;

    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Batch lookup of which atom_ids match the given `KindFilter`. Caller is
/// expected to skip the call for `KindFilter::All` (which is a no-op).
async fn pg_batch_atoms_matching_kind(
    pool: &sqlx::PgPool,
    atom_ids: &[&str],
    kinds: &crate::models::KindFilter,
    db_id: &str,
) -> Result<std::collections::HashSet<String>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let kind_predicate = kinds.postgres_predicate("kind", "$3");
    let sql = format!(
        "SELECT id FROM atoms
         WHERE id = ANY($1) AND db_id = $2 AND {kind_predicate}"
    );
    let atom_id_strings: Vec<String> = atom_ids.iter().map(|s| s.to_string()).collect();
    let mut q = sqlx::query_as::<_, (String,)>(&sql)
        .bind(&atom_id_strings)
        .bind(db_id);
    if kinds.has_bind_value() {
        q = q.bind(kinds.kind_strings());
    }
    let rows = q
        .fetch_all(pool)
        .await
        .map_err(|e| AtomicCoreError::Search(format!("Failed to check kind filter: {}", e)))?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

async fn pg_keyword_search_wiki(
    pool: &sqlx::PgPool,
    query: &str,
    limit: i32,
    db_id: &str,
) -> Result<Vec<GlobalWikiSearchResult>, AtomicCoreError> {
    let tag_pattern = format!("%{}%", query.to_lowercase());
    let rows: Vec<(String, String, String, String, String, i32, f32)> = sqlx::query_as(
        "SELECT w.id, w.tag_id, t.name, w.content, w.updated_at, w.atom_count,
                GREATEST(
                    COALESCE(ts_rank(w.content_tsv, plainto_tsquery('english', $1)), 0),
                    CASE
                        WHEN LOWER(t.name) = LOWER($1) THEN 1.0
                        WHEN LOWER(t.name) LIKE LOWER($3) THEN 0.95
                        ELSE 0.0
                    END
                ) AS score
         FROM wiki_articles w
         JOIN tags t ON t.id = w.tag_id AND t.db_id = $2
         WHERE w.db_id = $2
           AND (
                w.content_tsv @@ plainto_tsquery('english', $1)
                OR LOWER(t.name) LIKE LOWER($3)
           )
         ORDER BY score DESC, w.updated_at DESC
         LIMIT $4",
    )
    .bind(query)
    .bind(db_id)
    .bind(&tag_pattern)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::Search(format!("Wiki keyword search failed: {}", e)))?;

    Ok(rows
        .into_iter()
        .map(
            |(id, tag_id, tag_name, content, updated_at, atom_count, score)| {
                GlobalWikiSearchResult {
                    id,
                    tag_id,
                    tag_name,
                    content_snippet: pg_snippet(&content, 180),
                    content,
                    updated_at,
                    atom_count,
                    score: score.clamp(0.0, 1.0),
                    match_snippet: None,
                    match_offsets: None,
                    match_count: None,
                }
            },
        )
        .collect())
}

async fn pg_keyword_search_chats(
    pool: &sqlx::PgPool,
    query: &str,
    limit: i32,
    db_id: &str,
) -> Result<Vec<GlobalChatSearchResult>, AtomicCoreError> {
    let title_pattern = format!("%{}%", query.to_lowercase());
    let rows: Vec<(String, Option<String>, String, i64, String, f32)> = sqlx::query_as(
        "WITH message_hits AS (
            SELECT c.id,
                   c.title,
                   c.updated_at,
                   COUNT(m_all.id) AS message_count,
                   m.content AS matching_message_content,
                   ts_rank(m.content_tsv, plainto_tsquery('english', $1)) AS score,
                   ROW_NUMBER() OVER (
                       PARTITION BY c.id
                       ORDER BY ts_rank(m.content_tsv, plainto_tsquery('english', $1)) DESC, m.message_index DESC
                   ) AS rn
            FROM conversations c
            JOIN chat_messages m
              ON m.conversation_id = c.id AND m.db_id = c.db_id
            LEFT JOIN chat_messages m_all
              ON m_all.conversation_id = c.id AND m_all.db_id = c.db_id
            WHERE c.db_id = $2
              AND c.is_archived = 0
              AND m.content_tsv @@ plainto_tsquery('english', $1)
            GROUP BY c.id, c.title, c.updated_at, m.id, m.content, m.content_tsv, m.message_index
         ),
         title_hits AS (
            SELECT c.id,
                   c.title,
                   c.updated_at,
                   COUNT(m_all.id) AS message_count,
                   COALESCE(c.title, '') AS matching_message_content,
                   CASE
                       WHEN LOWER(c.title) = LOWER($1) THEN 1.0
                       WHEN LOWER(c.title) LIKE LOWER($3) THEN 0.9
                       ELSE 0.0
                   END AS score,
                   1 AS rn
            FROM conversations c
            LEFT JOIN chat_messages m_all
              ON m_all.conversation_id = c.id AND m_all.db_id = c.db_id
            WHERE c.db_id = $2
              AND c.is_archived = 0
              AND c.title IS NOT NULL
              AND LOWER(c.title) LIKE LOWER($3)
            GROUP BY c.id, c.title, c.updated_at
         ),
         combined AS (
            SELECT * FROM message_hits WHERE rn = 1
            UNION ALL
            SELECT * FROM title_hits
         ),
         ranked AS (
            SELECT *,
                   ROW_NUMBER() OVER (PARTITION BY id ORDER BY score DESC, updated_at DESC) AS best_rn
            FROM combined
         )
         SELECT id, title, updated_at, message_count, matching_message_content, score
         FROM ranked
         WHERE best_rn = 1
         ORDER BY score DESC, updated_at DESC
         LIMIT $4",
    )
    .bind(query)
    .bind(db_id)
    .bind(&title_pattern)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::Search(format!("Chat keyword search failed: {}", e)))?;

    let conversation_ids: Vec<String> = rows.iter().map(|(id, _, _, _, _, _)| id.clone()).collect();
    let tag_map = pg_batch_fetch_conversation_tags(pool, &conversation_ids, db_id).await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, title, updated_at, message_count, matching_message_content, score)| {
                GlobalChatSearchResult {
                    id: id.clone(),
                    title,
                    updated_at,
                    message_count: message_count as i32,
                    tags: tag_map.get(&id).cloned().unwrap_or_default(),
                    matching_message_content: pg_snippet(&matching_message_content, 180),
                    score: score.clamp(0.0, 1.0),
                }
            },
        )
        .collect())
}

async fn pg_keyword_search_tags(
    pool: &sqlx::PgPool,
    query: &str,
    limit: i32,
    db_id: &str,
) -> Result<Vec<GlobalTagSearchResult>, AtomicCoreError> {
    let pattern = format!("%{}%", query.to_lowercase());
    let prefix_pattern = format!("{}%", query.to_lowercase());
    let rows: Vec<(String, String, Option<String>, String, i32, f32)> = sqlx::query_as(
        "SELECT id, name, parent_id, created_at, atom_count,
                CASE
                    WHEN LOWER(name) = LOWER($1) THEN 1.0
                    WHEN LOWER(name) LIKE LOWER($3) THEN 0.95
                    WHEN LOWER(name) LIKE LOWER($2) THEN 0.8
                    ELSE 0.0
                END AS score
         FROM tags
         WHERE db_id = $4
           AND LOWER(name) LIKE LOWER($2)
         ORDER BY score DESC, atom_count DESC, name ASC
         LIMIT $5",
    )
    .bind(query)
    .bind(&pattern)
    .bind(&prefix_pattern)
    .bind(db_id)
    .bind(limit * 4)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::Search(format!("Tag keyword search failed: {}", e)))?;

    let query_lower = query.to_lowercase();
    let mut results: Vec<GlobalTagSearchResult> = rows
        .into_iter()
        .filter_map(|(id, name, parent_id, created_at, atom_count, score)| {
            let lower = name.to_lowercase();
            let exactish = lower == query_lower
                || lower.starts_with(&query_lower)
                || pg_strong_substring_match(&lower, &query_lower);
            if !exactish {
                return None;
            }
            Some(GlobalTagSearchResult {
                id,
                name,
                parent_id,
                created_at,
                atom_count,
                score: score.clamp(0.0, 1.0),
            })
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.atom_count.cmp(&a.atom_count))
            .then(a.name.cmp(&b.name))
    });
    results.truncate(limit as usize);
    Ok(results)
}

async fn pg_batch_fetch_conversation_tags(
    pool: &sqlx::PgPool,
    conversation_ids: &[String],
    db_id: &str,
) -> Result<HashMap<String, Vec<Tag>>, AtomicCoreError> {
    if conversation_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let rows: Vec<(String, String, String, Option<String>, String, bool, String)> = sqlx::query_as(
        "SELECT ct.conversation_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM conversation_tags ct
         JOIN tags t ON ct.tag_id = t.id
         WHERE ct.conversation_id = ANY($1) AND ct.db_id = $2 AND t.db_id = $2
         ORDER BY t.name",
    )
    .bind(conversation_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        AtomicCoreError::Search(format!("Failed to batch fetch conversation tags: {}", e))
    })?;

    let mut map: HashMap<String, Vec<Tag>> = HashMap::new();
    for (
        conversation_id,
        id,
        name,
        parent_id,
        created_at,
        is_autotag_target,
        autotag_description,
    ) in rows
    {
        map.entry(conversation_id).or_default().push(Tag {
            id,
            name,
            parent_id,
            created_at,
            is_autotag_target,
            autotag_description,
        });
    }
    Ok(map)
}

fn pg_snippet(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let snippet: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}...", snippet)
    } else {
        snippet
    }
}

fn pg_strong_substring_match(haystack: &str, needle: &str) -> bool {
    if needle.len() < 2 {
        return haystack == needle;
    }
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment.contains(needle))
}
