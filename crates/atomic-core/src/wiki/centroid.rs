//! Centroid-based wiki generation strategy
//!
//! Uses the tag's centroid embedding to rank chunks by semantic relevance,
//! then generates/updates articles via a single-shot LLM call.

use crate::chunking::count_tokens;
use crate::embedding::distance_to_similarity;
use crate::models::{ChunkWithContext, WikiArticleWithCitations};
use crate::providers::ProviderConfig;

use rusqlite::Connection;

use super::{batch_fetch_chunk_details, synthesize_article, WikiStrategyContext};

/// Data needed for wiki article generation (extracted before async call)
pub struct WikiGenerationInput {
    pub chunks: Vec<ChunkWithContext>,
    pub atom_count: i32,
    pub tag_id: String,
    pub tag_name: String,
}



/// Generate a wiki article using centroid-based chunk selection + single-shot LLM.
pub(crate) async fn generate(
    ctx: &WikiStrategyContext,
) -> Result<WikiArticleWithCitations, String> {
    let max_tokens = ctx.max_source_tokens();
    tracing::info!(
        budget_tokens = max_tokens,
        "[wiki/centroid] Preparing sources (centroid similarity)"
    );

    let (chunks, atom_count) = ctx
        .storage
        .get_wiki_source_chunks_sync(&ctx.tag_id, max_tokens)
        .await
        .map_err(|e| e.to_string())?;

    let input = WikiGenerationInput {
        chunks,
        atom_count,
        tag_id: ctx.tag_id.clone(),
        tag_name: ctx.tag_name.clone(),
    };

    tracing::info!(
        chunks = input.chunks.len(),
        atoms = input.atom_count,
        "[wiki/centroid] Found chunks"
    );

    tracing::info!("[wiki/centroid] Calling LLM...");
    let result = generate_wiki_content(
        &ctx.provider_config,
        &input,
        &ctx.wiki_model,
        &ctx.linkable_article_names,
        ctx.generation_prompt(),
    )
    .await?;

    Ok(result)
}



/// Select chunks ranked by similarity to the tag centroid, up to the token budget.
pub(crate) fn select_chunks_by_centroid(
    conn: &Connection,
    centroid_blob: &[u8],
    scoped_atom_ids: &std::collections::HashSet<String>,
    max_source_tokens: usize,
) -> Result<Vec<ChunkWithContext>, String> {
    // Fetch more than we need from vec_chunks since we'll filter by scope.
    // Over-fetch by 3x to account for chunks outside the tag hierarchy.
    let fetch_limit = 3000_i32;

    let mut vec_stmt = conn
        .prepare(
            "SELECT chunk_id, distance
         FROM vec_chunks
         WHERE embedding MATCH ?1
         ORDER BY distance
         LIMIT ?2",
        )
        .map_err(|e| format!("Failed to prepare vec_chunks query: {}", e))?;

    let candidates: Vec<(String, f32)> = vec_stmt
        .query_map(rusqlite::params![centroid_blob, fetch_limit], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(|e| format!("Failed to query vec_chunks: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect vec_chunks: {}", e))?;

    // Batch-load chunk details for all candidates
    let chunk_ids: Vec<&str> = candidates.iter().map(|(id, _)| id.as_str()).collect();
    let chunk_details = batch_fetch_chunk_details(conn, &chunk_ids)?;

    // Filter to scoped atoms and fill token budget
    let mut chunks = Vec::new();
    let mut total_tokens = 0;

    for (chunk_id, distance) in &candidates {
        if let Some((atom_id, chunk_index, content)) = chunk_details.get(chunk_id.as_str()) {
            if !scoped_atom_ids.contains(atom_id) {
                continue;
            }
            let tokens = count_tokens(content);
            if total_tokens + tokens > max_source_tokens && !chunks.is_empty() {
                break;
            }
            total_tokens += tokens;
            chunks.push(ChunkWithContext {
                atom_id: atom_id.clone(),
                chunk_index: *chunk_index,
                content: content.clone(),
                similarity_score: distance_to_similarity(*distance),
            });
        }
    }

    tracing::info!(
        chunks = chunks.len(),
        tokens = total_tokens,
        "[wiki/centroid] Selected chunks by centroid similarity"
    );

    Ok(chunks)
}

/// Fallback: select chunks by insertion order up to the token budget.
///
/// Takes the pre-resolved `scoped_atom_ids` set (same input as the centroid
/// path) so kind / tag scoping live in exactly one place — the caller's
/// scope-resolution query — and cannot drift between the ranked and
/// unranked paths.
pub(crate) fn select_chunks_unranked(
    conn: &Connection,
    scoped_atom_ids: &std::collections::HashSet<String>,
    max_source_tokens: usize,
) -> Result<Vec<ChunkWithContext>, String> {
    if scoped_atom_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = scoped_atom_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT atom_id, chunk_index, content FROM atom_chunks
         WHERE atom_id IN ({})
         ORDER BY atom_id, chunk_index",
        placeholders
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare chunks query: {}", e))?;

    let params: Vec<&str> = scoped_atom_ids.iter().map(|s| s.as_str()).collect();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| format!("Failed to query chunks: {}", e))?;

    let mut chunks = Vec::new();
    let mut total_tokens = 0;

    while let Some(row) = rows
        .next()
        .map_err(|e| format!("Failed to read row: {}", e))?
    {
        let content: String = row
            .get(2)
            .map_err(|e| format!("Failed to get content: {}", e))?;
        let tokens = count_tokens(&content);
        if total_tokens + tokens > max_source_tokens && !chunks.is_empty() {
            break;
        }
        total_tokens += tokens;
        chunks.push(ChunkWithContext {
            atom_id: row
                .get(0)
                .map_err(|e| format!("Failed to get atom_id: {}", e))?,
            chunk_index: row
                .get(1)
                .map_err(|e| format!("Failed to get chunk_index: {}", e))?,
            content,
            similarity_score: 1.0,
        });
    }

    tracing::info!(
        chunks = chunks.len(),
        tokens = total_tokens,
        "[wiki/centroid] Selected chunks by insertion order (no centroid)"
    );

    Ok(chunks)
}

/// Generate wiki article content via shared synthesis (async, no db needed)
async fn generate_wiki_content(
    provider_config: &ProviderConfig,
    input: &WikiGenerationInput,
    model: &str,
    existing_article_names: &[(String, String)],
    system_prompt: &str,
) -> Result<WikiArticleWithCitations, String> {
    synthesize_article(
        provider_config,
        &input.tag_id,
        &input.tag_name,
        &input.chunks,
        input.atom_count,
        model,
        existing_article_names,
        system_prompt,
    )
    .await
}


