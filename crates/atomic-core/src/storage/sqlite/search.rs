use std::collections::HashMap;

use super::SqliteStorage;
use crate::embedding::{distance_to_similarity, f32_vec_to_blob_public};
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::search;
use crate::storage::traits::*;
use async_trait::async_trait;
use rusqlite::OptionalExtension;

/// Sync helper methods for search operations.
impl SqliteStorage {
    pub(crate) fn vector_search_sync(
        &self,
        query_embedding: &[f32],
        limit: i32,
        threshold: f32,
        tag_id: Option<&str>,
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<SemanticSearchResult>> {
        let query_blob = f32_vec_to_blob_public(query_embedding);
        let conn = self.db.read_conn()?;
        let fetch_limit = limit * 10;

        let similar_chunks: Vec<(String, f32)> =
            vec_knn_with_cutoff(&conn, &query_blob, fetch_limit, created_after)?;

        // Filter by threshold
        let filtered: Vec<(String, f32)> = similar_chunks
            .into_iter()
            .filter(|(_, distance)| distance_to_similarity(*distance) >= threshold)
            .collect();

        // Batch-load chunk details
        let chunk_ids: Vec<String> = filtered.iter().map(|(id, _)| id.clone()).collect();
        let chunk_map = batch_fetch_chunk_info(&conn, &chunk_ids)?;

        // Scope filtering
        let scope_tag_ids: Vec<String> = tag_id.map(|t| vec![t.to_string()]).unwrap_or_default();
        let scope_atom_ids: std::collections::HashSet<String> = if !scope_tag_ids.is_empty() {
            let candidate_atom_ids: Vec<&str> =
                chunk_map.values().map(|(aid, _, _)| aid.as_str()).collect();
            batch_atoms_with_scope_tags(&conn, &candidate_atom_ids, &scope_tag_ids)?
        } else {
            std::collections::HashSet::new()
        };
        // Kind filtering. `None` short-circuits `KindFilter::All`.
        let kind_allowed: Option<std::collections::HashSet<String>> =
            if matches!(kinds, crate::models::KindFilter::All) {
                None
            } else {
                let candidate_atom_ids: Vec<&str> =
                    chunk_map.values().map(|(aid, _, _)| aid.as_str()).collect();
                Some(batch_atoms_matching_kind(
                    &conn,
                    &candidate_atom_ids,
                    kinds,
                )?)
            };

        // Deduplicate by atom_id, keeping best score
        let mut atom_best: HashMap<String, (f32, String, i32)> = HashMap::new();
        for (chunk_id, distance) in &filtered {
            let similarity = distance_to_similarity(*distance);
            if let Some((atom_id, content, chunk_index)) = chunk_map.get(chunk_id) {
                if !scope_tag_ids.is_empty() && !scope_atom_ids.contains(atom_id) {
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
                        if similarity > e.get().0 {
                            e.insert((similarity, content.clone(), *chunk_index));
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert((similarity, content.clone(), *chunk_index));
                    }
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
        let atom_map = batch_fetch_atoms(&conn, &atom_ids)?;
        let tag_map = batch_fetch_tags(&conn, &atom_ids)?;

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

    pub(crate) fn keyword_search_sync(
        &self,
        query: &str,
        limit: i32,
        tag_id: Option<&str>,
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<SemanticSearchResult>> {
        let conn = self.db.read_conn()?;

        let escaped_query = escape_fts5_query(query);
        if escaped_query.is_empty() {
            return Ok(Vec::new());
        }
        let fetch_limit = limit * 2;

        // Query atom-level FTS. Each row is already one atom — no chunk dedupe
        // needed. `highlight` wraps every match in the full atom content so we
        // can parse out per-match byte offsets for the reader's cycle flow.
        let raw_results: Vec<(String, f64, String, String)> =
            atom_fts_search_with_cutoff(&conn, &escaped_query, fetch_limit, created_after)?;

        // Apply tag scope filter if specified
        let filtered = if let Some(tid) = tag_id {
            let scope_tag_ids = vec![tid.to_string()];
            let candidate_atom_ids: Vec<&str> = raw_results.iter().map(|r| r.0.as_str()).collect();
            let matching = batch_atoms_with_scope_tags(&conn, &candidate_atom_ids, &scope_tag_ids)?;
            raw_results
                .into_iter()
                .filter(|r| matching.contains(r.0.as_str()))
                .collect::<Vec<_>>()
        } else {
            raw_results
        };

        // Apply kind filter post-pass (FTS index is kind-agnostic).
        let filtered = if matches!(kinds, crate::models::KindFilter::All) {
            filtered
        } else {
            let candidate_atom_ids: Vec<&str> = filtered.iter().map(|r| r.0.as_str()).collect();
            let allowed = batch_atoms_matching_kind(&conn, &candidate_atom_ids, kinds)?;
            filtered
                .into_iter()
                .filter(|r| allowed.contains(&r.0))
                .collect()
        };

        // Sort by BM25 ascending (lower = better) then truncate
        let mut sorted = filtered;
        sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        sorted.truncate(limit as usize);

        // Batch fetch atom data
        let atom_ids: Vec<String> = sorted.iter().map(|(id, _, _, _)| id.clone()).collect();
        let atom_map = batch_fetch_atoms(&conn, &atom_ids)?;
        let tag_map = batch_fetch_tags(&conn, &atom_ids)?;

        let mut results = Vec::with_capacity(sorted.len());
        for (atom_id, bm25_score, snippet, highlighted) in sorted {
            if let Some(atom) = atom_map.get(&atom_id) {
                let tags = tag_map.get(&atom_id).cloned().unwrap_or_default();
                let mut offsets = parse_match_offsets(&highlighted);
                let total = offsets.len() as u32;
                offsets.truncate(MAX_MATCH_OFFSETS_PER_RESULT);
                results.push(SemanticSearchResult {
                    atom: AtomWithTags {
                        atom: atom.clone(),
                        tags,
                    },
                    similarity_score: normalize_bm25_score(bm25_score),
                    // Chunk fields aren't meaningful for atom-level search.
                    matching_chunk_content: String::new(),
                    matching_chunk_index: 0,
                    match_snippet: Some(snippet),
                    match_offsets: Some(offsets),
                    match_count: Some(total),
                });
            }
        }

        Ok(results)
    }

    pub(crate) fn keyword_search_chunks_sync(
        &self,
        query: &str,
        limit: i32,
        scope_tag_ids: &[String],
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<ChunkSearchResult>> {
        let conn = self.db.read_conn()?;

        let escaped_query = escape_fts5_query(query);
        if escaped_query.is_empty() {
            return Ok(Vec::new());
        }
        let fetch_limit = limit * 3;

        let raw_results: Vec<(String, String, String, i32, f64, String)> =
            fts_search_with_cutoff(&conn, &escaped_query, fetch_limit, created_after)?;

        // Apply tag scope filter if specified
        let filtered = if scope_tag_ids.is_empty() {
            raw_results
        } else {
            let candidate_atom_ids: Vec<&str> = raw_results.iter().map(|r| r.1.as_str()).collect();
            let matching = batch_atoms_with_scope_tags(&conn, &candidate_atom_ids, scope_tag_ids)?;
            raw_results
                .into_iter()
                .filter(|r| matching.contains(r.1.as_str()))
                .collect()
        };

        // Apply kind filter (post-pass — keeps FTS helper kind-agnostic)
        let filtered = if matches!(kinds, crate::models::KindFilter::All) {
            filtered
        } else {
            let candidate_atom_ids: Vec<&str> = filtered.iter().map(|r| r.1.as_str()).collect();
            let allowed = batch_atoms_matching_kind(&conn, &candidate_atom_ids, kinds)?;
            filtered
                .into_iter()
                .filter(|r| allowed.contains(&r.1))
                .collect()
        };

        let results: Vec<ChunkSearchResult> = filtered
            .into_iter()
            .take(limit as usize)
            .map(
                |(chunk_id, atom_id, content, chunk_index, bm25_score, _snippet)| {
                    ChunkSearchResult {
                        chunk_id,
                        atom_id,
                        content,
                        chunk_index,
                        score: normalize_bm25_score(bm25_score),
                    }
                },
            )
            .collect();

        Ok(results)
    }

    pub(crate) fn vector_search_chunks_sync(
        &self,
        query_embedding: &[f32],
        limit: i32,
        threshold: f32,
        scope_tag_ids: &[String],
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<ChunkSearchResult>> {
        let query_blob = f32_vec_to_blob_public(query_embedding);
        let conn = self.db.read_conn()?;
        let fetch_limit = limit * 5;

        let similar_chunks: Vec<(String, f32)> =
            vec_knn_with_cutoff(&conn, &query_blob, fetch_limit, created_after)?;

        // Filter by threshold
        let filtered: Vec<(String, f32)> = similar_chunks
            .into_iter()
            .filter(|(_, distance)| distance_to_similarity(*distance) >= threshold)
            .collect();

        // Batch-load chunk details
        let chunk_ids: Vec<String> = filtered.iter().map(|(id, _)| id.clone()).collect();
        let chunk_map = batch_fetch_chunk_info(&conn, &chunk_ids)?;

        // Apply tag scope filter
        let scope_atom_ids: std::collections::HashSet<String> = if !scope_tag_ids.is_empty() {
            let candidate_atom_ids: Vec<&str> =
                chunk_map.values().map(|(aid, _, _)| aid.as_str()).collect();
            batch_atoms_with_scope_tags(&conn, &candidate_atom_ids, scope_tag_ids)?
        } else {
            std::collections::HashSet::new()
        };

        // Apply kind filter
        let kind_allowed: Option<std::collections::HashSet<String>> =
            if matches!(kinds, crate::models::KindFilter::All) {
                None
            } else {
                let candidate_atom_ids: Vec<&str> =
                    chunk_map.values().map(|(aid, _, _)| aid.as_str()).collect();
                Some(batch_atoms_matching_kind(
                    &conn,
                    &candidate_atom_ids,
                    kinds,
                )?)
            };

        let mut results = Vec::new();
        for (chunk_id, distance) in &filtered {
            let similarity = distance_to_similarity(*distance);
            if let Some((atom_id, content, chunk_index)) = chunk_map.get(chunk_id) {
                if !scope_tag_ids.is_empty() && !scope_atom_ids.contains(atom_id) {
                    continue;
                }
                if let Some(allowed) = &kind_allowed {
                    if !allowed.contains(atom_id) {
                        continue;
                    }
                }
                results.push(ChunkSearchResult {
                    chunk_id: chunk_id.clone(),
                    atom_id: atom_id.clone(),
                    content: content.clone(),
                    chunk_index: *chunk_index,
                    score: similarity,
                });
            }
            if results.len() >= limit as usize {
                break;
            }
        }

        Ok(results)
    }

    pub(crate) fn find_similar_sync(
        &self,
        atom_id: &str,
        limit: i32,
        threshold: f32,
    ) -> StorageResult<Vec<SimilarAtomResult>> {
        let conn = self.db.read_conn()?;
        search::find_similar_atoms(&conn, atom_id, limit, threshold)
            .map_err(|e| AtomicCoreError::Search(e))
    }

    pub(crate) fn global_keyword_search_sync(
        &self,
        query: &str,
        section_limit: i32,
    ) -> StorageResult<GlobalSearchResponse> {
        let conn = self.db.read_conn()?;
        let escaped_query = escape_fts5_query(query);
        let trimmed_query = query.trim().to_lowercase();
        if escaped_query.is_empty() || trimmed_query.is_empty() {
            return Ok(GlobalSearchResponse {
                atoms: Vec::new(),
                wiki: Vec::new(),
                chats: Vec::new(),
                tags: Vec::new(),
            });
        }

        // Global search is the in-app palette — show every kind, mirroring
        // the UI list/canvas surfaces.
        let atoms = self.keyword_search_sync(
            query,
            section_limit,
            None,
            None,
            &crate::models::KindFilter::All,
        )?;
        let wiki = keyword_search_wiki(&conn, &escaped_query, section_limit)?;
        let chats = keyword_search_chats(&conn, &escaped_query, &trimmed_query, section_limit)?;
        let tags = keyword_search_tags(&conn, &trimmed_query, section_limit)?;

        Ok(GlobalSearchResponse {
            atoms,
            wiki,
            chats,
            tags,
        })
    }
}

#[async_trait]
impl SearchStore for SqliteStorage {
    async fn vector_search(
        &self,
        query_embedding: &[f32],
        limit: i32,
        threshold: f32,
        tag_id: Option<&str>,
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<SemanticSearchResult>> {
        let storage = self.clone();
        let query_embedding = query_embedding.to_vec();
        let tag_id = tag_id.map(|s| s.to_string());
        let created_after = created_after.map(|s| s.to_string());
        let kinds = kinds.clone();
        tokio::task::spawn_blocking(move || {
            storage.vector_search_sync(
                &query_embedding,
                limit,
                threshold,
                tag_id.as_deref(),
                created_after.as_deref(),
                &kinds,
            )
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn keyword_search(
        &self,
        query: &str,
        limit: i32,
        tag_id: Option<&str>,
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<SemanticSearchResult>> {
        let storage = self.clone();
        let query = query.to_string();
        let tag_id = tag_id.map(|s| s.to_string());
        let created_after = created_after.map(|s| s.to_string());
        let kinds = kinds.clone();
        tokio::task::spawn_blocking(move || {
            storage.keyword_search_sync(
                &query,
                limit,
                tag_id.as_deref(),
                created_after.as_deref(),
                &kinds,
            )
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn find_similar(
        &self,
        atom_id: &str,
        limit: i32,
        threshold: f32,
    ) -> StorageResult<Vec<SimilarAtomResult>> {
        let storage = self.clone();
        let atom_id = atom_id.to_string();
        tokio::task::spawn_blocking(move || storage.find_similar_sync(&atom_id, limit, threshold))
            .await
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }

    async fn keyword_search_chunks(
        &self,
        query: &str,
        limit: i32,
        scope_tag_ids: &[String],
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<ChunkSearchResult>> {
        let storage = self.clone();
        let query = query.to_string();
        let scope_tag_ids = scope_tag_ids.to_vec();
        let created_after = created_after.map(|s| s.to_string());
        let kinds = kinds.clone();
        tokio::task::spawn_blocking(move || {
            storage.keyword_search_chunks_sync(
                &query,
                limit,
                &scope_tag_ids,
                created_after.as_deref(),
                &kinds,
            )
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
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
        let storage = self.clone();
        let query_embedding = query_embedding.to_vec();
        let scope_tag_ids = scope_tag_ids.to_vec();
        let created_after = created_after.map(|s| s.to_string());
        let kinds = kinds.clone();
        tokio::task::spawn_blocking(move || {
            storage.vector_search_chunks_sync(
                &query_embedding,
                limit,
                threshold,
                &scope_tag_ids,
                created_after.as_deref(),
                &kinds,
            )
        })
        .await
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?
    }
}

// ==================== Local Helper Functions ====================

/// Maximum number of match offsets included per search result. Capping here
/// (rather than on the client) keeps the payload bounded and gives every
/// consumer — palette, future API users, tests — the same truncated view.
/// When a result has more matches than this, `SemanticSearchResult.match_count`
/// / `GlobalWikiSearchResult.match_count` carries the true total so the UI can
/// still honestly say "37 matches".
pub(crate) const MAX_MATCH_OFFSETS_PER_RESULT: usize = 10;

/// Walk a `highlight()` result (atom content with `\u{E000}`/`\u{E001}` pairs
/// wrapping each match) and produce byte-offset ranges into the un-marked text.
fn parse_match_offsets(highlighted: &str) -> Vec<crate::models::MatchOffset> {
    const MARK_START: char = '\u{E000}';
    const MARK_END: char = '\u{E001}';
    let mut offsets = Vec::new();
    let mut stripped_pos: u32 = 0;
    let mut current_start: Option<u32> = None;
    for ch in highlighted.chars() {
        if ch == MARK_START {
            current_start = Some(stripped_pos);
        } else if ch == MARK_END {
            if let Some(start) = current_start.take() {
                offsets.push(crate::models::MatchOffset {
                    start,
                    end: stripped_pos,
                });
            }
        } else {
            stripped_pos += ch.len_utf8() as u32;
        }
    }
    offsets
}

/// Run the atom-level FTS5 query, returning `(atom_id, bm25_score, snippet,
/// highlighted_content)` per match. The snippet is windowed around matched
/// tokens; the highlighted content is the full atom body with markers around
/// every hit so the caller can extract match offsets.
fn atom_fts_search_with_cutoff(
    conn: &rusqlite::Connection,
    escaped_query: &str,
    fetch_limit: i32,
    created_after: Option<&str>,
) -> Result<Vec<(String, f64, String, String)>, AtomicCoreError> {
    let row_map = |row: &rusqlite::Row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    };
    if let Some(cutoff) = created_after {
        let mut stmt = conn
            .prepare(
                "SELECT f.id,
                        bm25(atoms_fts) AS score,
                        snippet(atoms_fts, 1, '\u{E000}', '\u{E001}', '…', 20) AS snippet,
                        highlight(atoms_fts, 1, '\u{E000}', '\u{E001}') AS highlighted
                 FROM atoms_fts f
                 INNER JOIN atoms a ON a.rowid = f.rowid
                 WHERE atoms_fts MATCH ?1 AND a.created_at >= ?2
                 ORDER BY bm25(atoms_fts)
                 LIMIT ?3",
            )
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to prepare atom FTS query: {}", e))
            })?;
        let rows: Vec<_> = stmt
            .query_map(
                rusqlite::params![escaped_query, cutoff, fetch_limit],
                row_map,
            )
            .map_err(|e| AtomicCoreError::Search(format!("Failed to query atom FTS: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to collect atom FTS results: {}", e))
            })?;
        Ok(rows)
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT id,
                        bm25(atoms_fts) AS score,
                        snippet(atoms_fts, 1, '\u{E000}', '\u{E001}', '…', 20) AS snippet,
                        highlight(atoms_fts, 1, '\u{E000}', '\u{E001}') AS highlighted
                 FROM atoms_fts
                 WHERE atoms_fts MATCH ?1
                 ORDER BY bm25(atoms_fts)
                 LIMIT ?2",
            )
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to prepare atom FTS query: {}", e))
            })?;
        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![escaped_query, fetch_limit], row_map)
            .map_err(|e| AtomicCoreError::Search(format!("Failed to query atom FTS: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to collect atom FTS results: {}", e))
            })?;
        Ok(rows)
    }
}

/// Run the FTS5 keyword query, optionally constrained to chunks whose parent atom
/// was created at or after `created_after` (ISO 8601 cutoff).
///
/// Each row carries a `snippet` column: a windowed excerpt around the matched
/// tokens with `\u{E000}`/`\u{E001}` Private Use Area markers wrapping each hit.
/// The column index `3` refers to the FTS virtual table's `content` column.
fn fts_search_with_cutoff(
    conn: &rusqlite::Connection,
    escaped_query: &str,
    fetch_limit: i32,
    created_after: Option<&str>,
) -> Result<Vec<(String, String, String, i32, f64, String)>, AtomicCoreError> {
    let row_map = |row: &rusqlite::Row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i32>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, String>(5)?,
        ))
    };
    if let Some(cutoff) = created_after {
        let mut stmt = conn
            .prepare(
                "SELECT f.id, f.atom_id, f.content, f.chunk_index,
                        bm25(atom_chunks_fts) AS score,
                        snippet(atom_chunks_fts, 3, '\u{E000}', '\u{E001}', '…', 20) AS snippet
                 FROM atom_chunks_fts f
                 INNER JOIN atoms a ON a.id = f.atom_id
                 WHERE atom_chunks_fts MATCH ?1 AND a.created_at >= ?2
                 ORDER BY bm25(atom_chunks_fts)
                 LIMIT ?3",
            )
            .map_err(|e| AtomicCoreError::Search(format!("Failed to prepare FTS query: {}", e)))?;
        let rows: Vec<_> = stmt
            .query_map(
                rusqlite::params![escaped_query, cutoff, fetch_limit],
                row_map,
            )
            .map_err(|e| AtomicCoreError::Search(format!("Failed to query FTS: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to collect FTS results: {}", e))
            })?;
        Ok(rows)
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT id, atom_id, content, chunk_index,
                        bm25(atom_chunks_fts) AS score,
                        snippet(atom_chunks_fts, 3, '\u{E000}', '\u{E001}', '…', 20) AS snippet
                 FROM atom_chunks_fts
                 WHERE atom_chunks_fts MATCH ?1
                 ORDER BY bm25(atom_chunks_fts)
                 LIMIT ?2",
            )
            .map_err(|e| AtomicCoreError::Search(format!("Failed to prepare FTS query: {}", e)))?;
        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![escaped_query, fetch_limit], row_map)
            .map_err(|e| AtomicCoreError::Search(format!("Failed to query FTS: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to collect FTS results: {}", e))
            })?;
        Ok(rows)
    }
}

/// Run the sqlite-vec KNN query, optionally restricted to chunks whose parent atom
/// was created at or after `created_after`. The KNN MATCH must be the sole constraint
/// on `vec_chunks`, so the cutoff is applied via an outer join that filters the top-k
/// by-distance set.
fn vec_knn_with_cutoff(
    conn: &rusqlite::Connection,
    query_blob: &[u8],
    fetch_limit: i32,
    created_after: Option<&str>,
) -> Result<Vec<(String, f32)>, AtomicCoreError> {
    let row_map = |row: &rusqlite::Row| Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?));
    if let Some(cutoff) = created_after {
        // sqlite-vec requires MATCH to be the sole predicate on vec_chunks, so the
        // cutoff filter runs over the top-k result set. Inflate the inner LIMIT so
        // that if most of the nearest chunks predate the cutoff we still have
        // enough survivors to reach the caller's requested limit.
        let knn_limit = fetch_limit.saturating_mul(5);
        let mut stmt = conn
            .prepare(
                "SELECT v.chunk_id, v.distance
                 FROM (
                     SELECT chunk_id, distance
                     FROM vec_chunks
                     WHERE embedding MATCH ?1
                     ORDER BY distance
                     LIMIT ?2
                 ) v
                 INNER JOIN atom_chunks c ON c.id = v.chunk_id
                 INNER JOIN atoms a ON a.id = c.atom_id
                 WHERE a.created_at >= ?3
                 ORDER BY v.distance
                 LIMIT ?4",
            )
            .map_err(|e| AtomicCoreError::Search(format!("Failed to prepare vec query: {}", e)))?;
        let rows: Vec<_> = stmt
            .query_map(
                rusqlite::params![query_blob, knn_limit, cutoff, fetch_limit],
                row_map,
            )
            .map_err(|e| AtomicCoreError::Search(format!("Failed to query similar chunks: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to collect similar chunks: {}", e))
            })?;
        Ok(rows)
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT chunk_id, distance
                 FROM vec_chunks
                 WHERE embedding MATCH ?1
                 ORDER BY distance
                 LIMIT ?2",
            )
            .map_err(|e| AtomicCoreError::Search(format!("Failed to prepare vec query: {}", e)))?;
        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![query_blob, fetch_limit], row_map)
            .map_err(|e| AtomicCoreError::Search(format!("Failed to query similar chunks: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to collect similar chunks: {}", e))
            })?;
        Ok(rows)
    }
}

/// Escape special characters for FTS5 MATCH query.
/// Wraps each word in quotes to treat them as literal terms.
fn escape_fts5_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|word| {
            let cleaned = word.replace('"', "");
            if cleaned.is_empty() {
                String::new()
            } else {
                format!("\"{}\"", cleaned)
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize BM25 score to 0-1 range.
/// BM25 scores are negative (lower = better), typically -30 to 0.
fn normalize_bm25_score(score: f64) -> f32 {
    let clamped = score.clamp(-30.0, 0.0);
    (1.0 - (clamped / -30.0) * 0.7) as f32
}

fn keyword_search_wiki(
    conn: &rusqlite::Connection,
    escaped_query: &str,
    limit: i32,
) -> Result<Vec<GlobalWikiSearchResult>, AtomicCoreError> {
    // Column 3 of wiki_articles_fts is `content` (the indexed body).
    let mut stmt = conn
        .prepare(
            "SELECT id, tag_id, tag_name, content,
                    bm25(wiki_articles_fts) AS score,
                    snippet(wiki_articles_fts, 3, '\u{E000}', '\u{E001}', '…', 20) AS snippet,
                    highlight(wiki_articles_fts, 3, '\u{E000}', '\u{E001}') AS highlighted
             FROM wiki_articles_fts
             WHERE wiki_articles_fts MATCH ?1
             ORDER BY bm25(wiki_articles_fts)
             LIMIT ?2",
        )
        .map_err(|e| AtomicCoreError::Search(format!("Failed to prepare wiki FTS query: {}", e)))?;
    let rows: Vec<(String, String, String, String, f64, String, String)> = stmt
        .query_map(rusqlite::params![escaped_query, limit], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .map_err(|e| AtomicCoreError::Search(format!("Failed to query wiki FTS: {}", e)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AtomicCoreError::Search(format!("Failed to collect wiki FTS rows: {}", e)))?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let tag_ids: Vec<String> = rows
        .iter()
        .map(|(_, tag_id, _, _, _, _, _)| tag_id.clone())
        .collect();
    let mut atom_counts = HashMap::new();
    let placeholders = tag_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT tag_id, atom_count FROM wiki_articles WHERE tag_id IN ({})",
        placeholders
    );
    let mut count_stmt = conn.prepare(&query).map_err(|e| {
        AtomicCoreError::Search(format!("Failed to prepare wiki count query: {}", e))
    })?;
    let count_rows = count_stmt
        .query_map(rusqlite::params_from_iter(tag_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
        })
        .map_err(|e| AtomicCoreError::Search(format!("Failed to query wiki counts: {}", e)))?;
    for row in count_rows {
        let (tag_id, atom_count) = row.map_err(|e| AtomicCoreError::Search(e.to_string()))?;
        atom_counts.insert(tag_id, atom_count);
    }

    let mut updated_stmt = conn
        .prepare("SELECT updated_at FROM wiki_articles WHERE id = ?1")
        .map_err(|e| {
            AtomicCoreError::Search(format!("Failed to prepare wiki updated_at query: {}", e))
        })?;

    let mut results = Vec::with_capacity(rows.len());
    for (id, tag_id, tag_name, content, score, fts_snippet, highlighted) in rows {
        let updated_at: Option<String> = updated_stmt
            .query_row([&id], |row| row.get(0))
            .optional()
            .map_err(|e| {
                AtomicCoreError::Search(format!("Failed to load wiki updated_at: {}", e))
            })?;
        let Some(updated_at) = updated_at else {
            tracing::warn!(wiki_article_id = %id, tag_id = %tag_id, "Skipping stale wiki FTS row without backing article");
            continue;
        };
        let mut offsets = parse_match_offsets(&highlighted);
        let total = offsets.len() as u32;
        offsets.truncate(MAX_MATCH_OFFSETS_PER_RESULT);
        results.push(GlobalWikiSearchResult {
            id,
            tag_id: tag_id.clone(),
            tag_name,
            content_snippet: snippet(&content, 180),
            content,
            updated_at,
            atom_count: atom_counts.get(&tag_id).copied().unwrap_or(0),
            score: normalize_bm25_score(score),
            match_snippet: Some(fts_snippet),
            match_offsets: Some(offsets),
            match_count: Some(total),
        });
    }

    Ok(results)
}

fn keyword_search_chats(
    conn: &rusqlite::Connection,
    escaped_query: &str,
    trimmed_query: &str,
    limit: i32,
) -> Result<Vec<GlobalChatSearchResult>, AtomicCoreError> {
    let mut conversation_best: HashMap<String, (f32, String)> = HashMap::new();

    let mut msg_stmt = conn
        .prepare(
            "SELECT conversation_id, content, bm25(chat_messages_fts) AS score
             FROM chat_messages_fts
             WHERE chat_messages_fts MATCH ?1
             ORDER BY bm25(chat_messages_fts)
             LIMIT ?2",
        )
        .map_err(|e| AtomicCoreError::Search(format!("Failed to prepare chat FTS query: {}", e)))?;
    let msg_rows = msg_stmt
        .query_map(rusqlite::params![escaped_query, limit * 4], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                normalize_bm25_score(row.get::<_, f64>(2)?),
            ))
        })
        .map_err(|e| AtomicCoreError::Search(format!("Failed to query chat FTS: {}", e)))?;
    for row in msg_rows {
        let (conversation_id, content, score) =
            row.map_err(|e| AtomicCoreError::Search(e.to_string()))?;
        let entry = conversation_best
            .entry(conversation_id)
            .or_insert((score, content.clone()));
        if score > entry.0 {
            *entry = (score, content);
        }
    }

    let title_pattern = format!("%{}%", trimmed_query);
    let mut title_stmt = conn
        .prepare(
            "SELECT id, COALESCE(title, '')
             FROM conversations
             WHERE is_archived = 0 AND title IS NOT NULL AND lower(title) LIKE ?1
             LIMIT ?2",
        )
        .map_err(|e| {
            AtomicCoreError::Search(format!("Failed to prepare chat title query: {}", e))
        })?;
    let title_rows = title_stmt
        .query_map(rusqlite::params![title_pattern, limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| AtomicCoreError::Search(format!("Failed to query chat titles: {}", e)))?;
    for row in title_rows {
        let (conversation_id, title) = row.map_err(|e| AtomicCoreError::Search(e.to_string()))?;
        let score = if title.to_lowercase().starts_with(trimmed_query) {
            0.98
        } else {
            0.9
        };
        let snippet_text = if title.is_empty() {
            String::new()
        } else {
            title
        };
        let entry = conversation_best
            .entry(conversation_id)
            .or_insert((score, snippet_text.clone()));
        if score > entry.0 {
            *entry = (score, snippet_text);
        }
    }

    if conversation_best.is_empty() {
        return Ok(Vec::new());
    }

    let mut ranked: Vec<(String, f32, String)> = conversation_best
        .into_iter()
        .map(|(conversation_id, (score, snippet_text))| (conversation_id, score, snippet_text))
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(limit as usize);

    let conversation_ids: Vec<String> = ranked.iter().map(|(id, _, _)| id.clone()).collect();
    let conversation_meta = batch_fetch_conversation_meta(conn, &conversation_ids)?;
    let conversation_tags = batch_fetch_conversation_tags(conn, &conversation_ids)?;

    let mut results = Vec::with_capacity(ranked.len());
    for (conversation_id, score, matching_text) in ranked {
        if let Some((title, updated_at, message_count)) = conversation_meta.get(&conversation_id) {
            results.push(GlobalChatSearchResult {
                id: conversation_id.clone(),
                title: title.clone(),
                updated_at: updated_at.clone(),
                message_count: *message_count,
                tags: conversation_tags
                    .get(&conversation_id)
                    .cloned()
                    .unwrap_or_default(),
                matching_message_content: snippet(&matching_text, 180),
                score,
            });
        }
    }

    Ok(results)
}

fn keyword_search_tags(
    conn: &rusqlite::Connection,
    trimmed_query: &str,
    limit: i32,
) -> Result<Vec<GlobalTagSearchResult>, AtomicCoreError> {
    let pattern = format!("%{}%", trimmed_query);
    let mut stmt = conn
        .prepare(
            "SELECT id, name, parent_id, created_at, atom_count
             FROM tags
             WHERE lower(name) LIKE ?1
             ORDER BY atom_count DESC, name ASC",
        )
        .map_err(|e| {
            AtomicCoreError::Search(format!("Failed to prepare tag search query: {}", e))
        })?;
    let rows = stmt
        .query_map([pattern], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i32>(4)?,
            ))
        })
        .map_err(|e| AtomicCoreError::Search(format!("Failed to query tags: {}", e)))?;

    let mut exactish = Vec::new();
    for row in rows {
        let (id, name, parent_id, created_at, atom_count) =
            row.map_err(|e| AtomicCoreError::Search(e.to_string()))?;
        let lower = name.to_lowercase();
        let score = if lower == trimmed_query {
            1.0
        } else if lower.starts_with(trimmed_query) {
            0.95
        } else if strong_substring_match(&lower, trimmed_query) {
            0.8
        } else {
            continue;
        };
        exactish.push(GlobalTagSearchResult {
            id,
            name,
            parent_id,
            created_at,
            atom_count,
            score,
        });
    }

    exactish.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.atom_count.cmp(&a.atom_count))
            .then(a.name.cmp(&b.name))
    });
    exactish.truncate(limit as usize);
    Ok(exactish)
}

fn strong_substring_match(haystack: &str, needle: &str) -> bool {
    if needle.len() < 2 {
        return haystack == needle;
    }
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment.contains(needle))
}

fn snippet(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let snippet: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}...", snippet)
    } else {
        snippet
    }
}

/// Batch fetch atoms by IDs in a single query.
fn batch_fetch_atoms(
    conn: &rusqlite::Connection,
    atom_ids: &[String],
) -> Result<HashMap<String, Atom>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT {} FROM atoms WHERE id IN ({})",
        crate::ATOM_COLUMNS,
        placeholders
    );
    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(atom_ids.iter()),
            crate::atom_from_row,
        )
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;

    Ok(rows.into_iter().map(|a| (a.id.clone(), a)).collect())
}

/// Batch fetch tags for multiple atoms in a single query.
fn batch_fetch_tags(
    conn: &rusqlite::Connection,
    atom_ids: &[String],
) -> Result<HashMap<String, Vec<Tag>>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM atom_tags at
         INNER JOIN tags t ON at.tag_id = t.id
         WHERE at.atom_id IN ({})",
        placeholders
    );
    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
    let mut map: HashMap<String, Vec<Tag>> = HashMap::new();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(atom_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                Tag {
                    id: row.get(1)?,
                    name: row.get(2)?,
                    parent_id: row.get(3)?,
                    created_at: row.get(4)?,
                    is_autotag_target: row.get::<_, i32>(5)? != 0,
                    autotag_description: row.get(6)?,
                },
            ))
        })
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
    for row in rows {
        let (atom_id, tag) = row.map_err(|e| AtomicCoreError::Search(e.to_string()))?;
        map.entry(atom_id).or_default().push(tag);
    }
    Ok(map)
}

fn batch_fetch_conversation_meta(
    conn: &rusqlite::Connection,
    conversation_ids: &[String],
) -> Result<HashMap<String, (Option<String>, String, i32)>, AtomicCoreError> {
    if conversation_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = conversation_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT c.id, c.title, c.updated_at, COUNT(m.id) AS message_count
         FROM conversations c
         LEFT JOIN chat_messages m ON m.conversation_id = c.id
         WHERE c.id IN ({}) AND c.is_archived = 0
         GROUP BY c.id, c.title, c.updated_at",
        placeholders
    );
    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(conversation_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i32>(3)?,
            ))
        })
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
    let mut map = HashMap::new();
    for row in rows {
        let (id, title, updated_at, message_count) =
            row.map_err(|e| AtomicCoreError::Search(e.to_string()))?;
        map.insert(id, (title, updated_at, message_count));
    }
    Ok(map)
}

fn batch_fetch_conversation_tags(
    conn: &rusqlite::Connection,
    conversation_ids: &[String],
) -> Result<HashMap<String, Vec<Tag>>, AtomicCoreError> {
    if conversation_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = conversation_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT ct.conversation_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM conversation_tags ct
         INNER JOIN tags t ON t.id = ct.tag_id
         WHERE ct.conversation_id IN ({})",
        placeholders
    );
    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(conversation_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                Tag {
                    id: row.get(1)?,
                    name: row.get(2)?,
                    parent_id: row.get(3)?,
                    created_at: row.get(4)?,
                    is_autotag_target: row.get::<_, i32>(5)? != 0,
                    autotag_description: row.get(6)?,
                },
            ))
        })
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
    let mut map: HashMap<String, Vec<Tag>> = HashMap::new();
    for row in rows {
        let (conversation_id, tag) = row.map_err(|e| AtomicCoreError::Search(e.to_string()))?;
        map.entry(conversation_id).or_default().push(tag);
    }
    Ok(map)
}

/// Batch fetch chunk info by IDs in a single query.
fn batch_fetch_chunk_info(
    conn: &rusqlite::Connection,
    chunk_ids: &[String],
) -> Result<HashMap<String, (String, String, i32)>, AtomicCoreError> {
    if chunk_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT id, atom_id, content, chunk_index FROM atom_chunks WHERE id IN ({})",
        placeholders
    );
    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(chunk_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i32>(3)?,
            ))
        })
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AtomicCoreError::Search(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|(id, atom_id, content, idx)| (id, (atom_id, content, idx)))
        .collect())
}

/// Batch check which atom_ids have at least one of the specified scope tags.
fn batch_atoms_with_scope_tags(
    conn: &rusqlite::Connection,
    atom_ids: &[&str],
    scope_tag_ids: &[String],
) -> Result<std::collections::HashSet<String>, AtomicCoreError> {
    if atom_ids.is_empty() || scope_tag_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    // Use recursive CTE to include atoms tagged with descendants of the scope tags
    let atom_placeholders: Vec<&str> = atom_ids.iter().map(|_| "?").collect();
    let tag_placeholders: Vec<&str> = scope_tag_ids.iter().map(|_| "?").collect();
    let query = format!(
        "WITH RECURSIVE scope_tags(id) AS (
            SELECT id FROM tags WHERE id IN ({tag_ph})
            UNION ALL
            SELECT t.id FROM tags t
            INNER JOIN scope_tags st ON t.parent_id = st.id
         )
         SELECT DISTINCT atom_id FROM atom_tags
         WHERE atom_id IN ({atom_ph}) AND tag_id IN (SELECT id FROM scope_tags)",
        tag_ph = tag_placeholders.join(","),
        atom_ph = atom_placeholders.join(","),
    );

    // Bind order matches SQL: tag_ids first (CTE), then atom_ids (WHERE)
    let mut params: Vec<&dyn rusqlite::ToSql> =
        Vec::with_capacity(atom_ids.len() + scope_tag_ids.len());
    for id in scope_tag_ids {
        params.push(id);
    }
    for id in atom_ids {
        params.push(id);
    }

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| AtomicCoreError::Search(format!("Failed to prepare scope query: {}", e)))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| AtomicCoreError::Search(format!("Failed to execute scope query: {}", e)))?;

    let mut matching = std::collections::HashSet::new();
    for row in rows {
        matching.insert(
            row.map_err(|e| {
                AtomicCoreError::Search(format!("Failed to read scope result: {}", e))
            })?,
        );
    }
    Ok(matching)
}

/// Batch lookup of which atom_ids match the given `KindFilter`. Returns the
/// subset that passes. `KindFilter::All` is handled by the caller (no query).
fn batch_atoms_matching_kind(
    conn: &rusqlite::Connection,
    atom_ids: &[&str],
    kinds: &crate::models::KindFilter,
) -> Result<std::collections::HashSet<String>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let (kind_frag, kind_binds) = kinds.sqlite_in_clause("kind");
    let atom_placeholders: Vec<&str> = atom_ids.iter().map(|_| "?").collect();
    let query = format!(
        "SELECT id FROM atoms WHERE id IN ({}) AND {kind_frag}",
        atom_placeholders.join(",")
    );
    let mut params: Vec<&dyn rusqlite::ToSql> =
        Vec::with_capacity(atom_ids.len() + kind_binds.len());
    for id in atom_ids {
        params.push(id);
    }
    for v in &kind_binds {
        params.push(v as &dyn rusqlite::ToSql);
    }
    let mut stmt = conn.prepare(&query).map_err(|e| {
        AtomicCoreError::Search(format!("Failed to prepare kind-scope query: {}", e))
    })?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| {
            AtomicCoreError::Search(format!("Failed to execute kind-scope query: {}", e))
        })?;
    let mut matching = std::collections::HashSet::new();
    for row in rows {
        matching.insert(row.map_err(|e| {
            AtomicCoreError::Search(format!("Failed to read kind-scope result: {}", e))
        })?);
    }
    Ok(matching)
}
