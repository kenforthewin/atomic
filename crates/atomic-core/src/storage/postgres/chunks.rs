use std::collections::HashMap;

use super::PostgresStorage;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;
use pgvector::Vector;
use uuid::Uuid;

#[async_trait]
impl ChunkStore for PostgresStorage {
    async fn get_pending_embeddings(&self, limit: i32) -> StorageResult<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, content FROM atoms WHERE embedding_status = 'pending' AND db_id = $2 LIMIT $1",
        )
        .bind(limit)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to get pending embeddings: {}", e))
        })?;
        Ok(rows)
    }

    async fn set_embedding_status(
        &self,
        atom_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()> {
        sqlx::query("UPDATE atoms SET embedding_status = $2, embedding_error = $3 WHERE id = $1 AND db_id = $4")
            .bind(atom_id)
            .bind(status)
            .bind(error)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!(
                    "Failed to set embedding status: {}",
                    e
                ))
            })?;
        Ok(())
    }

    async fn set_tagging_status(
        &self,
        atom_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()> {
        sqlx::query(
            "UPDATE atoms SET tagging_status = $2, tagging_error = $3 WHERE id = $1 AND db_id = $4",
        )
        .bind(atom_id)
        .bind(status)
        .bind(error)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to set tagging status: {}", e))
        })?;
        Ok(())
    }

    async fn save_chunks_and_embeddings(
        &self,
        atom_id: &str,
        chunks: &[(String, Vec<f32>)],
    ) -> StorageResult<()> {
        let mut tx = self.pool.begin().await.map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to begin transaction: {}", e))
        })?;

        // Delete existing chunks for this atom (CASCADE handles nothing else since
        // Postgres unifies chunks + embeddings + FTS in one table)
        sqlx::query("DELETE FROM atom_chunks WHERE atom_id = $1 AND db_id = $2")
            .bind(atom_id)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to delete old chunks: {}", e))
            })?;

        // Insert new chunks with embeddings as pgvector Vector
        for (index, (chunk_content, embedding_vec)) in chunks.iter().enumerate() {
            let chunk_id = Uuid::new_v4().to_string();
            let pg_embedding = Vector::from(embedding_vec.clone());

            sqlx::query(
                "INSERT INTO atom_chunks (id, atom_id, chunk_index, content, embedding, db_id)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&chunk_id)
            .bind(atom_id)
            .bind(index as i32)
            .bind(chunk_content)
            .bind(&pg_embedding)
            .bind(&self.db_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to insert chunk: {}", e))
            })?;
        }

        tx.commit().await.map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to commit transaction: {}", e))
        })?;

        Ok(())
    }

    async fn get_chunks_for_atoms(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<ExistingAtomChunk>> {
        if atom_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, String, i32, String)> = sqlx::query_as(
            "SELECT id, atom_id, chunk_index, content
             FROM atom_chunks
             WHERE atom_id = ANY($1) AND db_id = $2
             ORDER BY atom_id, chunk_index",
        )
        .bind(atom_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to fetch existing chunks: {}", e))
        })?;
        Ok(rows
            .into_iter()
            .map(|(id, atom_id, chunk_index, content)| ExistingAtomChunk {
                id,
                atom_id,
                chunk_index,
                content,
            })
            .collect())
    }

    async fn update_chunk_embeddings(&self, chunks: &[(String, Vec<f32>)]) -> StorageResult<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to begin transaction: {}", e))
        })?;
        for (chunk_id, embedding_vec) in chunks {
            let pg_embedding = Vector::from(embedding_vec.clone());
            sqlx::query("UPDATE atom_chunks SET embedding = $1 WHERE id = $2 AND db_id = $3")
                .bind(&pg_embedding)
                .bind(chunk_id)
                .bind(&self.db_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    AtomicCoreError::DatabaseOperation(format!(
                        "Failed to update chunk embedding: {}",
                        e
                    ))
                })?;
        }
        tx.commit().await.map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to commit transaction: {}", e))
        })?;
        Ok(())
    }

    async fn delete_chunks(&self, atom_id: &str) -> StorageResult<()> {
        sqlx::query("DELETE FROM atom_chunks WHERE atom_id = $1 AND db_id = $2")
            .bind(atom_id)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to delete chunks: {}", e))
            })?;
        Ok(())
    }

    async fn reset_stuck_processing(&self) -> StorageResult<i32> {
        let embedding_result = sqlx::query(
            "UPDATE atoms SET embedding_status = 'pending' WHERE embedding_status = 'processing' AND db_id = $1",
        )
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to reset stuck embedding status: {}",
                e
            ))
        })?;

        let tagging_result = sqlx::query(
            "UPDATE atoms SET tagging_status = 'pending' WHERE tagging_status = 'processing' AND db_id = $1",
        )
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to reset stuck tagging status: {}",
                e
            ))
        })?;

        let edges_result = sqlx::query(
            "UPDATE atoms SET edges_status = 'pending' WHERE edges_status = 'processing' AND db_id = $1",
        )
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to reset stuck edges status: {}",
                e
            ))
        })?;

        Ok((embedding_result.rows_affected()
            + tagging_result.rows_affected()
            + edges_result.rows_affected()) as i32)
    }

    async fn reset_failed_embeddings(&self) -> StorageResult<i32> {
        let embedding_result = sqlx::query(
            "UPDATE atoms SET embedding_status = 'pending', embedding_error = NULL WHERE embedding_status = 'failed' AND db_id = $1",
        )
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to reset failed embeddings: {}",
                e
            ))
        })?;

        let tagging_result = sqlx::query(
            "UPDATE atoms SET tagging_status = 'pending', tagging_error = NULL WHERE tagging_status = 'failed' AND db_id = $1",
        )
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to reset failed tagging: {}",
                e
            ))
        })?;

        Ok((embedding_result.rows_affected() + tagging_result.rows_affected()) as i32)
    }

    async fn reset_failed_embedding_statuses(&self) -> StorageResult<i32> {
        let result = sqlx::query(
            "UPDATE atoms
             SET embedding_status = 'pending', embedding_error = NULL
             WHERE embedding_status = 'failed' AND db_id = $1",
        )
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to reset failed embeddings: {}", e))
        })?;

        Ok(result.rows_affected() as i32)
    }

    async fn reset_failed_tagging_statuses(&self) -> StorageResult<i32> {
        let result = sqlx::query(
            "UPDATE atoms
             SET tagging_status = 'pending', tagging_error = NULL
             WHERE tagging_status = 'failed'
               AND embedding_status = 'complete'
               AND db_id = $1",
        )
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to reset failed tagging: {}", e))
        })?;

        Ok(result.rows_affected() as i32)
    }

    async fn rebuild_semantic_edges(&self) -> StorageResult<i32> {
        // Get all atoms with completed embeddings
        let atom_ids: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT a.id FROM atoms a
             INNER JOIN atom_chunks ac ON a.id = ac.atom_id
             WHERE a.embedding_status = 'complete' AND ac.embedding IS NOT NULL AND a.db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to get atoms with embeddings: {}",
                e
            ))
        })?;

        let atom_ids: Vec<String> = atom_ids.into_iter().map(|(id,)| id).collect();

        // Clear all existing semantic edges
        sqlx::query("DELETE FROM semantic_edges WHERE db_id = $1")
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!(
                    "Failed to delete semantic edges: {}",
                    e
                ))
            })?;

        let mut total_edges = 0;
        let threshold = 0.5f32;
        let max_edges = 15i32;

        for (idx, atom_id) in atom_ids.iter().enumerate() {
            match self
                .compute_semantic_edges_for_atom_impl(atom_id, threshold, max_edges)
                .await
            {
                Ok(edge_count) => {
                    total_edges += edge_count;
                    if (idx + 1) % 50 == 0 {
                        tracing::info!(
                            progress = idx + 1,
                            total = atom_ids.len(),
                            total_edges,
                            "Edge computation progress"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        atom_id = %atom_id,
                        error = %e,
                        "Failed to compute edges for atom"
                    );
                }
            }
        }

        Ok(total_edges)
    }

    async fn get_semantic_edges(&self, min_similarity: f32) -> StorageResult<Vec<SemanticEdge>> {
        let rows: Vec<(
            String,
            String,
            String,
            f32,
            Option<i32>,
            Option<i32>,
            String,
        )> = sqlx::query_as(
            "SELECT id, source_atom_id, target_atom_id, similarity_score,
                        source_chunk_index, target_chunk_index, created_at
                 FROM semantic_edges
                 WHERE similarity_score >= $1 AND db_id = $2
                 ORDER BY similarity_score DESC
                 LIMIT 10000",
        )
        .bind(min_similarity)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to get semantic edges: {}", e))
        })?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    source_atom_id,
                    target_atom_id,
                    similarity_score,
                    source_chunk_index,
                    target_chunk_index,
                    created_at,
                )| {
                    SemanticEdge {
                        id,
                        source_atom_id,
                        target_atom_id,
                        similarity_score,
                        source_chunk_index,
                        target_chunk_index,
                        created_at,
                    }
                },
            )
            .collect())
    }

    async fn get_atom_neighborhood(
        &self,
        atom_id: &str,
        depth: i32,
        min_similarity: f32,
    ) -> StorageResult<NeighborhoodGraph> {
        let mut atoms_at_depth: HashMap<String, i32> = HashMap::new();
        atoms_at_depth.insert(atom_id.to_string(), 0);

        // Depth 1 semantic connections
        let semantic_d1: Vec<(String, f32)> = sqlx::query_as(
            "SELECT
                CASE WHEN source_atom_id = $1 THEN target_atom_id ELSE source_atom_id END AS other_atom_id,
                similarity_score
             FROM semantic_edges
             WHERE (source_atom_id = $1 OR target_atom_id = $1)
               AND similarity_score >= $2 AND db_id = $3
             ORDER BY similarity_score DESC
             LIMIT 20",
        )
        .bind(atom_id)
        .bind(min_similarity)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to get semantic neighbors: {}",
                e
            ))
        })?;

        for (other_id, _) in &semantic_d1 {
            atoms_at_depth.entry(other_id.clone()).or_insert(1);
        }

        // Depth 1 tag connections
        let center_tags: Vec<(String,)> =
            sqlx::query_as("SELECT tag_id FROM atom_tags WHERE atom_id = $1 AND db_id = $2")
                .bind(atom_id)
                .bind(&self.db_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| {
                    AtomicCoreError::DatabaseOperation(format!("Failed to get center tags: {}", e))
                })?;

        let center_tag_ids: Vec<String> = center_tags.into_iter().map(|(id,)| id).collect();

        if !center_tag_ids.is_empty() {
            let tag_neighbors: Vec<(String, i64)> = sqlx::query_as(
                "SELECT atom_id, COUNT(*) AS shared_count
                 FROM atom_tags
                 WHERE tag_id = ANY($1)
                   AND atom_id != $2 AND db_id = $3
                 GROUP BY atom_id
                 HAVING COUNT(*) >= 1
                 ORDER BY COUNT(*) DESC
                 LIMIT 20",
            )
            .bind(&center_tag_ids)
            .bind(atom_id)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to get tag neighbors: {}", e))
            })?;

            for (other_id, _) in &tag_neighbors {
                atoms_at_depth.entry(other_id.clone()).or_insert(1);
            }
        }

        // Depth 2 if requested
        if depth >= 2 {
            let depth1_ids: Vec<String> = atoms_at_depth
                .iter()
                .filter(|(_, d)| **d == 1)
                .map(|(id, _)| id.clone())
                .collect();

            for d1_id in &depth1_ids {
                let d2_rows: Vec<(String,)> = sqlx::query_as(
                    "SELECT
                        CASE WHEN source_atom_id = $1 THEN target_atom_id ELSE source_atom_id END
                     FROM semantic_edges
                     WHERE (source_atom_id = $1 OR target_atom_id = $1)
                       AND similarity_score >= $2 AND db_id = $3
                     ORDER BY similarity_score DESC
                     LIMIT 5",
                )
                .bind(d1_id)
                .bind(min_similarity)
                .bind(&self.db_id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| {
                    AtomicCoreError::DatabaseOperation(format!(
                        "Failed to get depth-2 neighbors: {}",
                        e
                    ))
                })?;

                for (d2_id,) in d2_rows {
                    atoms_at_depth.entry(d2_id).or_insert(2);
                }
            }
        }

        // Limit total atoms
        let max_atoms = if depth >= 2 { 30 } else { 20 };
        let mut sorted_atoms: Vec<(String, i32)> = atoms_at_depth.into_iter().collect();
        sorted_atoms.sort_by_key(|(_, d)| *d);
        sorted_atoms.truncate(max_atoms);

        let atom_ids: Vec<String> = sorted_atoms.iter().map(|(id, _)| id.clone()).collect();
        let atom_depths: HashMap<String, i32> = sorted_atoms.into_iter().collect();

        // Batch fetch atom data
        let atom_rows: Vec<(
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
        )> = sqlx::query_as(
            "SELECT id, content, title, snippet, source_url, source, published_at,
                    created_at, updated_at,
                    COALESCE(embedding_status, 'pending'),
                    COALESCE(tagging_status, 'pending'),
                    embedding_error, tagging_error
             FROM atoms WHERE id = ANY($1) AND db_id = $2",
        )
        .bind(&atom_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to batch fetch neighborhood atoms: {}",
                e
            ))
        })?;

        let atom_lookup: HashMap<String, Atom> = atom_rows
            .into_iter()
            .map(|r| {
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
                                is_locked: false,
                            };
                (r.0, atom)
            })
            .collect();

        // Batch fetch tags
        let tag_rows: Vec<(String, String, String, Option<String>, String, bool, String)> = sqlx::query_as(
            "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
             FROM atom_tags at
             INNER JOIN tags t ON at.tag_id = t.id
             WHERE at.atom_id = ANY($1) AND at.db_id = $2",
        )
        .bind(&atom_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to batch fetch neighborhood tags: {}",
                e
            ))
        })?;

        let mut tag_map: HashMap<String, Vec<Tag>> = HashMap::new();
        for (atom_id_val, tag_id, name, parent_id, created_at, is_autotag_target, autotag_description) in tag_rows {
            tag_map.entry(atom_id_val).or_default().push(Tag {
                id: tag_id,
                name,
                parent_id,
                created_at,
                is_autotag_target,
                autotag_description,
            });
        }

        let mut atoms = Vec::new();
        for aid in &atom_ids {
            if let Some(atom) = atom_lookup.get(aid) {
                let tags = tag_map.get(aid).cloned().unwrap_or_default();
                let d = *atom_depths.get(aid).unwrap_or(&0);
                atoms.push(NeighborhoodAtom {
                    atom: AtomWithTags {
                        atom: atom.clone(),
                        tags,
                    },
                    depth: d,
                });
            }
        }

        // Batch fetch semantic edges between these atoms
        let semantic_edges_rows: Vec<(String, String, f32)> = sqlx::query_as(
            "SELECT source_atom_id, target_atom_id, similarity_score
             FROM semantic_edges
             WHERE source_atom_id = ANY($1) AND target_atom_id = ANY($1) AND db_id = $2",
        )
        .bind(&atom_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to fetch neighborhood edges: {}", e))
        })?;

        let semantic_edges: HashMap<(String, String), f32> = semantic_edges_rows
            .into_iter()
            .map(|(src, tgt, score)| ((src, tgt), score))
            .collect();

        // Batch fetch shared tag counts
        let shared_tag_rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT a1.atom_id, a2.atom_id, COUNT(*) AS shared
             FROM atom_tags a1
             INNER JOIN atom_tags a2 ON a1.tag_id = a2.tag_id
             WHERE a1.atom_id = ANY($1) AND a2.atom_id = ANY($1)
               AND a1.atom_id < a2.atom_id AND a1.db_id = $2
             GROUP BY a1.atom_id, a2.atom_id",
        )
        .bind(&atom_ids)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to fetch shared tags: {}", e))
        })?;

        let shared_tags_map: HashMap<(String, String), i32> = shared_tag_rows
            .into_iter()
            .map(|(a, b, count)| ((a, b), count as i32))
            .collect();

        // Build edges
        let mut edges = Vec::new();
        for i in 0..atom_ids.len() {
            for j in (i + 1)..atom_ids.len() {
                let id_a = &atom_ids[i];
                let id_b = &atom_ids[j];

                let semantic_score = semantic_edges
                    .get(&(id_a.clone(), id_b.clone()))
                    .or_else(|| semantic_edges.get(&(id_b.clone(), id_a.clone())))
                    .copied();

                let (key_a, key_b) = if id_a < id_b {
                    (id_a, id_b)
                } else {
                    (id_b, id_a)
                };
                let shared_tags = shared_tags_map
                    .get(&(key_a.clone(), key_b.clone()))
                    .copied()
                    .unwrap_or(0);

                if semantic_score.is_some() || shared_tags > 0 {
                    let edge_type = match (semantic_score.is_some(), shared_tags > 0) {
                        (true, true) => "both",
                        (true, false) => "semantic",
                        (false, true) => "tag",
                        (false, false) => continue,
                    };

                    let semantic_strength = semantic_score.unwrap_or(0.0);
                    let tag_strength = (shared_tags as f32 * 0.15).min(0.6);
                    let strength = (semantic_strength + tag_strength).min(1.0);

                    edges.push(NeighborhoodEdge {
                        source_id: id_a.clone(),
                        target_id: id_b.clone(),
                        edge_type: edge_type.to_string(),
                        strength,
                        shared_tag_count: shared_tags,
                        similarity_score: semantic_score,
                    });
                }
            }
        }

        Ok(NeighborhoodGraph {
            center_atom_id: atom_id.to_string(),
            atoms,
            edges,
        })
    }

    async fn get_connection_counts(
        &self,
        min_similarity: f32,
    ) -> StorageResult<HashMap<String, i32>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT atom_id, COUNT(*) AS cnt FROM (
                SELECT source_atom_id AS atom_id FROM semantic_edges WHERE similarity_score >= $1 AND db_id = $2
                UNION ALL
                SELECT target_atom_id AS atom_id FROM semantic_edges WHERE similarity_score >= $1 AND db_id = $2
            ) sub GROUP BY atom_id",
        )
        .bind(min_similarity)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to get connection counts: {}",
                e
            ))
        })?;

        Ok(rows
            .into_iter()
            .map(|(id, count)| (id, count as i32))
            .collect())
    }

    async fn save_tag_centroid(&self, tag_id: &str, embedding: &[f32]) -> StorageResult<()> {
        let pg_embedding = Vector::from(embedding.to_vec());

        sqlx::query(
            "INSERT INTO tag_embeddings (tag_id, embedding, db_id)
             VALUES ($1, $2, $3)
             ON CONFLICT (tag_id, db_id) DO UPDATE SET embedding = EXCLUDED.embedding",
        )
        .bind(tag_id)
        .bind(&pg_embedding)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to save tag centroid: {}", e))
        })?;

        Ok(())
    }

    async fn recompute_all_tag_embeddings(&self) -> StorageResult<i32> {
        // Get all tags that have at least one atom with embeddings
        let tag_ids: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT at.tag_id
             FROM atom_tags at
             INNER JOIN atom_chunks ac ON at.atom_id = ac.atom_id
             WHERE ac.embedding IS NOT NULL AND at.db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to get tags with embeddings: {}", e))
        })?;

        let tag_ids: Vec<String> = tag_ids.into_iter().map(|(id,)| id).collect();
        let count = tag_ids.len() as i32;
        tracing::info!(count, "Recomputing centroid embeddings for tags");

        for tag_id in &tag_ids {
            // Get all descendant tag IDs (recursive CTE)
            let descendant_ids: Vec<(String,)> = sqlx::query_as(
                "WITH RECURSIVE descendant_tags(id) AS (
                    SELECT $1::text
                    UNION ALL
                    SELECT t.id FROM tags t
                    INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                    WHERE t.db_id = $2
                )
                SELECT id FROM descendant_tags",
            )
            .bind(tag_id)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to get tag descendants: {}", e))
            })?;

            let desc_ids: Vec<String> = descendant_ids.into_iter().map(|(id,)| id).collect();

            // Get all chunk embeddings for atoms tagged with any descendant tag
            let embeddings: Vec<(Vector,)> = sqlx::query_as(
                "SELECT ac.embedding
                 FROM atom_chunks ac
                 INNER JOIN atom_tags at ON ac.atom_id = at.atom_id
                 WHERE at.tag_id = ANY($1) AND ac.embedding IS NOT NULL AND at.db_id = $2",
            )
            .bind(&desc_ids)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!(
                    "Failed to get embeddings for tag centroid: {}",
                    e
                ))
            })?;

            if embeddings.is_empty() {
                continue;
            }

            // Compute centroid (average of all embeddings)
            let embedding_vecs: Vec<Vec<f32>> =
                embeddings.into_iter().map(|(v,)| v.to_vec()).collect();

            let dim = embedding_vecs[0].len();
            let mut centroid = vec![0.0f32; dim];
            let n = embedding_vecs.len() as f32;

            for emb in &embedding_vecs {
                for (i, val) in emb.iter().enumerate() {
                    if i < dim {
                        centroid[i] += val;
                    }
                }
            }
            for val in centroid.iter_mut() {
                *val /= n;
            }

            // Normalize the centroid
            let magnitude: f32 = centroid.iter().map(|v| v * v).sum::<f32>().sqrt();
            if magnitude > 0.0 {
                for val in centroid.iter_mut() {
                    *val /= magnitude;
                }
            }

            // Save the centroid
            self.save_tag_centroid(tag_id, &centroid).await?;
        }

        tracing::info!(count, "Tag centroid embeddings recomputed");
        Ok(count)
    }

    async fn claim_pending_embeddings(&self, limit: i32) -> StorageResult<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "UPDATE atoms SET embedding_status = 'processing'
             WHERE id IN (SELECT id FROM atoms WHERE embedding_status = 'pending' AND db_id = $2 LIMIT $1)
             AND db_id = $2
             RETURNING id, content",
        )
        .bind(limit)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to claim pending embeddings: {}",
                e
            ))
        })?;
        Ok(rows)
    }

    async fn claim_pending_embeddings_due(
        &self,
        limit: i32,
        max_updated_at: &str,
    ) -> StorageResult<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "UPDATE atoms SET embedding_status = 'processing'
             WHERE id IN (
                 SELECT id FROM atoms
                 WHERE embedding_status = 'pending'
                   AND updated_at <= $3
                   AND db_id = $2
                 LIMIT $1
             )
             AND db_id = $2
             RETURNING id, content",
        )
        .bind(limit)
        .bind(&self.db_id)
        .bind(max_updated_at)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(rows)
    }

    async fn delete_chunks_batch(&self, atom_ids: &[String]) -> StorageResult<()> {
        sqlx::query("DELETE FROM atom_chunks WHERE atom_id = ANY($1) AND db_id = $2")
            .bind(atom_ids)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to delete chunks batch: {}", e))
            })?;
        Ok(())
    }

    async fn compute_semantic_edges_for_atom(
        &self,
        atom_id: &str,
        threshold: f32,
        max_edges: i32,
    ) -> StorageResult<i32> {
        // Delegate to the private helper method
        self.compute_semantic_edges_for_atom_impl(atom_id, threshold, max_edges)
            .await
    }

    async fn rebuild_fts_index(&self) -> StorageResult<()> {
        // No-op for Postgres: tsvector is auto-maintained via generated column
        Ok(())
    }

    async fn check_vector_extension(&self) -> StorageResult<String> {
        let version: (String,) =
            sqlx::query_as("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
                .fetch_one(&self.pool)
                .await
                .map_err(|e| {
                    AtomicCoreError::DatabaseOperation(format!(
                        "pgvector extension not found: {}",
                        e
                    ))
                })?;
        Ok(format!("pgvector {}", version.0))
    }

    async fn claim_pending_tagging(&self) -> StorageResult<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "UPDATE atoms SET tagging_status = 'processing'
             WHERE embedding_status = 'complete'
             AND tagging_status = 'pending'
             AND db_id = $1
             RETURNING id",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn claim_pending_tagging_due(&self, max_updated_at: &str) -> StorageResult<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "UPDATE atoms SET tagging_status = 'processing'
             WHERE embedding_status = 'complete'
               AND tagging_status = 'pending'
               AND updated_at <= $2
               AND db_id = $1
             RETURNING id",
        )
        .bind(&self.db_id)
        .bind(max_updated_at)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn get_embedding_dimension(&self) -> StorageResult<Option<usize>> {
        let dim: Option<i32> = sqlx::query_scalar::<_, Option<i32>>(
            "SELECT atttypmod FROM pg_attribute
             WHERE attrelid = 'atom_chunks'::regclass
             AND attname = 'embedding'
             AND atttypmod > 0",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        .flatten();
        Ok(dim.map(|d| d as usize))
    }

    async fn recreate_vector_index(&self, dimension: usize) -> StorageResult<()> {
        // Embedding model is a global setting — dimension change affects all databases.
        // Clear old vectors before ALTER so existing chunk rows can be preserved
        // even when their previous embedding dimension differs from the new one.
        sqlx::query("UPDATE atom_chunks SET embedding = NULL")
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query(&format!(
            "ALTER TABLE atom_chunks ALTER COLUMN embedding TYPE vector({}) USING NULL",
            dimension
        ))
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to alter vector dimension: {}", e))
        })?;

        // Reset all atoms across all databases for embed-only re-embedding.
        sqlx::query("UPDATE atoms SET embedding_status = 'pending'")
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query("UPDATE atoms SET tagging_status = 'skipped'")
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query("DELETE FROM semantic_edges")
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        sqlx::query("DELETE FROM tag_embeddings")
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        Ok(())
    }

    async fn claim_pending_reembedding(&self) -> StorageResult<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "UPDATE atoms SET embedding_status = 'processing'
             WHERE embedding_status IN ('pending', 'processing')
             AND db_id = $1
             RETURNING id",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn claim_all_for_reembedding(&self) -> StorageResult<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "UPDATE atoms SET embedding_status = 'processing'
             WHERE db_id = $1
             RETURNING id",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn claim_pending_edges(&self, limit: i32) -> StorageResult<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "UPDATE atoms SET edges_status = 'processing'
             WHERE id IN (SELECT id FROM atoms WHERE edges_status = 'pending' AND embedding_status = 'complete' AND db_id = $2 LIMIT $1)
             AND db_id = $2
             RETURNING id",
        )
        .bind(limit)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn set_edges_status_batch(&self, atom_ids: &[String], status: &str) -> StorageResult<()> {
        for atom_id in atom_ids {
            sqlx::query("UPDATE atoms SET edges_status = $1 WHERE id = $2 AND db_id = $3")
                .bind(status)
                .bind(atom_id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        }
        Ok(())
    }

    async fn count_pending_edges(&self) -> StorageResult<i32> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM atoms WHERE edges_status = 'pending' AND embedding_status = 'complete' AND db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(row.0 as i32)
    }

    async fn enqueue_pipeline_jobs(&self, jobs: &[AtomPipelineJobRequest]) -> StorageResult<i32> {
        if jobs.is_empty() {
            return Ok(0);
        }

        let mut tx = self.pool.begin().await.map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to begin transaction: {}", e))
        })?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut count = 0i32;

        for job in jobs {
            if !job.embed_requested && !job.tag_requested {
                continue;
            }
            let not_before = job.not_before.as_deref().unwrap_or(&now);
            let result = sqlx::query(
                "INSERT INTO atom_pipeline_jobs (
                    atom_id, db_id, embed_requested, tag_requested, reason, not_before,
                    state, lease_until, attempts, atom_updated_at, last_error,
                    created_at, updated_at
                 )
                 SELECT id, db_id, $3, $4, $5, $6, 'pending', NULL, 0, updated_at, NULL, $7, $7
                 FROM atoms
                 WHERE id = $1 AND db_id = $2
                 ON CONFLICT(atom_id, db_id) DO UPDATE SET
                    embed_requested = atom_pipeline_jobs.embed_requested OR EXCLUDED.embed_requested,
                    tag_requested = atom_pipeline_jobs.tag_requested OR EXCLUDED.tag_requested,
                    reason = EXCLUDED.reason,
                    not_before = LEAST(atom_pipeline_jobs.not_before, EXCLUDED.not_before),
                    state = 'pending',
                    lease_until = NULL,
                    atom_updated_at = EXCLUDED.atom_updated_at,
                    last_error = NULL,
                    updated_at = EXCLUDED.updated_at",
            )
            .bind(&job.atom_id)
            .bind(&self.db_id)
            .bind(job.embed_requested)
            .bind(job.tag_requested)
            .bind(&job.reason)
            .bind(not_before)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!(
                    "Failed to enqueue pipeline job: {}",
                    e
                ))
            })?;
            count += result.rows_affected() as i32;
        }

        tx.commit().await.map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to commit transaction: {}", e))
        })?;
        Ok(count)
    }

    async fn enqueue_pipeline_jobs_from_statuses(
        &self,
        max_updated_at: Option<&str>,
    ) -> StorageResult<i32> {
        let now = chrono::Utc::now().to_rfc3339();
        let result = if let Some(cutoff) = max_updated_at {
            sqlx::query(
                "INSERT INTO atom_pipeline_jobs (
                    atom_id, db_id, embed_requested, tag_requested, reason, not_before,
                    state, lease_until, attempts, atom_updated_at, last_error,
                    created_at, updated_at
                 )
                 SELECT id,
                        db_id,
                        embedding_status = 'pending',
                        tagging_status = 'pending',
                        'status-backfill',
                        $2,
                        'pending',
                        NULL,
                        0,
                        updated_at,
                        NULL,
                        $2,
                        $2
                 FROM atoms
                 WHERE db_id = $1
                   AND updated_at <= $3
                   AND (
                     embedding_status = 'pending'
                     OR (embedding_status = 'complete' AND tagging_status = 'pending')
                   )
                 ON CONFLICT(atom_id, db_id) DO UPDATE SET
                    embed_requested = atom_pipeline_jobs.embed_requested OR EXCLUDED.embed_requested,
                    tag_requested = atom_pipeline_jobs.tag_requested OR EXCLUDED.tag_requested,
                    reason = EXCLUDED.reason,
                    not_before = LEAST(atom_pipeline_jobs.not_before, EXCLUDED.not_before),
                    state = 'pending',
                    lease_until = NULL,
                    atom_updated_at = EXCLUDED.atom_updated_at,
                    last_error = NULL,
                    updated_at = EXCLUDED.updated_at",
            )
            .bind(&self.db_id)
            .bind(&now)
            .bind(cutoff)
            .execute(&self.pool)
            .await
        } else {
            sqlx::query(
                "INSERT INTO atom_pipeline_jobs (
                    atom_id, db_id, embed_requested, tag_requested, reason, not_before,
                    state, lease_until, attempts, atom_updated_at, last_error,
                    created_at, updated_at
                 )
                 SELECT id,
                        db_id,
                        embedding_status = 'pending',
                        tagging_status = 'pending',
                        'status-backfill',
                        $2,
                        'pending',
                        NULL,
                        0,
                        updated_at,
                        NULL,
                        $2,
                        $2
                 FROM atoms
                 WHERE db_id = $1
                   AND (
                     embedding_status = 'pending'
                     OR (embedding_status = 'complete' AND tagging_status = 'pending')
                   )
                 ON CONFLICT(atom_id, db_id) DO UPDATE SET
                    embed_requested = atom_pipeline_jobs.embed_requested OR EXCLUDED.embed_requested,
                    tag_requested = atom_pipeline_jobs.tag_requested OR EXCLUDED.tag_requested,
                    reason = EXCLUDED.reason,
                    not_before = LEAST(atom_pipeline_jobs.not_before, EXCLUDED.not_before),
                    state = 'pending',
                    lease_until = NULL,
                    atom_updated_at = EXCLUDED.atom_updated_at,
                    last_error = NULL,
                    updated_at = EXCLUDED.updated_at",
            )
            .bind(&self.db_id)
            .bind(&now)
            .execute(&self.pool)
            .await
        }
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to enqueue pipeline jobs from statuses: {}",
                e
            ))
        })?;

        Ok(result.rows_affected() as i32)
    }

    async fn claim_pipeline_jobs(
        &self,
        limit: i32,
        lease_until: &str,
        now: &str,
    ) -> StorageResult<Vec<AtomPipelineJob>> {
        let rows: Vec<(String, bool, bool, String, i32)> = sqlx::query_as(
            "WITH claimed AS (
                SELECT j.atom_id, j.db_id
                FROM atom_pipeline_jobs j
                INNER JOIN atoms a ON a.id = j.atom_id AND a.db_id = j.db_id
                WHERE j.db_id = $1
                  AND (
                    j.state = 'pending'
                    OR (j.state = 'processing' AND j.lease_until IS NOT NULL AND j.lease_until <= $3)
                  )
                  AND j.not_before <= $3
                  AND (
                    j.embed_requested
                    OR (j.tag_requested AND a.embedding_status = 'complete')
                  )
                ORDER BY j.updated_at ASC
                LIMIT $2
                FOR UPDATE SKIP LOCKED
             )
             UPDATE atom_pipeline_jobs j
             SET state = 'processing',
                 lease_until = $4,
                 attempts = attempts + 1,
                 updated_at = $3
             FROM claimed c
             WHERE j.atom_id = c.atom_id AND j.db_id = c.db_id
             RETURNING j.atom_id, j.embed_requested, j.tag_requested, j.atom_updated_at, j.attempts",
        )
        .bind(&self.db_id)
        .bind(limit as i64)
        .bind(now)
        .bind(lease_until)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to claim pipeline jobs: {}", e))
        })?;

        Ok(rows
            .into_iter()
            .map(
                |(atom_id, embed_requested, tag_requested, atom_updated_at, attempts)| {
                    AtomPipelineJob {
                        atom_id,
                        embed_requested,
                        tag_requested,
                        atom_updated_at,
                        attempts,
                    }
                },
            )
            .collect())
    }

    async fn clear_pipeline_jobs(&self, jobs: &[AtomPipelineJob]) -> StorageResult<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to begin transaction: {}", e))
        })?;
        for job in jobs {
            sqlx::query(
                "DELETE FROM atom_pipeline_jobs
                 WHERE atom_id = $1
                   AND db_id = $2
                   AND atom_updated_at = $3
                   AND attempts = $4
                   AND state = 'processing'",
            )
            .bind(&job.atom_id)
            .bind(&self.db_id)
            .bind(&job.atom_updated_at)
            .bind(job.attempts)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to clear pipeline job: {}", e))
            })?;
        }
        tx.commit().await.map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to commit transaction: {}", e))
        })?;
        Ok(())
    }

    async fn count_pipeline_jobs(&self) -> StorageResult<i32> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM atom_pipeline_jobs WHERE db_id = $1")
                .bind(&self.db_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| {
                    AtomicCoreError::DatabaseOperation(format!(
                        "Failed to count pipeline jobs: {}",
                        e
                    ))
                })?;
        Ok(count as i32)
    }
}

// ==================== Pipeline status ====================

impl PostgresStorage {
    /// Embedding/tagging snapshot for the active database. Mirrors the SQLite
    /// implementation in `sqlite/chunks.rs:get_pipeline_status_sync` so the
    /// `/api/pipeline-status` endpoint returns the same shape on either backend.
    pub(crate) async fn get_pipeline_status_impl(&self) -> StorageResult<PipelineStatus> {
        let count_by_status = |status: &'static str| async move {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM atoms WHERE embedding_status = $1 AND db_id = $2",
            )
            .bind(status)
            .bind(&self.db_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok::<i32, AtomicCoreError>(count as i32)
        };

        let pending = count_by_status("pending").await?;
        let processing = count_by_status("processing").await?;
        let complete = count_by_status("complete").await?;
        let failed_count = count_by_status("failed").await?;

        let failed: Vec<FailedAtom> =
            sqlx::query_as::<_, (String, String, String, Option<String>, String)>(
                "SELECT id, title, snippet, embedding_error, updated_at
             FROM atoms
             WHERE embedding_status = 'failed' AND db_id = $1
             ORDER BY updated_at DESC
             LIMIT 100",
            )
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            .into_iter()
            .map(|(atom_id, title, snippet, error, updated_at)| FailedAtom {
                atom_id,
                title,
                snippet,
                error,
                updated_at,
            })
            .collect();

        let queued_embedding: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM atom_pipeline_jobs WHERE embed_requested AND db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let queued_tagging: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM atom_pipeline_jobs WHERE tag_requested AND db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let count_tagging_by_status = |status: &'static str| async move {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM atoms WHERE tagging_status = $1 AND db_id = $2",
            )
            .bind(status)
            .bind(&self.db_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok::<i32, AtomicCoreError>(count as i32)
        };

        let tagging_pending = count_tagging_by_status("pending").await?;
        let tagging_processing = count_tagging_by_status("processing").await?;
        let tagging_complete = count_tagging_by_status("complete").await?;
        let tagging_skipped = count_tagging_by_status("skipped").await?;
        let tagging_failed_count = count_tagging_by_status("failed").await?;

        let tagging_failed: Vec<FailedAtom> =
            sqlx::query_as::<_, (String, String, String, Option<String>, String)>(
                "SELECT id, title, snippet, tagging_error, updated_at
             FROM atoms
             WHERE tagging_status = 'failed' AND db_id = $1
             ORDER BY updated_at DESC
             LIMIT 100",
            )
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            .into_iter()
            .map(|(atom_id, title, snippet, error, updated_at)| FailedAtom {
                atom_id,
                title,
                snippet,
                error,
                updated_at,
            })
            .collect();

        Ok(PipelineStatus {
            pending,
            processing,
            complete,
            failed_count,
            failed,
            queued_embedding: queued_embedding as i32,
            queued_tagging: queued_tagging as i32,
            tagging_pending,
            tagging_processing,
            tagging_complete,
            tagging_skipped,
            tagging_failed_count,
            tagging_failed,
        })
    }
}

// ==================== Private Helpers ====================

impl PostgresStorage {
    /// Compute semantic edges for a single atom using pgvector similarity search (internal impl).
    async fn compute_semantic_edges_for_atom_impl(
        &self,
        atom_id: &str,
        threshold: f32,
        max_edges: i32,
    ) -> Result<i32, AtomicCoreError> {
        // Delete existing edges for this atom (bidirectional)
        sqlx::query(
            "DELETE FROM semantic_edges WHERE (source_atom_id = $1 OR target_atom_id = $1) AND db_id = $2",
        )
        .bind(atom_id)
        .bind(&self.db_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!(
                "Failed to delete existing edges: {}",
                e
            ))
        })?;

        // Get all chunks with embeddings for the source atom
        let source_chunks: Vec<(String, i32, Vector)> = sqlx::query_as(
            "SELECT id, chunk_index, embedding FROM atom_chunks
             WHERE atom_id = $1 AND embedding IS NOT NULL AND db_id = $2",
        )
        .bind(atom_id)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("Failed to get source chunks: {}", e))
        })?;

        if source_chunks.is_empty() {
            return Ok(0);
        }

        // For each source chunk, find similar chunks from other atoms
        let mut atom_similarities: HashMap<String, (f32, i32, i32)> = HashMap::new();

        for (_chunk_id, source_chunk_index, embedding) in &source_chunks {
            let similar: Vec<(String, i32, f64)> = sqlx::query_as(
                "SELECT ac.atom_id, ac.chunk_index,
                        (ac.embedding <=> $1::vector) AS distance
                 FROM atom_chunks ac
                 WHERE ac.embedding IS NOT NULL AND ac.atom_id != $2 AND ac.db_id = $4
                 ORDER BY ac.embedding <=> $1::vector
                 LIMIT $3",
            )
            .bind(embedding)
            .bind(atom_id)
            .bind(max_edges * 5)
            .bind(&self.db_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Failed to find similar chunks: {}", e))
            })?;

            for (target_atom_id, target_chunk_index, distance) in similar {
                let similarity = 1.0 - distance as f32;
                if similarity < threshold {
                    continue;
                }

                let entry = atom_similarities.entry(target_atom_id);
                match entry {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if similarity > e.get().0 {
                            e.insert((similarity, *source_chunk_index, target_chunk_index));
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert((similarity, *source_chunk_index, target_chunk_index));
                    }
                }
            }
        }

        // Sort by similarity and take top N
        let mut edges: Vec<(String, f32, i32, i32)> = atom_similarities
            .into_iter()
            .map(|(target_id, (sim, src_idx, tgt_idx))| (target_id, sim, src_idx, tgt_idx))
            .collect();
        edges.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        edges.truncate(max_edges as usize);

        // Insert edges with consistent ordering (smaller ID is source)
        let now = chrono::Utc::now().to_rfc3339();
        let mut edges_created = 0;

        for (target_atom_id, similarity, source_chunk_index, target_chunk_index) in edges {
            let (src_id, tgt_id, src_chunk, tgt_chunk) = if atom_id < target_atom_id.as_str() {
                (
                    atom_id.to_string(),
                    target_atom_id.clone(),
                    source_chunk_index,
                    target_chunk_index,
                )
            } else {
                (
                    target_atom_id.clone(),
                    atom_id.to_string(),
                    target_chunk_index,
                    source_chunk_index,
                )
            };

            let edge_id = Uuid::new_v4().to_string();

            let result = sqlx::query(
                "INSERT INTO semantic_edges
                 (id, source_atom_id, target_atom_id, similarity_score, source_chunk_index, target_chunk_index, created_at, db_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (id) DO UPDATE SET
                    similarity_score = EXCLUDED.similarity_score,
                    source_chunk_index = EXCLUDED.source_chunk_index,
                    target_chunk_index = EXCLUDED.target_chunk_index,
                    created_at = EXCLUDED.created_at",
            )
            .bind(&edge_id)
            .bind(&src_id)
            .bind(&tgt_id)
            .bind(similarity)
            .bind(src_chunk)
            .bind(tgt_chunk)
            .bind(&now)
            .bind(&self.db_id)
            .execute(&self.pool)
            .await;

            if result.is_ok() {
                edges_created += 1;
            }
        }

        Ok(edges_created)
    }
}
