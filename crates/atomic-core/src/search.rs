//! Unified search module for Atomic
//!
//! Provides a single search implementation that supports keyword (BM25), semantic (vector),
//! and hybrid (RRF-combined) search modes. Used by UI, chat agent, MCP, and wiki generation.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::db::Database;
use crate::embedding::{distance_to_similarity, f32_vec_to_blob_public};
use crate::models::{Atom, AtomWithTags, KindFilter, SemanticSearchResult, SimilarAtomResult, Tag};
use crate::providers::{get_embedding_provider, EmbeddingConfig, ProviderConfig};
use crate::settings::get_all_settings;

/// Search mode - determines which search algorithm(s) to use
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchMode {
    /// BM25 keyword search using FTS5
    Keyword,
    /// Vector similarity search using embeddings
    Semantic,
    /// Combines keyword and semantic using Reciprocal Rank Fusion
    Hybrid,
}

/// Options for search queries
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// The search query text
    pub query: String,
    /// Search algorithm to use
    pub mode: SearchMode,
    /// Maximum number of results to return
    pub limit: i32,
    /// Minimum similarity threshold (0.0-1.0) for semantic/hybrid modes
    pub threshold: f32,
    /// Optional tag IDs to filter results (only return atoms with these tags)
    pub scope_tag_ids: Vec<String>,
    /// Optional recency filter: only return atoms created within the last N days
    pub since_days: Option<i32>,
    /// Atom-kind filter. Defaults to `KindFilter::All` so existing callers
    /// (UI, chat, wiki) keep their unfiltered behavior; external surfaces
    /// (the MCP `semantic_search` tool, and any future `?kinds=` query
    /// parameter on `/api/search`) opt in to a tighter filter — typically
    /// `KindFilter::only(AtomKind::Captured)` to exclude report findings.
    pub kinds: KindFilter,
}

impl SearchOptions {
    pub fn new(query: impl Into<String>, mode: SearchMode, limit: i32) -> Self {
        Self {
            query: query.into(),
            mode,
            limit,
            threshold: 0.3,
            scope_tag_ids: vec![],
            since_days: None,
            kinds: KindFilter::All,
        }
    }

    pub fn with_threshold(mut self, threshold: f32) -> Self {
        self.threshold = threshold;
        self
    }

    pub fn with_scope(mut self, tag_ids: Vec<String>) -> Self {
        self.scope_tag_ids = tag_ids;
        self
    }

    pub fn with_since_days(mut self, since_days: Option<i32>) -> Self {
        self.since_days = since_days;
        self
    }

    pub fn with_kinds(mut self, kinds: KindFilter) -> Self {
        self.kinds = kinds;
        self
    }
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            mode: SearchMode::Hybrid,
            limit: 10,
            threshold: 0.3,
            scope_tag_ids: vec![],
            since_days: None,
            kinds: KindFilter::All,
        }
    }
}

/// A single chunk result from search
#[derive(Debug, Clone)]
pub struct ChunkResult {
    pub chunk_id: String,
    pub atom_id: String,
    pub content: String,
    pub chunk_index: i32,
    /// Normalized score (0.0-1.0), higher is better
    pub score: f32,
}

/// RRF constant - standard value that prevents high ranks from dominating
const RRF_K: f32 = 60.0;

/// Merge semantic and keyword search results using Reciprocal Rank Fusion.
/// Deduplicates by atom_id, keeping the highest combined score.
/// Used by the Postgres search path where results are already `SemanticSearchResult`.
pub fn merge_search_results_rrf(
    semantic: Vec<SemanticSearchResult>,
    keyword: Vec<SemanticSearchResult>,
    limit: i32,
) -> Vec<SemanticSearchResult> {
    let mut scores: HashMap<String, (f32, SemanticSearchResult)> = HashMap::new();

    for (rank, result) in semantic.iter().enumerate() {
        let rrf = 1.0 / (RRF_K + (rank + 1) as f32);
        scores
            .entry(result.atom.atom.id.clone())
            .and_modify(|(s, _)| *s += rrf)
            .or_insert((rrf, result.clone()));
    }

    for (rank, result) in keyword.iter().enumerate() {
        let rrf = 1.0 / (RRF_K + (rank + 1) as f32);
        scores
            .entry(result.atom.atom.id.clone())
            .and_modify(|(s, _)| *s += rrf)
            .or_insert((rrf, result.clone()));
    }

    let max_rrf = 2.0 / (RRF_K + 1.0);
    let mut combined: Vec<SemanticSearchResult> = scores
        .into_values()
        .map(|(score, mut result)| {
            result.similarity_score = (score / max_rrf).clamp(0.0, 1.0);
            result
        })
        .collect();

    combined.sort_by(|a, b| {
        b.similarity_score
            .partial_cmp(&a.similarity_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    combined.truncate(limit as usize);
    combined
}

/// Escape special characters for FTS5 MATCH query
/// Wraps each word in quotes to treat them as literal terms
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

/// Normalize BM25 score to 0-1 range
/// BM25 scores are negative (lower = better), typically -30 to 0
fn normalize_bm25_score(score: f64) -> f32 {
    let clamped = score.clamp(-30.0, 0.0);
    (1.0 - (clamped / -30.0) * 0.7) as f32
}

/// Core search function - returns raw chunks without atom deduplication
///
/// Use this when you need multiple chunks per atom (e.g., wiki generation).
/// For most UI cases, use `search_atoms()` instead.
pub async fn search_chunks(
    db: &Database,
    options: SearchOptions,
) -> Result<Vec<ChunkResult>, String> {
    search_chunks_with_settings(db, options, None).await
}

/// Like `search_chunks` but with externally-provided settings (from registry).
pub async fn search_chunks_with_settings(
    db: &Database,
    options: SearchOptions,
    external_settings: Option<HashMap<String, String>>,
) -> Result<Vec<ChunkResult>, String> {
    match options.mode {
        SearchMode::Keyword => search_keyword_chunks(db, &options).await,
        SearchMode::Semantic => search_semantic_chunks(db, &options, external_settings).await,
        SearchMode::Hybrid => search_hybrid_chunks(db, &options, external_settings).await,
    }
}

/// Search and return deduplicated atoms with full data
///
/// This is the main entry point for UI/chat/MCP search. Returns one result per atom
/// with the best-matching chunk info attached.
pub async fn search_atoms(
    db: &Database,
    options: SearchOptions,
) -> Result<Vec<SemanticSearchResult>, String> {
    search_atoms_with_settings(db, options, None).await
}

/// Like `search_atoms` but with externally-provided settings (from registry).
pub async fn search_atoms_with_settings(
    db: &Database,
    options: SearchOptions,
    external_settings: Option<HashMap<String, String>>,
) -> Result<Vec<SemanticSearchResult>, String> {
    // Get raw chunk results
    let chunks = search_chunks_with_settings(db, options.clone(), external_settings).await?;

    // Deduplicate by atom_id, keeping highest score per atom
    let mut atom_best: HashMap<String, ChunkResult> = HashMap::new();
    for chunk in chunks {
        let entry = atom_best.entry(chunk.atom_id.clone());
        match entry {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if chunk.score > e.get().score {
                    e.insert(chunk);
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(chunk);
            }
        }
    }

    // Sort by score descending and limit. The recency filter is pushed into the
    // underlying SQL queries, so no post-filtering is needed here.
    let mut deduped: Vec<ChunkResult> = atom_best.into_values().collect();
    deduped.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    deduped.truncate(options.limit as usize);

    // Batch fetch all atom data in one query
    let conn = db.read_conn().map_err(|e| e.to_string())?;

    let atom_ids: Vec<String> = deduped.iter().map(|c| c.atom_id.clone()).collect();
    let atom_map = batch_fetch_atoms(&conn, &atom_ids)?;
    let tag_map = batch_fetch_tags(&conn, &atom_ids)?;

    let mut results = Vec::with_capacity(deduped.len());
    for chunk in deduped {
        if let Some(atom) = atom_map.get(&chunk.atom_id) {
            let tags = tag_map.get(&chunk.atom_id).cloned().unwrap_or_default();
            results.push(SemanticSearchResult {
                atom: AtomWithTags {
                    atom: atom.clone(),
                    tags,
                },
                similarity_score: chunk.score,
                matching_chunk_content: chunk.content,
                matching_chunk_index: chunk.chunk_index,
                match_snippet: None,
                match_offsets: None,
                match_count: None,
            });
        }
    }

    Ok(results)
}

/// Compute an ISO 8601 UTC cutoff string for "N days ago".
/// created_at values are stored as ISO 8601 (with 'Z' or offset), which sort lexicographically.
pub fn since_days_cutoff(since_days: i32) -> String {
    let days = since_days.max(0) as i64;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
    cutoff.to_rfc3339()
}

/// Batch fetch atoms by IDs in a single query
fn batch_fetch_atoms(
    conn: &rusqlite::Connection,
    atom_ids: &[String],
) -> Result<HashMap<String, Atom>, String> {
    if atom_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT {} FROM atoms WHERE id IN ({})",
        crate::ATOM_COLUMNS,
        placeholders
    );
    let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(atom_ids.iter()),
            crate::atom_from_row,
        )
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    Ok(rows.into_iter().map(|a| (a.id.clone(), a)).collect())
}

/// Batch fetch tags for multiple atoms in a single query
fn batch_fetch_tags(
    conn: &rusqlite::Connection,
    atom_ids: &[String],
) -> Result<HashMap<String, Vec<Tag>>, String> {
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
    let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
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
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (atom_id, tag) = row.map_err(|e| e.to_string())?;
        map.entry(atom_id).or_default().push(tag);
    }
    Ok(map)
}

/// Batch fetch chunk info by IDs in a single query
fn batch_fetch_chunk_info(
    conn: &rusqlite::Connection,
    chunk_ids: &[String],
) -> Result<HashMap<String, (String, String, i32)>, String> {
    if chunk_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT id, atom_id, content, chunk_index FROM atom_chunks WHERE id IN ({})",
        placeholders
    );
    let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(chunk_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i32>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    Ok(rows
        .into_iter()
        .map(|(id, atom_id, content, idx)| (id, (atom_id, content, idx)))
        .collect())
}

/// Keyword search using FTS5/BM25
async fn search_keyword_chunks(
    db: &Database,
    options: &SearchOptions,
) -> Result<Vec<ChunkResult>, String> {
    let conn = db.read_conn().map_err(|e| e.to_string())?;

    let escaped_query = escape_fts5_query(&options.query);
    let fetch_limit = options.limit * 5; // Fetch extra for dedup/scope

    let cutoff = options.since_days.map(since_days_cutoff);

    let row_map = |row: &rusqlite::Row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i32>(3)?,
            row.get::<_, f64>(4)?,
        ))
    };

    let raw_results: Vec<(String, String, String, i32, f64)> = if let Some(ref c) = cutoff {
        let mut stmt = conn
            .prepare(
                "SELECT f.id, f.atom_id, f.content, f.chunk_index, bm25(atom_chunks_fts) AS score
                 FROM atom_chunks_fts f
                 INNER JOIN atoms a ON a.id = f.atom_id
                 WHERE atom_chunks_fts MATCH ?1 AND a.created_at >= ?2
                 ORDER BY bm25(atom_chunks_fts)
                 LIMIT ?3",
            )
            .map_err(|e| format!("Failed to prepare FTS query: {}", e))?;
        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![&escaped_query, c, fetch_limit], row_map)
            .map_err(|e| format!("Failed to query FTS: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect FTS results: {}", e))?;
        rows
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT id, atom_id, content, chunk_index, bm25(atom_chunks_fts) AS score
                 FROM atom_chunks_fts
                 WHERE atom_chunks_fts MATCH ?1
                 ORDER BY bm25(atom_chunks_fts)
                 LIMIT ?2",
            )
            .map_err(|e| format!("Failed to prepare FTS query: {}", e))?;
        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![&escaped_query, fetch_limit], row_map)
            .map_err(|e| format!("Failed to query FTS: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect FTS results: {}", e))?;
        rows
    };

    // Apply tag scope filter if specified
    let filtered = filter_by_scope(&conn, raw_results, &options.scope_tag_ids)?;
    // Apply atom-kind filter (e.g. exclude report findings on external paths).
    let filtered = filter_by_kind(&conn, filtered, &options.kinds)?;

    // Convert to ChunkResult with normalized scores
    Ok(filtered
        .into_iter()
        .map(
            |(chunk_id, atom_id, content, chunk_index, bm25_score)| ChunkResult {
                chunk_id,
                atom_id,
                content,
                chunk_index,
                score: normalize_bm25_score(bm25_score),
            },
        )
        .collect())
}

/// Semantic search using vector similarity
async fn search_semantic_chunks(
    db: &Database,
    options: &SearchOptions,
    external_settings: Option<HashMap<String, String>>,
) -> Result<Vec<ChunkResult>, String> {
    // Get provider config from settings
    let provider_config = {
        let settings_map = match external_settings {
            Some(s) => s,
            None => {
                let conn = db.read_conn().map_err(|e| e.to_string())?;
                get_all_settings(&conn).map_err(|e| e.to_string())?
            }
        };
        ProviderConfig::from_settings(&settings_map)
    };

    // Create embedding provider and generate query embedding
    let provider = get_embedding_provider(&provider_config)
        .map_err(|e| format!("Failed to create embedding provider: {}", e))?;
    let embedding_config = EmbeddingConfig::new(provider_config.embedding_model());
    let embeddings = provider
        .embed_batch(&[options.query.clone()], &embedding_config)
        .await
        .map_err(|e| format!("Failed to generate query embedding: {}", e))?;

    let query_blob = f32_vec_to_blob_public(&embeddings[0]);

    // Query vec_chunks
    let conn = db.read_conn().map_err(|e| e.to_string())?;
    let fetch_limit = options.limit * 10;

    let cutoff = options.since_days.map(since_days_cutoff);
    let row_map = |row: &rusqlite::Row| Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?));

    // sqlite-vec requires MATCH to be the sole constraint on vec_chunks, so the
    // cutoff is applied in an outer JOIN over the top-k by-distance set.
    let similar_chunks: Vec<(String, f32)> = if let Some(ref c) = cutoff {
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
                 INNER JOIN atom_chunks ac ON ac.id = v.chunk_id
                 INNER JOIN atoms a ON a.id = ac.atom_id
                 WHERE a.created_at >= ?3
                 ORDER BY v.distance
                 LIMIT ?4",
            )
            .map_err(|e| format!("Failed to prepare vec query: {}", e))?;
        let rows: Vec<_> = stmt
            .query_map(
                rusqlite::params![&query_blob, knn_limit, c, fetch_limit],
                row_map,
            )
            .map_err(|e| format!("Failed to query similar chunks: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect similar chunks: {}", e))?;
        rows
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT chunk_id, distance
                 FROM vec_chunks
                 WHERE embedding MATCH ?1
                 ORDER BY distance
                 LIMIT ?2",
            )
            .map_err(|e| format!("Failed to prepare vec query: {}", e))?;
        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![&query_blob, fetch_limit], row_map)
            .map_err(|e| format!("Failed to query similar chunks: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect similar chunks: {}", e))?;
        rows
    };

    // Filter by threshold first, then batch-load chunk details
    let filtered: Vec<(String, f32)> = similar_chunks
        .into_iter()
        .filter(|(_, distance)| distance_to_similarity(*distance) >= options.threshold)
        .collect();

    let chunk_ids: Vec<String> = filtered.iter().map(|(id, _)| id.clone()).collect();
    let chunk_map = batch_fetch_chunk_info(&conn, &chunk_ids)?;

    // Pre-compute scope filter for all candidate atom_ids in one batch query
    let scope_atom_ids: std::collections::HashSet<String> = if !options.scope_tag_ids.is_empty() {
        let candidate_atom_ids: Vec<&str> =
            chunk_map.values().map(|(aid, _, _)| aid.as_str()).collect();
        batch_atoms_with_scope_tags(&conn, &candidate_atom_ids, &options.scope_tag_ids)?
    } else {
        std::collections::HashSet::new()
    };
    // Same shape for the kind filter: one batch lookup, then a per-chunk
    // gate in the loop below. `None` means `KindFilter::All` (no filtering).
    let kind_allowed_atom_ids = batch_atom_ids_matching_kind(
        &conn,
        chunk_map.values().map(|(aid, _, _)| aid.as_str()),
        &options.kinds,
    )?;

    let mut results = Vec::new();
    for (chunk_id, distance) in filtered {
        let similarity = distance_to_similarity(distance);
        if let Some((atom_id, content, chunk_index)) = chunk_map.get(&chunk_id) {
            if !options.scope_tag_ids.is_empty() && !scope_atom_ids.contains(atom_id) {
                continue;
            }
            if let Some(ref allowed) = kind_allowed_atom_ids {
                if !allowed.contains(atom_id) {
                    continue;
                }
            }
            results.push(ChunkResult {
                chunk_id,
                atom_id: atom_id.clone(),
                content: content.clone(),
                chunk_index: *chunk_index,
                score: similarity,
            });
        }
    }

    Ok(results)
}

/// Hybrid search combining keyword and semantic with RRF
async fn search_hybrid_chunks(
    db: &Database,
    options: &SearchOptions,
    external_settings: Option<HashMap<String, String>>,
) -> Result<Vec<ChunkResult>, String> {
    // Run keyword and semantic searches in parallel — keyword is pure DB,
    // semantic includes an embedding API call, so overlapping saves significant time.
    let (keyword_results, semantic_results) = tokio::join!(
        search_keyword_chunks(db, options),
        search_semantic_chunks(db, options, external_settings),
    );
    let keyword_results = keyword_results?;
    let semantic_results = semantic_results?;

    // Combine with Reciprocal Rank Fusion
    // RRF score = sum of 1/(k + rank) across result sets
    let mut chunk_scores: HashMap<String, (f32, String, String, i32)> = HashMap::new();

    for (rank, chunk) in keyword_results.iter().enumerate() {
        let rrf = 1.0 / (RRF_K + (rank + 1) as f32);
        chunk_scores.insert(
            chunk.chunk_id.clone(),
            (
                rrf,
                chunk.atom_id.clone(),
                chunk.content.clone(),
                chunk.chunk_index,
            ),
        );
    }

    for (rank, chunk) in semantic_results.iter().enumerate() {
        let rrf = 1.0 / (RRF_K + (rank + 1) as f32);
        chunk_scores
            .entry(chunk.chunk_id.clone())
            .and_modify(|(score, _, _, _)| *score += rrf)
            .or_insert((
                rrf,
                chunk.atom_id.clone(),
                chunk.content.clone(),
                chunk.chunk_index,
            ));
    }

    // Sort by RRF score and normalize to 0-1
    let max_rrf = 2.0 / (RRF_K + 1.0);
    let mut combined: Vec<ChunkResult> = chunk_scores
        .into_iter()
        .map(
            |(chunk_id, (score, atom_id, content, chunk_index))| ChunkResult {
                chunk_id,
                atom_id,
                content,
                chunk_index,
                score: (score / max_rrf).clamp(0.0, 1.0),
            },
        )
        .collect();

    combined.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    combined.truncate(options.limit as usize);

    Ok(combined)
}

/// Filter results by tag scope (for keyword search batch filtering)
fn filter_by_scope<T>(
    conn: &rusqlite::Connection,
    results: Vec<(String, String, String, i32, T)>,
    scope_tag_ids: &[String],
) -> Result<Vec<(String, String, String, i32, T)>, String> {
    if scope_tag_ids.is_empty() {
        return Ok(results);
    }

    // Batch check: get all atom_ids that have at least one scope tag
    let atom_ids: Vec<&str> = results.iter().map(|r| r.1.as_str()).collect();
    let matching_atom_ids = batch_atoms_with_scope_tags(conn, &atom_ids, scope_tag_ids)?;

    let filtered = results
        .into_iter()
        .filter(|r| matching_atom_ids.contains(r.1.as_str()))
        .collect();
    Ok(filtered)
}

/// Filter results by atom-kind discriminator. Mirrors `filter_by_scope` in
/// shape — one batch lookup against the atoms table, then a `retain`. The
/// FTS / vec virtual tables are kind-blind by design (kind isn't part of
/// the index), so the filter lives in the post-pass rather than the
/// underlying SQL. `KindFilter::All` short-circuits.
fn filter_by_kind<T>(
    conn: &rusqlite::Connection,
    results: Vec<(String, String, String, i32, T)>,
    kinds: &KindFilter,
) -> Result<Vec<(String, String, String, i32, T)>, String> {
    let allowed = batch_atom_ids_matching_kind(conn, results.iter().map(|r| r.1.as_str()), kinds)?;
    let Some(allowed) = allowed else {
        // KindFilter::All — no filtering.
        return Ok(results);
    };
    Ok(results
        .into_iter()
        .filter(|r| allowed.contains(r.1.as_str()))
        .collect())
}

/// Resolve the set of atom_ids in `candidates` whose `kind` is within the
/// filter. Returns `None` for `KindFilter::All` so callers can skip the
/// allocation; returns an empty set for `KindFilter::Only(vec![])`
/// (defensive — matches the empty-only short-circuit elsewhere in the
/// codebase).
fn batch_atom_ids_matching_kind<'a>(
    conn: &rusqlite::Connection,
    candidates: impl Iterator<Item = &'a str>,
    kinds: &KindFilter,
) -> Result<Option<std::collections::HashSet<String>>, String> {
    let kinds = match kinds {
        KindFilter::All => return Ok(None),
        KindFilter::Only(ks) => ks,
    };
    if kinds.is_empty() {
        return Ok(Some(std::collections::HashSet::new()));
    }
    let candidate_ids: Vec<&str> = candidates.collect();
    if candidate_ids.is_empty() {
        return Ok(Some(std::collections::HashSet::new()));
    }

    let atom_placeholders: Vec<&str> = candidate_ids.iter().map(|_| "?").collect();
    let kind_placeholders: Vec<&str> = kinds.iter().map(|_| "?").collect();
    let query = format!(
        "SELECT id FROM atoms WHERE id IN ({}) AND kind IN ({})",
        atom_placeholders.join(","),
        kind_placeholders.join(","),
    );

    let kind_strs: Vec<&'static str> = kinds.iter().map(|k| k.as_str()).collect();
    let mut params: Vec<&dyn rusqlite::ToSql> =
        Vec::with_capacity(candidate_ids.len() + kind_strs.len());
    for id in &candidate_ids {
        params.push(id);
    }
    for k in &kind_strs {
        params.push(k);
    }

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare kind query: {}", e))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| format!("Failed to execute kind query: {}", e))?;

    let mut allowed = std::collections::HashSet::new();
    for row in rows {
        allowed.insert(row.map_err(|e| format!("Failed to read kind result: {}", e))?);
    }
    Ok(Some(allowed))
}

/// Batch check which atom_ids have at least one of the specified scope tags.
/// Returns the set of matching atom_ids.
fn batch_atoms_with_scope_tags(
    conn: &rusqlite::Connection,
    atom_ids: &[&str],
    scope_tag_ids: &[String],
) -> Result<std::collections::HashSet<String>, String> {
    if atom_ids.is_empty() || scope_tag_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    let atom_placeholders: Vec<&str> = atom_ids.iter().map(|_| "?").collect();
    let tag_placeholders: Vec<&str> = scope_tag_ids.iter().map(|_| "?").collect();
    let query = format!(
        "SELECT DISTINCT atom_id FROM atom_tags WHERE atom_id IN ({}) AND tag_id IN ({})",
        atom_placeholders.join(","),
        tag_placeholders.join(","),
    );

    let mut params: Vec<&dyn rusqlite::ToSql> =
        Vec::with_capacity(atom_ids.len() + scope_tag_ids.len());
    for id in atom_ids {
        params.push(id);
    }
    for id in scope_tag_ids {
        params.push(id);
    }

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare scope query: {}", e))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| format!("Failed to execute scope query: {}", e))?;

    let mut matching = std::collections::HashSet::new();
    for row in rows {
        matching.insert(row.map_err(|e| format!("Failed to read scope result: {}", e))?);
    }
    Ok(matching)
}

/// Find atoms similar to a given atom based on embedding similarity
///
/// 1. Get all chunks for the given atom
/// 2. For each chunk, find similar chunks in vec_chunks
/// 3. Filter by threshold (convert distance to similarity)
/// 4. Deduplicate by parent atom_id, keep highest similarity
/// 5. Exclude the source atom itself
/// 6. Return up to `limit` results
pub fn find_similar_atoms(
    conn: &rusqlite::Connection,
    atom_id: &str,
    limit: i32,
    threshold: f32,
) -> Result<Vec<SimilarAtomResult>, String> {
    use crate::embedding::distance_to_similarity;
    use std::collections::hash_map::Entry;

    // 1. Get all chunks for the given atom
    let mut stmt = conn
        .prepare("SELECT id, embedding FROM atom_chunks WHERE atom_id = ?1")
        .map_err(|e| format!("Failed to prepare chunk query: {}", e))?;

    let source_chunks: Vec<(String, Vec<u8>)> = stmt
        .query_map([&atom_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| format!("Failed to query chunks: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect chunks: {}", e))?;

    if source_chunks.is_empty() {
        return Ok(Vec::new());
    }

    // Map to store best similarity per atom_id
    let mut atom_similarities: HashMap<String, (f32, String, i32)> = HashMap::new();

    // 2. For each source chunk, find similar chunks
    for (_, embedding_blob) in &source_chunks {
        let mut vec_stmt = conn
            .prepare(
                "SELECT chunk_id, distance
                 FROM vec_chunks
                 WHERE embedding MATCH ?1
                 ORDER BY distance
                 LIMIT ?2",
            )
            .map_err(|e| format!("Failed to prepare vec query: {}", e))?;

        let similar_chunks: Vec<(String, f32)> = vec_stmt
            .query_map(rusqlite::params![embedding_blob, limit * 10], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|e| format!("Failed to query similar chunks: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect similar chunks: {}", e))?;

        // 3. Filter by threshold, then batch-fetch chunk info
        let filtered: Vec<(String, f32)> = similar_chunks
            .into_iter()
            .filter(|(_, distance)| distance_to_similarity(*distance) >= threshold)
            .collect();

        let chunk_ids: Vec<String> = filtered.iter().map(|(id, _)| id.clone()).collect();
        let chunk_map = batch_fetch_chunk_info(conn, &chunk_ids)
            .map_err(|e| format!("Failed to batch fetch chunks: {}", e))?;

        for (chunk_id, distance) in filtered {
            let similarity = distance_to_similarity(distance);

            if let Some((parent_atom_id, chunk_content, chunk_index)) = chunk_map.get(&chunk_id) {
                // 5. Exclude the source atom itself
                if parent_atom_id == atom_id {
                    continue;
                }

                // 4. Keep highest similarity per atom
                match atom_similarities.entry(parent_atom_id.clone()) {
                    Entry::Occupied(mut e) => {
                        if similarity > e.get().0 {
                            e.insert((similarity, chunk_content.clone(), *chunk_index));
                        }
                    }
                    Entry::Vacant(e) => {
                        e.insert((similarity, chunk_content.clone(), *chunk_index));
                    }
                }
            }
        }
    }

    // 6. Build results, sorted by similarity
    let mut results: Vec<(String, f32, String, i32)> = atom_similarities
        .into_iter()
        .map(|(id, (sim, content, idx))| (id, sim, content, idx))
        .collect();

    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit as usize);

    // Batch fetch atom data for results
    let result_atom_ids: Vec<String> = results.iter().map(|(id, _, _, _)| id.clone()).collect();
    let atom_map = batch_fetch_atoms(conn, &result_atom_ids)
        .map_err(|e| format!("Failed to batch fetch atoms: {}", e))?;

    let mut final_results = Vec::new();
    for (result_atom_id, similarity, chunk_content, chunk_index) in results {
        if let Some(atom) = atom_map.get(&result_atom_id) {
            final_results.push(SimilarAtomResult {
                atom: AtomWithTags {
                    atom: atom.clone(),
                    tags: vec![],
                },
                similarity_score: similarity,
                matching_chunk_content: chunk_content,
                matching_chunk_index: chunk_index,
            });
        }
    }

    Ok(final_results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use tempfile::NamedTempFile;

    fn create_test_db() -> (Database, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();
        (db, temp_file)
    }

    // ==================== Pure Function Tests ====================

    #[test]
    fn test_escape_fts5_query_basic() {
        let result = escape_fts5_query("hello world");
        assert_eq!(result, "\"hello\" \"world\"");
    }

    #[test]
    fn test_escape_fts5_query_special_chars() {
        // Quotes should be removed/escaped
        let result = escape_fts5_query("hello \"world\"");
        assert_eq!(result, "\"hello\" \"world\"");
    }

    #[test]
    fn test_escape_fts5_query_empty() {
        let result = escape_fts5_query("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_normalize_bm25_score_range() {
        // BM25 scores are negative, lower (more negative) = worse match
        let best = normalize_bm25_score(0.0); // Best match
        let mid = normalize_bm25_score(-15.0); // Medium match
        let worst = normalize_bm25_score(-30.0); // Worst match

        // Best should be highest, worst should be lowest
        assert!(best > mid, "best {} > mid {}", best, mid);
        assert!(mid > worst, "mid {} > worst {}", mid, worst);

        // All should be in 0-1 range
        assert!(best >= 0.0 && best <= 1.0);
        assert!(mid >= 0.0 && mid <= 1.0);
        assert!(worst >= 0.0 && worst <= 1.0);
    }

    // ==================== SearchOptions Tests ====================

    #[test]
    fn test_search_options_defaults() {
        let options = SearchOptions::default();

        assert_eq!(options.query, "");
        assert_eq!(options.mode, SearchMode::Hybrid);
        assert_eq!(options.limit, 10);
        assert_eq!(options.threshold, 0.3);
        assert!(options.scope_tag_ids.is_empty());
    }

    #[test]
    fn test_search_options_builder() {
        let options = SearchOptions::new("test query", SearchMode::Semantic, 20)
            .with_threshold(0.5)
            .with_scope(vec!["tag1".to_string()]);

        assert_eq!(options.query, "test query");
        assert_eq!(options.mode, SearchMode::Semantic);
        assert_eq!(options.limit, 20);
        assert_eq!(options.threshold, 0.5);
        assert_eq!(options.scope_tag_ids, vec!["tag1".to_string()]);
    }

    // ==================== find_similar_atoms Tests ====================

    #[test]
    fn test_find_similar_empty_atom() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        // Create an atom with no chunks
        let atom_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO atoms (id, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![&atom_id, "test content", &now, &now],
        )
        .unwrap();

        // Find similar should return empty (no chunks to compare)
        let results = find_similar_atoms(&conn, &atom_id, 10, 0.5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_find_similar_excludes_source() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();

        // Create source atom
        let source_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO atoms (id, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![&source_id, "source content", &now, &now],
        )
        .unwrap();

        // Create source chunk with embedding
        let chunk_id = uuid::Uuid::new_v4().to_string();
        let embedding: Vec<f32> = vec![0.1; 1536];
        let blob: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

        conn.execute(
            "INSERT INTO atom_chunks (id, atom_id, chunk_index, content, embedding) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![&chunk_id, &source_id, 0, "chunk content", &blob],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![&chunk_id, &blob],
        )
        .unwrap();

        // Results should never include the source atom itself
        let results = find_similar_atoms(&conn, &source_id, 10, 0.0).unwrap();

        // Even with zero threshold, source shouldn't be in results
        for result in &results {
            assert_ne!(result.atom.atom.id, source_id);
        }
    }

    #[test]
    fn test_find_similar_threshold_filtering() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();

        // Create source atom with chunk
        let source_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO atoms (id, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![&source_id, "source", &now, &now],
        )
        .unwrap();

        let source_chunk_id = uuid::Uuid::new_v4().to_string();
        let embedding: Vec<f32> = vec![1.0; 1536]; // Unit vector
        let blob: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

        conn.execute(
            "INSERT INTO atom_chunks (id, atom_id, chunk_index, content, embedding) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![&source_chunk_id, &source_id, 0, "source chunk", &blob],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![&source_chunk_id, &blob],
        )
        .unwrap();

        // Create a target atom with very different embedding
        let target_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO atoms (id, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![&target_id, "target", &now, &now],
        )
        .unwrap();

        let target_chunk_id = uuid::Uuid::new_v4().to_string();
        let diff_embedding: Vec<f32> = vec![-1.0; 1536]; // Opposite vector
        let diff_blob: Vec<u8> = diff_embedding
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();

        conn.execute(
            "INSERT INTO atom_chunks (id, atom_id, chunk_index, content, embedding) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![&target_chunk_id, &target_id, 0, "target chunk", &diff_blob],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
            rusqlite::params![&target_chunk_id, &diff_blob],
        )
        .unwrap();

        // With very high threshold, should find nothing (vectors are opposite)
        let results = find_similar_atoms(&conn, &source_id, 10, 0.99).unwrap();
        assert!(
            results.is_empty(),
            "High threshold should filter out dissimilar atoms"
        );
    }
}
