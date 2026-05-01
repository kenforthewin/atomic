use std::collections::HashMap;

use super::SqliteStorage;
use crate::embedding;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;
use uuid::Uuid;

impl SqliteStorage {
    pub(crate) fn get_pending_embeddings_sync(
        &self,
        limit: i32,
    ) -> StorageResult<Vec<(String, String)>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn
            .prepare("SELECT id, content FROM atoms WHERE embedding_status = 'pending' LIMIT ?1")?;
        let results = stmt
            .query_map([limit], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    pub(crate) fn set_embedding_status_sync(
        &self,
        atom_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "UPDATE atoms SET embedding_status = ?2, embedding_error = ?3 WHERE id = ?1",
            rusqlite::params![atom_id, status, error],
        )?;
        Ok(())
    }

    /// Set embedding status for multiple atoms in a single statement.
    pub(crate) fn set_embedding_status_batch_sync(
        &self,
        atom_ids: &[String],
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()> {
        if atom_ids.is_empty() {
            return Ok(());
        }
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let placeholders = atom_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 3))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE atoms SET embedding_status = ?1, embedding_error = ?2 WHERE id IN ({})",
            placeholders
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
            Vec::with_capacity(2 + atom_ids.len());
        params.push(Box::new(status.to_string()));
        params.push(Box::new(error.map(|e| e.to_string())));
        for id in atom_ids {
            params.push(Box::new(id.clone()));
        }
        conn.execute(
            &sql,
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        )?;
        Ok(())
    }

    pub(crate) fn set_tagging_status_sync(
        &self,
        atom_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "UPDATE atoms SET tagging_status = ?2, tagging_error = ?3 WHERE id = ?1",
            rusqlite::params![atom_id, status, error],
        )?;
        Ok(())
    }

    pub(crate) fn save_chunks_and_embeddings_sync(
        &self,
        atom_id: &str,
        chunks: &[(String, Vec<f32>)],
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        Self::save_chunks_for_atom(&conn, atom_id, chunks)
    }

    pub(crate) fn get_chunks_for_atoms_sync(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<ExistingAtomChunk>> {
        if atom_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.db.read_conn()?;
        let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, atom_id, chunk_index, content
             FROM atom_chunks
             WHERE atom_id IN ({})
             ORDER BY atom_id, chunk_index",
            placeholders
        );
        let mut stmt = conn.prepare(&sql)?;
        let chunks = stmt
            .query_map(rusqlite::params_from_iter(atom_ids.iter()), |row| {
                Ok(ExistingAtomChunk {
                    id: row.get(0)?,
                    atom_id: row.get(1)?,
                    chunk_index: row.get(2)?,
                    content: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(chunks)
    }

    pub(crate) fn update_chunk_embeddings_sync(
        &self,
        chunks: &[(String, Vec<f32>)],
    ) -> StorageResult<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        for (chunk_id, embedding_vec) in chunks {
            let embedding_blob = embedding::f32_vec_to_blob_public(embedding_vec);
            tx.execute(
                "UPDATE atom_chunks SET embedding = ?1 WHERE id = ?2",
                rusqlite::params![&embedding_blob, chunk_id],
            )?;
            tx.execute("DELETE FROM vec_chunks WHERE chunk_id = ?1", [chunk_id])?;
            tx.execute(
                "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![chunk_id, &embedding_blob],
            )?;
        }

        tx.commit()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    /// Save chunks and embeddings for multiple atoms in a single transaction.
    /// Each atom is wrapped in a SAVEPOINT so a mid-atom failure rolls back
    /// only that atom's partial state (DELETEs + INSERTs), not the whole batch.
    pub(crate) fn save_chunks_and_embeddings_batch_sync(
        &self,
        atoms: &[(String, Vec<(String, Vec<f32>)>)],
    ) -> StorageResult<Vec<String>> {
        if atoms.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut tx = conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut succeeded = Vec::new();
        for (atom_id, chunks) in atoms {
            // SAVEPOINT per atom: if save_chunks_for_atom fails mid-way
            // (after DELETE but during INSERTs), the SAVEPOINT rollback
            // restores the atom's prior chunk/FTS state instead of
            // committing a partial write.
            let sp = tx
                .savepoint()
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            match Self::save_chunks_for_atom(&sp, atom_id, chunks) {
                Ok(()) => {
                    sp.commit()
                        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
                    succeeded.push(atom_id.clone());
                }
                Err(e) => {
                    // sp is dropped without commit → implicit rollback to savepoint
                    tracing::warn!(atom_id = %atom_id, error = %e, "Failed to save chunks for atom, rolled back");
                }
            }
        }

        tx.commit()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(succeeded)
    }

    /// Inner helper: save chunks for a single atom on an existing connection/transaction.
    fn save_chunks_for_atom(
        conn: &rusqlite::Connection,
        atom_id: &str,
        chunks: &[(String, Vec<f32>)],
    ) -> StorageResult<()> {
        // Remove old FTS entries before deleting chunks
        conn.execute(
            "INSERT INTO atom_chunks_fts(atom_chunks_fts, rowid, id, atom_id, chunk_index, content)
             SELECT 'delete', rowid, id, atom_id, chunk_index, content FROM atom_chunks WHERE atom_id = ?1",
            [atom_id],
        )
        .ok();

        // Delete existing vec_chunks
        conn.execute(
            "DELETE FROM vec_chunks WHERE chunk_id IN (SELECT id FROM atom_chunks WHERE atom_id = ?1)",
            [atom_id],
        )
        .ok();

        // Delete existing atom_chunks
        conn.execute("DELETE FROM atom_chunks WHERE atom_id = ?1", [atom_id])?;

        // Insert new chunks and embeddings
        for (index, (chunk_content, embedding_vec)) in chunks.iter().enumerate() {
            let chunk_id = Uuid::new_v4().to_string();
            let hash = crate::boilerplate::content_hash(chunk_content);
            let embedding_blob = if embedding_vec.is_empty() {
                None::<Vec<u8>>
            } else {
                Some(embedding::f32_vec_to_blob_public(embedding_vec))
            };

            conn.execute(
                "INSERT INTO atom_chunks (id, atom_id, chunk_index, content, content_hash, embedding) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![&chunk_id, atom_id, index as i32, chunk_content, &hash, &embedding_blob],
            )?;

            if let Some(ref blob) = embedding_blob {
                conn.execute(
                    "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
                    rusqlite::params![&chunk_id, blob],
                )?;
            }
        }

        // Incrementally update FTS index
        conn.execute(
            "INSERT INTO atom_chunks_fts(rowid, id, atom_id, chunk_index, content)
             SELECT rowid, id, atom_id, chunk_index, content FROM atom_chunks WHERE atom_id = ?1",
            [atom_id],
        )
        .ok();

        Ok(())
    }

    pub(crate) fn delete_chunks_sync(&self, atom_id: &str) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        // Remove FTS entries
        conn.execute(
            "INSERT INTO atom_chunks_fts(atom_chunks_fts, rowid, id, atom_id, chunk_index, content)
             SELECT 'delete', rowid, id, atom_id, chunk_index, content FROM atom_chunks WHERE atom_id = ?1",
            [atom_id],
        )
        .ok();

        conn.execute(
            "DELETE FROM vec_chunks WHERE chunk_id IN (SELECT id FROM atom_chunks WHERE atom_id = ?1)",
            [atom_id],
        )
        .ok();

        conn.execute("DELETE FROM atom_chunks WHERE atom_id = ?1", [atom_id])?;
        Ok(())
    }

    pub(crate) fn reset_stuck_processing_sync(&self) -> StorageResult<i32> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let embedding_count = conn.execute(
            "UPDATE atoms SET embedding_status = 'pending' WHERE embedding_status = 'processing'",
            [],
        )?;

        let tagging_count = conn.execute(
            "UPDATE atoms SET tagging_status = 'pending' WHERE tagging_status = 'processing'",
            [],
        )?;

        // Reset edges stuck in 'processing' back to 'pending'
        let edges_count = conn.execute(
            "UPDATE atoms SET edges_status = 'pending' WHERE edges_status = 'processing'",
            [],
        )?;

        Ok((embedding_count + tagging_count + edges_count) as i32)
    }

    /// Reset failed embedding and tagging atoms back to pending (for auto-retry on config fix).
    pub(crate) fn reset_failed_embeddings_sync(&self) -> StorageResult<i32> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let embedding_count = conn.execute(
            "UPDATE atoms SET embedding_status = 'pending', embedding_error = NULL WHERE embedding_status = 'failed'",
            [],
        )?;

        let tagging_count = conn.execute(
            "UPDATE atoms SET tagging_status = 'pending', tagging_error = NULL WHERE tagging_status = 'failed'",
            [],
        )?;

        Ok((embedding_count + tagging_count) as i32)
    }

    pub(crate) fn reset_failed_embedding_statuses_sync(&self) -> StorageResult<i32> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let count = conn.execute(
            "UPDATE atoms
             SET embedding_status = 'pending', embedding_error = NULL
             WHERE embedding_status = 'failed'",
            [],
        )?;

        Ok(count as i32)
    }

    pub(crate) fn reset_failed_tagging_statuses_sync(&self) -> StorageResult<i32> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let count = conn.execute(
            "UPDATE atoms
             SET tagging_status = 'pending', tagging_error = NULL
             WHERE tagging_status = 'failed'
               AND embedding_status = 'complete'",
            [],
        )?;

        Ok(count as i32)
    }

    pub(crate) fn rebuild_semantic_edges_sync(&self) -> StorageResult<i32> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        // Clear all existing edges and mark all embedded atoms as needing edge recomputation.
        // The facade (`AtomicCore::rebuild_semantic_edges`) kicks off
        // `process_pending_edges` afterwards so it can supply the canvas cache
        // handle — storage has no knowledge of the cache.
        conn.execute("DELETE FROM semantic_edges", [])?;
        let count = conn.execute(
            "UPDATE atoms SET edges_status = 'pending' WHERE embedding_status = 'complete'",
            [],
        )? as i32;

        if count > 0 {
            tracing::info!(count, "Marked atoms for edge recomputation");
        }

        Ok(count)
    }

    /// Raw edge triples (source, target, score) sorted by score DESC.
    /// Lightweight — no full SemanticEdge struct, no chunk indexes, no timestamps.
    pub(crate) fn get_semantic_edges_raw_sync(
        &self,
        min_similarity: f32,
    ) -> StorageResult<Vec<(String, String, f32)>> {
        let conn = self.db.read_conn()?;
        let mut stmt = conn.prepare(
            "SELECT source_atom_id, target_atom_id, similarity_score
             FROM semantic_edges
             WHERE similarity_score >= ?1
             ORDER BY similarity_score DESC",
        )?;
        let edges = stmt
            .query_map([min_similarity], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(edges)
    }

    pub(crate) fn get_semantic_edges_sync(
        &self,
        min_similarity: f32,
    ) -> StorageResult<Vec<SemanticEdge>> {
        let conn = self.db.read_conn()?;

        let mut stmt = conn.prepare(
            "SELECT id, source_atom_id, target_atom_id, similarity_score,
                    source_chunk_index, target_chunk_index, created_at
             FROM semantic_edges
             WHERE similarity_score >= ?1
             ORDER BY similarity_score DESC
             LIMIT 10000",
        )?;

        let edges = stmt
            .query_map([min_similarity], |row| {
                Ok(SemanticEdge {
                    id: row.get(0)?,
                    source_atom_id: row.get(1)?,
                    target_atom_id: row.get(2)?,
                    similarity_score: row.get(3)?,
                    source_chunk_index: row.get(4)?,
                    target_chunk_index: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(edges)
    }

    pub(crate) fn get_atom_neighborhood_sync(
        &self,
        atom_id: &str,
        depth: i32,
        min_similarity: f32,
    ) -> StorageResult<NeighborhoodGraph> {
        let conn = self.db.read_conn()?;
        crate::build_neighborhood_graph(&conn, atom_id, depth, min_similarity)
    }

    pub(crate) fn get_connection_counts_sync(
        &self,
        min_similarity: f32,
    ) -> StorageResult<HashMap<String, i32>> {
        let conn = self.db.read_conn()?;
        crate::clustering::get_connection_counts(&conn, min_similarity)
            .map_err(|e| AtomicCoreError::Clustering(e))
    }

    pub(crate) fn save_tag_centroid_sync(
        &self,
        tag_id: &str,
        embedding_vec: &[f32],
    ) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let embedding_blob = embedding::f32_vec_to_blob_public(embedding_vec);
        let now = chrono::Utc::now().to_rfc3339();

        conn.execute(
            "INSERT OR REPLACE INTO tag_embeddings (tag_id, embedding, atom_count, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![tag_id, &embedding_blob, 0, &now],
        )?;

        // vec0 doesn't support REPLACE, so delete + insert
        conn.execute("DELETE FROM vec_tags WHERE tag_id = ?1", [tag_id])
            .ok();
        conn.execute(
            "INSERT INTO vec_tags (tag_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![tag_id, &embedding_blob],
        )?;

        Ok(())
    }

    pub(crate) fn recompute_all_tag_embeddings_sync(&self) -> StorageResult<i32> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        // Get all tags that have at least one atom with embeddings
        let mut stmt = conn.prepare(
            "SELECT DISTINCT at.tag_id
             FROM atom_tags at
             INNER JOIN atom_chunks ac ON at.atom_id = ac.atom_id
             WHERE ac.embedding IS NOT NULL",
        )?;

        let tag_ids: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        let count = tag_ids.len() as i32;
        tracing::info!(count, "Recomputing centroid embeddings for tags");

        embedding::compute_tag_embeddings_batch(&conn, &tag_ids)
            .map_err(|e| AtomicCoreError::Embedding(e))?;

        tracing::info!(count, "Tag centroid embeddings recomputed");
        Ok(count)
    }

    pub(crate) fn claim_pending_embeddings_sync(
        &self,
        limit: i32,
    ) -> StorageResult<Vec<(String, String)>> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "UPDATE atoms SET embedding_status = 'processing'
             WHERE id IN (SELECT id FROM atoms WHERE embedding_status = 'pending' LIMIT ?1)
             RETURNING id, content",
        )?;
        let results = stmt
            .query_map([limit], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    pub(crate) fn claim_pending_embeddings_due_sync(
        &self,
        limit: i32,
        max_updated_at: &str,
    ) -> StorageResult<Vec<(String, String)>> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "UPDATE atoms SET embedding_status = 'processing'
             WHERE id IN (
                 SELECT id FROM atoms
                 WHERE embedding_status = 'pending'
                   AND updated_at <= ?2
                 LIMIT ?1
             )
             RETURNING id, content",
        )?;
        let results = stmt
            .query_map((limit, max_updated_at), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    /// Atomically claim atoms that need edge computation: sets edges_status to 'processing'
    /// and returns their IDs.
    pub(crate) fn claim_pending_edges_sync(&self, limit: i32) -> StorageResult<Vec<String>> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "UPDATE atoms SET edges_status = 'processing'
             WHERE id IN (SELECT id FROM atoms WHERE edges_status = 'pending' AND embedding_status = 'complete' LIMIT ?1)
             RETURNING id",
        )?;
        let results = stmt
            .query_map([limit], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    /// Mark edges_status for a batch of atoms.
    pub(crate) fn set_edges_status_batch_sync(
        &self,
        atom_ids: &[String],
        status: &str,
    ) -> StorageResult<()> {
        if atom_ids.is_empty() {
            return Ok(());
        }
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE atoms SET edges_status = ?1 WHERE id IN ({})",
            placeholders
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
            Vec::with_capacity(1 + atom_ids.len());
        params.push(Box::new(status.to_string()));
        for id in atom_ids {
            params.push(Box::new(id.clone()));
        }
        conn.execute(
            &sql,
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        )?;
        Ok(())
    }

    /// Count atoms with pending edge computation.
    pub(crate) fn count_pending_edges_sync(&self) -> StorageResult<i32> {
        let conn = self.db.read_conn()?;
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE edges_status = 'pending' AND embedding_status = 'complete'",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub(crate) fn delete_chunks_batch_sync(&self, atom_ids: &[String]) -> StorageResult<()> {
        if atom_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let tx = conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        for atom_id in atom_ids {
            // Remove FTS entries
            tx.execute(
                "INSERT INTO atom_chunks_fts(atom_chunks_fts, rowid, id, atom_id, chunk_index, content)
                 SELECT 'delete', rowid, id, atom_id, chunk_index, content FROM atom_chunks WHERE atom_id = ?1",
                [atom_id],
            )
            .ok();

            tx.execute(
                "DELETE FROM vec_chunks WHERE chunk_id IN (SELECT id FROM atom_chunks WHERE atom_id = ?1)",
                [atom_id],
            )
            .ok();

            tx.execute("DELETE FROM atom_chunks WHERE atom_id = ?1", [atom_id])?;
        }

        tx.commit()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    pub(crate) fn compute_semantic_edges_for_atom_sync(
        &self,
        atom_id: &str,
        threshold: f32,
        max_edges: i32,
    ) -> StorageResult<i32> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        embedding::compute_semantic_edges_for_atom(&conn, atom_id, threshold, max_edges)
            .map_err(|e| AtomicCoreError::Embedding(e))
    }

    /// Compute semantic edges for a batch of atoms, processing in sub-batches
    /// of EDGE_SUB_BATCH atoms each. Each sub-batch acquires the write lock,
    /// runs a transaction, and releases — keeping mutex hold times short so
    /// concurrent writes (e.g. UI atom saves) aren't stalled.
    pub(crate) fn compute_semantic_edges_batch_sync(
        &self,
        atom_ids: &[String],
        threshold: f32,
        max_edges: i32,
    ) -> StorageResult<i32> {
        const EDGE_SUB_BATCH: usize = 50;
        let mut total_edges = 0;
        for chunk in atom_ids.chunks(EDGE_SUB_BATCH) {
            let mut conn = self
                .db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            let tx = conn
                .transaction()
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            for atom_id in chunk {
                match embedding::compute_semantic_edges_for_atom(&tx, atom_id, threshold, max_edges)
                {
                    Ok(count) => total_edges += count,
                    Err(e) => {
                        tracing::warn!(atom_id = %atom_id, error = %e, "Failed to compute edges for atom");
                    }
                }
            }
            tx.commit()
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            // conn lock dropped here — other writers can proceed between sub-batches
        }
        Ok(total_edges)
    }

    pub(crate) fn rebuild_fts_index_sync(&self) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "INSERT INTO atom_chunks_fts(atom_chunks_fts) VALUES('rebuild')",
            [],
        )?;
        conn.execute("DELETE FROM wiki_articles_fts", [])?;
        conn.execute(
            "INSERT INTO wiki_articles_fts(id, tag_id, tag_name, content)
             SELECT w.id, w.tag_id, t.name, w.content
             FROM wiki_articles w
             JOIN tags t ON t.id = w.tag_id",
            [],
        )?;
        conn.execute("DELETE FROM chat_messages_fts", [])?;
        conn.execute(
            "INSERT INTO chat_messages_fts(id, conversation_id, content)
             SELECT id, conversation_id, content FROM chat_messages",
            [],
        )?;
        Ok(())
    }

    pub(crate) fn check_vector_extension_sync(&self) -> StorageResult<String> {
        let conn = self.db.read_conn()?;
        let version: String = conn.query_row("SELECT vec_version()", [], |row| row.get(0))?;
        Ok(version)
    }

    pub(crate) fn claim_pending_tagging_sync(&self) -> StorageResult<Vec<String>> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "UPDATE atoms SET tagging_status = 'processing'
             WHERE embedding_status = 'complete'
             AND tagging_status = 'pending'
             RETURNING id",
        )?;
        let results = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    pub(crate) fn claim_pending_tagging_due_sync(
        &self,
        max_updated_at: &str,
    ) -> StorageResult<Vec<String>> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "UPDATE atoms SET tagging_status = 'processing'
             WHERE embedding_status = 'complete'
               AND tagging_status = 'pending'
               AND updated_at <= ?1
             RETURNING id",
        )?;
        let results = stmt
            .query_map([max_updated_at], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    pub(crate) fn enqueue_pipeline_jobs_sync(
        &self,
        jobs: &[AtomPipelineJobRequest],
    ) -> StorageResult<i32> {
        if jobs.is_empty() {
            return Ok(0);
        }
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut count = 0i32;

        for job in jobs {
            if !job.embed_requested && !job.tag_requested {
                continue;
            }
            let not_before = job.not_before.as_deref().unwrap_or(&now);
            let changed = tx.execute(
                "INSERT INTO atom_pipeline_jobs (
                    atom_id, embed_requested, tag_requested, reason, not_before,
                    state, lease_until, attempts, atom_updated_at, last_error,
                    created_at, updated_at
                 )
                 SELECT id, ?2, ?3, ?4, ?5, 'pending', NULL, 0, updated_at, NULL, ?6, ?6
                 FROM atoms
                 WHERE id = ?1
                 ON CONFLICT(atom_id) DO UPDATE SET
                    embed_requested = MAX(atom_pipeline_jobs.embed_requested, excluded.embed_requested),
                    tag_requested = MAX(atom_pipeline_jobs.tag_requested, excluded.tag_requested),
                    reason = excluded.reason,
                    not_before = MIN(atom_pipeline_jobs.not_before, excluded.not_before),
                    state = 'pending',
                    lease_until = NULL,
                    atom_updated_at = excluded.atom_updated_at,
                    last_error = NULL,
                    updated_at = excluded.updated_at",
                rusqlite::params![
                    &job.atom_id,
                    if job.embed_requested { 1 } else { 0 },
                    if job.tag_requested { 1 } else { 0 },
                    &job.reason,
                    not_before,
                    &now,
                ],
            )?;
            count += changed as i32;
        }

        tx.commit()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(count)
    }

    pub(crate) fn enqueue_pipeline_jobs_from_statuses_sync(
        &self,
        max_updated_at: Option<&str>,
    ) -> StorageResult<i32> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();

        let count = conn.execute(
            "INSERT INTO atom_pipeline_jobs (
                atom_id, embed_requested, tag_requested, reason, not_before,
                state, lease_until, attempts, atom_updated_at, last_error,
                created_at, updated_at
             )
             SELECT id,
                    CASE WHEN embedding_status = 'pending' THEN 1 ELSE 0 END,
                    CASE WHEN tagging_status = 'pending' THEN 1 ELSE 0 END,
                    'status-backfill',
                    ?1,
                    'pending',
                    NULL,
                    0,
                    updated_at,
                    NULL,
                    ?1,
                    ?1
             FROM atoms
             WHERE (?2 IS NULL OR updated_at <= ?2)
               AND (
                 embedding_status = 'pending'
                 OR (embedding_status = 'complete' AND tagging_status = 'pending')
               )
             ON CONFLICT(atom_id) DO UPDATE SET
                embed_requested = MAX(atom_pipeline_jobs.embed_requested, excluded.embed_requested),
                tag_requested = MAX(atom_pipeline_jobs.tag_requested, excluded.tag_requested),
                reason = excluded.reason,
                not_before = MIN(atom_pipeline_jobs.not_before, excluded.not_before),
                state = 'pending',
                lease_until = NULL,
                atom_updated_at = excluded.atom_updated_at,
                last_error = NULL,
                updated_at = excluded.updated_at
             WHERE atom_pipeline_jobs.state != 'processing'
                OR atom_pipeline_jobs.lease_until IS NULL
                OR atom_pipeline_jobs.lease_until <= excluded.updated_at
                OR atom_pipeline_jobs.atom_updated_at != excluded.atom_updated_at",
            rusqlite::params![&now, max_updated_at],
        )?;
        Ok(count as i32)
    }

    pub(crate) fn claim_pipeline_jobs_sync(
        &self,
        limit: i32,
        lease_until: &str,
        now: &str,
    ) -> StorageResult<Vec<AtomPipelineJob>> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "UPDATE atom_pipeline_jobs
             SET state = 'processing',
                 lease_until = ?1,
                 attempts = attempts + 1,
                 updated_at = ?2
             WHERE atom_id IN (
                 SELECT j.atom_id
                 FROM atom_pipeline_jobs j
                 INNER JOIN atoms a ON a.id = j.atom_id
                 WHERE (j.state = 'pending'
                        OR (j.state = 'processing' AND j.lease_until IS NOT NULL AND j.lease_until <= ?2))
                   AND j.not_before <= ?2
                   AND (j.embed_requested = 1
                        OR (j.tag_requested = 1 AND a.embedding_status = 'complete'))
                 ORDER BY j.updated_at ASC
                 LIMIT ?3
             )
             RETURNING atom_id, embed_requested, tag_requested, atom_updated_at, attempts",
        )?;
        let jobs = stmt
            .query_map((lease_until, now, limit), |row| {
                Ok(AtomPipelineJob {
                    atom_id: row.get(0)?,
                    embed_requested: row.get::<_, i32>(1)? != 0,
                    tag_requested: row.get::<_, i32>(2)? != 0,
                    atom_updated_at: row.get(3)?,
                    attempts: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(jobs)
    }

    pub(crate) fn clear_pipeline_jobs_sync(&self, jobs: &[AtomPipelineJob]) -> StorageResult<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        {
            let mut stmt = tx.prepare(
                "DELETE FROM atom_pipeline_jobs
                 WHERE atom_id = ?1
                   AND atom_updated_at = ?2
                   AND attempts = ?3
                   AND state = 'processing'",
            )?;
            for job in jobs {
                stmt.execute(rusqlite::params![
                    &job.atom_id,
                    &job.atom_updated_at,
                    job.attempts
                ])?;
            }
        }
        tx.commit()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    pub(crate) fn count_pipeline_jobs_sync(&self) -> StorageResult<i32> {
        let conn = self.db.read_conn()?;
        conn.query_row("SELECT COUNT(*) FROM atom_pipeline_jobs", [], |row| {
            row.get(0)
        })
        .map_err(AtomicCoreError::from)
    }

    pub(crate) fn get_embedding_dimension_sync(&self) -> StorageResult<Option<usize>> {
        let conn = self.db.read_conn()?;
        let dim = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='vec_chunks'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|sql| {
                let start = sql.find("float[")?;
                let after = &sql[start + 6..];
                let end = after.find(']')?;
                after[..end].parse::<usize>().ok()
            });
        Ok(dim)
    }

    pub(crate) fn recreate_vector_index_sync(&self, dimension: usize) -> StorageResult<()> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        crate::db::recreate_vec_chunks_with_dimension(&conn, dimension)
    }

    pub(crate) fn claim_pending_reembedding_sync(&self) -> StorageResult<Vec<String>> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "UPDATE atoms SET embedding_status = 'processing'
             WHERE embedding_status IN ('pending', 'processing')
             RETURNING id",
        )?;
        let results = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    pub(crate) fn claim_all_for_reembedding_sync(&self) -> StorageResult<Vec<String>> {
        let conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "UPDATE atoms SET embedding_status = 'processing'
             RETURNING id",
        )?;
        let results = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    pub(crate) fn get_pipeline_status_sync(&self) -> StorageResult<PipelineStatus> {
        let conn = self.db.read_conn()?;
        let pending: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE embedding_status = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let processing: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE embedding_status = 'processing'",
            [],
            |r| r.get(0),
        )?;
        let complete: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE embedding_status = 'complete'",
            [],
            |r| r.get(0),
        )?;
        let failed_count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE embedding_status = 'failed'",
            [],
            |r| r.get(0),
        )?;

        let mut stmt = conn.prepare(
            "SELECT id, title, snippet, embedding_error, updated_at FROM atoms WHERE embedding_status = 'failed' ORDER BY updated_at DESC LIMIT 100",
        )?;
        let failed: Vec<FailedAtom> = stmt
            .query_map([], |row| {
                Ok(FailedAtom {
                    atom_id: row.get(0)?,
                    title: row.get(1)?,
                    snippet: row.get(2)?,
                    error: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let queued_embedding: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atom_pipeline_jobs WHERE embed_requested = 1",
            [],
            |r| r.get(0),
        )?;
        let queued_tagging: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atom_pipeline_jobs WHERE tag_requested = 1",
            [],
            |r| r.get(0),
        )?;

        let tagging_failed_count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE tagging_status = 'failed'",
            [],
            |r| r.get(0),
        )?;
        let tagging_pending: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE tagging_status = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let tagging_processing: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE tagging_status = 'processing'",
            [],
            |r| r.get(0),
        )?;
        let tagging_complete: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE tagging_status = 'complete'",
            [],
            |r| r.get(0),
        )?;
        let tagging_skipped: i32 = conn.query_row(
            "SELECT COUNT(*) FROM atoms WHERE tagging_status = 'skipped'",
            [],
            |r| r.get(0),
        )?;

        let mut stmt = conn.prepare(
            "SELECT id, title, snippet, tagging_error, updated_at FROM atoms WHERE tagging_status = 'failed' ORDER BY updated_at DESC LIMIT 100",
        )?;
        let tagging_failed: Vec<FailedAtom> = stmt
            .query_map([], |row| {
                Ok(FailedAtom {
                    atom_id: row.get(0)?,
                    title: row.get(1)?,
                    snippet: row.get(2)?,
                    error: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PipelineStatus {
            pending,
            processing,
            complete,
            failed_count,
            failed,
            queued_embedding,
            queued_tagging,
            tagging_pending,
            tagging_processing,
            tagging_complete,
            tagging_skipped,
            tagging_failed_count,
            tagging_failed,
        })
    }
    /// Given a list of content_hash values, return map of hash → count of distinct
    /// atoms containing a chunk with that hash. Used for boilerplate detection.
    pub(crate) fn count_chunk_hash_occurrences_impl(
        &self,
        hashes: &[String],
    ) -> StorageResult<std::collections::HashMap<String, i64>> {
        if hashes.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let conn = self.db.read_conn()?;
        let placeholders = hashes.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT content_hash, COUNT(DISTINCT atom_id) as cnt
             FROM atom_chunks
             WHERE content_hash IN ({placeholders})
               AND content_hash IS NOT NULL
             GROUP BY content_hash"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut map = std::collections::HashMap::new();
        let rows = stmt.query_map(
            rusqlite::params_from_iter(hashes.iter()),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        for row in rows {
            let (hash, cnt) = row?;
            map.insert(hash, cnt);
        }
        Ok(map)
    }

    /// Delete vec_chunks entries for specific chunk IDs.
    /// Used after boilerplate detection to remove vectors for shared chunks.
    pub(crate) fn delete_vec_chunks_by_ids_impl(
        &self,
        chunk_ids: &[String],
    ) -> StorageResult<()> {
        if chunk_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let placeholders = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM vec_chunks WHERE chunk_id IN ({placeholders})");
        tx.execute(&sql, rusqlite::params_from_iter(chunk_ids.iter()))?;
        tx.commit().map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(())
    }

    /// Backfill content_hash for all atom_chunks rows that have content but NULL hash.
    /// Safe to run multiple times (idempotent). Returns number of rows updated.
    pub(crate) fn backfill_content_hashes_impl(&self) -> StorageResult<usize> {
        use crate::boilerplate::content_hash;
        let conn = self.db.read_conn()?;
        let ids_and_contents: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, content FROM atom_chunks WHERE content_hash IS NULL LIMIT 5000",
            )?;
            let x = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            x
        };
        drop(conn);
        if ids_and_contents.is_empty() {
            return Ok(0);
        }
        let mut write_conn = self
            .db
            .conn
            .lock()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let tx = write_conn
            .transaction()
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let count = ids_and_contents.len();
        for (id, content) in &ids_and_contents {
            let hash = content_hash(content);
            tx.execute(
                "UPDATE atom_chunks SET content_hash = ?1 WHERE id = ?2",
                rusqlite::params![hash, id],
            )?;
        }
        tx.commit().map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        Ok(count)
    }
}

#[async_trait]
impl ChunkStore for SqliteStorage {
    async fn get_pending_embeddings(&self, limit: i32) -> StorageResult<Vec<(String, String)>> {
        self.get_pending_embeddings_sync(limit)
    }

    async fn set_embedding_status(
        &self,
        atom_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()> {
        self.set_embedding_status_sync(atom_id, status, error)
    }

    async fn set_tagging_status(
        &self,
        atom_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()> {
        self.set_tagging_status_sync(atom_id, status, error)
    }

    async fn save_chunks_and_embeddings(
        &self,
        atom_id: &str,
        chunks: &[(String, Vec<f32>)],
    ) -> StorageResult<()> {
        self.save_chunks_and_embeddings_sync(atom_id, chunks)
    }

    async fn get_chunks_for_atoms(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<ExistingAtomChunk>> {
        self.get_chunks_for_atoms_sync(atom_ids)
    }

    async fn update_chunk_embeddings(&self, chunks: &[(String, Vec<f32>)]) -> StorageResult<()> {
        self.update_chunk_embeddings_sync(chunks)
    }

    async fn delete_chunks(&self, atom_id: &str) -> StorageResult<()> {
        self.delete_chunks_sync(atom_id)
    }

    async fn reset_stuck_processing(&self) -> StorageResult<i32> {
        self.reset_stuck_processing_sync()
    }

    async fn reset_failed_embeddings(&self) -> StorageResult<i32> {
        self.reset_failed_embeddings_sync()
    }

    async fn reset_failed_embedding_statuses(&self) -> StorageResult<i32> {
        self.reset_failed_embedding_statuses_sync()
    }

    async fn reset_failed_tagging_statuses(&self) -> StorageResult<i32> {
        self.reset_failed_tagging_statuses_sync()
    }

    async fn rebuild_semantic_edges(&self) -> StorageResult<i32> {
        self.rebuild_semantic_edges_sync()
    }

    async fn get_semantic_edges(&self, min_similarity: f32) -> StorageResult<Vec<SemanticEdge>> {
        self.get_semantic_edges_sync(min_similarity)
    }

    async fn get_semantic_edges_raw(
        &self,
        min_similarity: f32,
    ) -> StorageResult<Vec<(String, String, f32)>> {
        self.get_semantic_edges_raw_sync(min_similarity)
    }

    async fn get_atom_neighborhood(
        &self,
        atom_id: &str,
        depth: i32,
        min_similarity: f32,
    ) -> StorageResult<NeighborhoodGraph> {
        self.get_atom_neighborhood_sync(atom_id, depth, min_similarity)
    }

    async fn get_connection_counts(
        &self,
        min_similarity: f32,
    ) -> StorageResult<HashMap<String, i32>> {
        self.get_connection_counts_sync(min_similarity)
    }

    async fn save_tag_centroid(&self, tag_id: &str, embedding: &[f32]) -> StorageResult<()> {
        self.save_tag_centroid_sync(tag_id, embedding)
    }

    async fn recompute_all_tag_embeddings(&self) -> StorageResult<i32> {
        self.recompute_all_tag_embeddings_sync()
    }

    async fn check_vector_extension(&self) -> StorageResult<String> {
        self.check_vector_extension_sync()
    }

    async fn claim_pending_embeddings(&self, limit: i32) -> StorageResult<Vec<(String, String)>> {
        self.claim_pending_embeddings_sync(limit)
    }

    async fn claim_pending_embeddings_due(
        &self,
        limit: i32,
        max_updated_at: &str,
    ) -> StorageResult<Vec<(String, String)>> {
        self.claim_pending_embeddings_due_sync(limit, max_updated_at)
    }

    async fn delete_chunks_batch(&self, atom_ids: &[String]) -> StorageResult<()> {
        self.delete_chunks_batch_sync(atom_ids)
    }

    async fn compute_semantic_edges_for_atom(
        &self,
        atom_id: &str,
        threshold: f32,
        max_edges: i32,
    ) -> StorageResult<i32> {
        self.compute_semantic_edges_for_atom_sync(atom_id, threshold, max_edges)
    }

    async fn rebuild_fts_index(&self) -> StorageResult<()> {
        self.rebuild_fts_index_sync()
    }

    async fn claim_pending_tagging(&self) -> StorageResult<Vec<String>> {
        self.claim_pending_tagging_sync()
    }

    async fn claim_pending_tagging_due(&self, max_updated_at: &str) -> StorageResult<Vec<String>> {
        self.claim_pending_tagging_due_sync(max_updated_at)
    }

    async fn get_embedding_dimension(&self) -> StorageResult<Option<usize>> {
        self.get_embedding_dimension_sync()
    }

    async fn recreate_vector_index(&self, dimension: usize) -> StorageResult<()> {
        self.recreate_vector_index_sync(dimension)
    }

    async fn claim_pending_reembedding(&self) -> StorageResult<Vec<String>> {
        self.claim_pending_reembedding_sync()
    }

    async fn claim_all_for_reembedding(&self) -> StorageResult<Vec<String>> {
        self.claim_all_for_reembedding_sync()
    }

    async fn claim_pending_edges(&self, limit: i32) -> StorageResult<Vec<String>> {
        self.claim_pending_edges_sync(limit)
    }

    async fn set_edges_status_batch(&self, atom_ids: &[String], status: &str) -> StorageResult<()> {
        self.set_edges_status_batch_sync(atom_ids, status)
    }

    async fn count_pending_edges(&self) -> StorageResult<i32> {
        self.count_pending_edges_sync()
    }

    async fn enqueue_pipeline_jobs(&self, jobs: &[AtomPipelineJobRequest]) -> StorageResult<i32> {
        self.enqueue_pipeline_jobs_sync(jobs)
    }

    async fn enqueue_pipeline_jobs_from_statuses(
        &self,
        max_updated_at: Option<&str>,
    ) -> StorageResult<i32> {
        self.enqueue_pipeline_jobs_from_statuses_sync(max_updated_at)
    }

    async fn claim_pipeline_jobs(
        &self,
        limit: i32,
        lease_until: &str,
        now: &str,
    ) -> StorageResult<Vec<AtomPipelineJob>> {
        self.claim_pipeline_jobs_sync(limit, lease_until, now)
    }

    async fn clear_pipeline_jobs(&self, jobs: &[AtomPipelineJob]) -> StorageResult<()> {
        self.clear_pipeline_jobs_sync(jobs)
    }

    async fn count_pipeline_jobs(&self) -> StorageResult<i32> {
        self.count_pipeline_jobs_sync()
    }
}

#[cfg(test)]
mod tests {
    use crate::{AtomicCore, CreateAtomRequest};
    use tempfile::TempDir;

    async fn enqueue_and_claim_pipeline_job(
        core: &AtomicCore,
        atom_id: &str,
    ) -> Vec<crate::models::AtomPipelineJob> {
        let initial_job = crate::models::AtomPipelineJobRequest {
            atom_id: atom_id.to_string(),
            embed_requested: true,
            tag_requested: true,
            not_before: None,
            reason: "initial".to_string(),
        };
        core.storage()
            .enqueue_pipeline_jobs_sync(&[initial_job])
            .await
            .expect("enqueue initial job");

        core.storage()
            .claim_pipeline_jobs_sync(10, "2099-01-01T00:30:00+00:00", "2099-01-01T00:00:00+00:00")
            .await
            .expect("claim job")
    }

    #[tokio::test]
    async fn clear_pipeline_jobs_keeps_newer_pending_job_for_same_atom() {
        let dir = TempDir::new().expect("create tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("pipeline.db"))
            .expect("open sqlite test db");
        let created = core
            .create_atom(
                CreateAtomRequest {
                    content: String::new(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .expect("create atom")
            .expect("atom inserted");

        let claimed = enqueue_and_claim_pipeline_job(&core, &created.atom.id).await;
        assert_eq!(claimed.len(), 1);

        let newer_updated_at = "2026-01-01T00:00:01+00:00";
        let conn = rusqlite::Connection::open(core.db_path()).expect("open sqlite db");
        conn.execute(
            "UPDATE atoms SET updated_at = ?1, embedding_status = 'pending', tagging_status = 'pending' WHERE id = ?2",
            rusqlite::params![newer_updated_at, &created.atom.id],
        )
        .expect("advance atom timestamp");

        let newer_job = crate::models::AtomPipelineJobRequest {
            atom_id: created.atom.id.clone(),
            embed_requested: true,
            tag_requested: true,
            not_before: None,
            reason: "newer".to_string(),
        };
        core.storage()
            .enqueue_pipeline_jobs_sync(&[newer_job])
            .await
            .expect("enqueue newer job");

        core.storage()
            .clear_pipeline_jobs_sync(&claimed)
            .await
            .expect("clear original claim");

        let row: (String, String) = conn
            .query_row(
                "SELECT state, atom_updated_at FROM atom_pipeline_jobs WHERE atom_id = ?1",
                [&created.atom.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("newer pipeline job should remain");
        assert_eq!(row.0, "pending");
        assert_eq!(row.1, newer_updated_at);
    }

    #[tokio::test]
    async fn status_backfill_requeues_newer_revision_during_active_lease() {
        let dir = TempDir::new().expect("create tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("pipeline.db"))
            .expect("open sqlite test db");
        let created = core
            .create_atom(
                CreateAtomRequest {
                    content: String::new(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .expect("create atom")
            .expect("atom inserted");

        let claimed = enqueue_and_claim_pipeline_job(&core, &created.atom.id).await;
        assert_eq!(claimed.len(), 1);

        let newer_updated_at = "2099-01-01T00:00:01+00:00";
        let conn = rusqlite::Connection::open(core.db_path()).expect("open sqlite db");
        conn.execute(
            "UPDATE atoms SET updated_at = ?1, embedding_status = 'pending', tagging_status = 'pending' WHERE id = ?2",
            rusqlite::params![newer_updated_at, &created.atom.id],
        )
        .expect("advance atom timestamp");

        core.storage()
            .enqueue_pipeline_jobs_from_statuses_sync(None)
            .await
            .expect("backfill pending statuses");

        let row: (String, Option<String>, String) = conn
            .query_row(
                "SELECT state, lease_until, atom_updated_at FROM atom_pipeline_jobs WHERE atom_id = ?1",
                [&created.atom.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("newer pipeline job should remain");
        assert_eq!(row.0, "pending");
        assert_eq!(row.1, None);
        assert_eq!(row.2, newer_updated_at);
    }

    #[tokio::test]
    async fn status_backfill_preserves_active_lease_for_same_revision() {
        let dir = TempDir::new().expect("create tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("pipeline.db"))
            .expect("open sqlite test db");
        let created = core
            .create_atom(
                CreateAtomRequest {
                    content: String::new(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .expect("create atom")
            .expect("atom inserted");

        let claimed = enqueue_and_claim_pipeline_job(&core, &created.atom.id).await;
        assert_eq!(claimed.len(), 1);

        core.storage()
            .enqueue_pipeline_jobs_from_statuses_sync(None)
            .await
            .expect("backfill pending statuses");

        let conn = rusqlite::Connection::open(core.db_path()).expect("open sqlite db");
        let row: (String, Option<String>, String) = conn
            .query_row(
                "SELECT state, lease_until, atom_updated_at FROM atom_pipeline_jobs WHERE atom_id = ?1",
                [&created.atom.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("existing pipeline job should remain");
        assert_eq!(row.0, "processing");
        assert_eq!(row.1.as_deref(), Some("2099-01-01T00:30:00+00:00"));
        assert_eq!(row.2, created.atom.updated_at);
    }

    #[tokio::test]
    async fn draft_pending_status_is_not_counted_as_queued_pipeline_work() {
        let dir = TempDir::new().expect("create tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("pipeline.db"))
            .expect("open sqlite test db");
        let created = core
            .create_atom(
                CreateAtomRequest {
                    content: String::new(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .expect("create atom")
            .expect("atom inserted");

        core.update_atom_content_only(
            &created.atom.id,
            crate::UpdateAtomRequest {
                content: "draft content still being edited".to_string(),
                source_url: None,
                published_at: None,
                tag_ids: None,
            },
        )
        .await
        .expect("draft save");

        let status = core.get_pipeline_status().await.expect("pipeline status");
        assert_eq!(status.pending, 1);
        assert_eq!(status.tagging_pending, 1);
        assert_eq!(status.queued_embedding, 0);
        assert_eq!(status.queued_tagging, 0);
    }
}
