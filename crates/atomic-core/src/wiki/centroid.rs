//! Centroid-based wiki generation strategy
//!
//! Uses the tag's centroid embedding to rank chunks by semantic relevance,
//! then generates/updates articles via a single-shot LLM call.

use crate::chunking::count_tokens;
use crate::embedding::distance_to_similarity;
use crate::models::{ChunkWithContext, WikiArticle, WikiArticleWithCitations, WikiCitation};
use crate::providers::ProviderConfig;

use chrono::Utc;
use rusqlite::Connection;

use super::{
    batch_fetch_chunk_details, call_llm_for_wiki, extract_citations, synthesize_article,
    WikiStrategyContext,
};

/// Data needed for wiki article generation (extracted before async call)
pub struct WikiGenerationInput {
    pub chunks: Vec<ChunkWithContext>,
    pub atom_count: i32,
    pub tag_id: String,
    pub tag_name: String,
}

/// Data needed for wiki article update (extracted before async call)
pub struct WikiUpdateInput {
    pub new_chunks: Vec<ChunkWithContext>,
    pub existing_article: WikiArticle,
    pub existing_citations: Vec<WikiCitation>,
    pub atom_count: i32,
    pub tag_id: String,
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

/// Update an existing wiki article with new content using centroid strategy.
/// Returns None if no new content is available.
pub(crate) async fn update(
    ctx: &WikiStrategyContext,
    existing: &WikiArticleWithCitations,
) -> Result<Option<WikiArticleWithCitations>, String> {
    let max_tokens = ctx.max_source_tokens();

    let update_data = ctx
        .storage
        .get_wiki_update_chunks_sync(&ctx.tag_id, &existing.article.updated_at, max_tokens)
        .await
        .map_err(|e| e.to_string())?;

    let (new_chunks, atom_count) = match update_data {
        Some(data) => data,
        None => return Ok(None),
    };

    tracing::info!(new_chunks = new_chunks.len(), "[wiki/centroid] Update");

    let input = WikiUpdateInput {
        new_chunks,
        existing_article: existing.article.clone(),
        existing_citations: existing.citations.clone(),
        atom_count,
        tag_id: ctx.tag_id.clone(),
    };

    let result = update_wiki_content(
        &ctx.provider_config,
        &input,
        &ctx.wiki_model,
        &ctx.linkable_article_names,
        ctx.update_prompt(),
    )
    .await?;

    Ok(Some(result))
}

/// Select chunks ranked by similarity to the tag centroid, up to the token budget.
pub(crate) fn select_chunks_by_centroid(
    conn: &Connection,
    centroid_blob: &[u8],
    scoped_atom_ids: &std::collections::HashSet<String>,
    max_source_tokens: usize,
    excluded_tag_ids: &[String],
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
    let chunk_details = super::batch_fetch_chunk_details_excluding_tags(conn, &chunk_ids, excluded_tag_ids)?;

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
pub(crate) fn select_chunks_unranked(
    conn: &Connection,
    placeholders: &str,
    all_tag_ids: &[String],
    max_source_tokens: usize,
) -> Result<Vec<ChunkWithContext>, String> {
    let query = format!(
        "SELECT DISTINCT ac.atom_id, ac.chunk_index, ac.content
         FROM atom_chunks ac
         INNER JOIN atom_tags at ON ac.atom_id = at.atom_id
         WHERE at.tag_id IN ({})
         ORDER BY ac.atom_id, ac.chunk_index",
        placeholders
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare chunks query: {}", e))?;

    let mut rows = stmt
        .query(rusqlite::params_from_iter(all_tag_ids.iter()))
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

/// Fallback for update: select new chunks by insertion order up to the token budget.
pub(crate) fn select_new_chunks_unranked(
    conn: &Connection,
    new_atom_ids: &std::collections::HashSet<String>,
    max_source_tokens: usize,
) -> Result<Vec<ChunkWithContext>, String> {
    if new_atom_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = new_atom_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT atom_id, chunk_index, content FROM atom_chunks WHERE atom_id IN ({}) ORDER BY atom_id, chunk_index",
        placeholders
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare new chunks query: {}", e))?;

    let params: Vec<&str> = new_atom_ids.iter().map(|s| s.as_str()).collect();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| format!("Failed to query new chunks: {}", e))?;

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
        "[wiki/centroid] Selected new chunks by insertion order (no centroid)"
    );

    Ok(chunks)
}

/// Update wiki article content via API (async, no db needed)
async fn update_wiki_content(
    provider_config: &ProviderConfig,
    input: &WikiUpdateInput,
    model: &str,
    existing_article_names: &[(String, String)],
    system_prompt: &str,
) -> Result<WikiArticleWithCitations, String> {
    // Build existing sources section
    let mut existing_sources = String::new();
    for citation in &input.existing_citations {
        existing_sources.push_str(&format!(
            "[{}] {}\n\n",
            citation.citation_index, citation.excerpt
        ));
    }

    // Build new sources section (continuing numbering)
    let start_index = input.existing_citations.len() as i32 + 1;
    let mut new_sources = String::new();
    for (i, chunk) in input.new_chunks.iter().enumerate() {
        new_sources.push_str(&format!(
            "[{}] {}\n\n",
            start_index + i as i32,
            chunk.content
        ));
    }

    // Build existing articles list for cross-linking
    let articles_section = if existing_article_names.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = existing_article_names
            .iter()
            .filter(|(tid, _)| tid != &input.tag_id)
            .map(|(_, name)| name.as_str())
            .collect();
        if names.is_empty() {
            String::new()
        } else {
            format!(
                "\nEXISTING WIKI ARTICLES IN THIS KNOWLEDGE BASE:\n{}\n",
                names.join(", ")
            )
        }
    };

    let user_content = format!(
        "CURRENT ARTICLE:\n{}\n\nEXISTING SOURCES (already cited as [1] through [{}]):\n{}\nNEW SOURCES TO INCORPORATE (cite as [{}] onwards):\n{}{}\nUpdate the article to incorporate the new information.{}",
        input.existing_article.content,
        input.existing_citations.len(),
        existing_sources,
        start_index,
        new_sources,
        articles_section,
        if articles_section.is_empty() {
            ""
        } else {
            " Use [[Article Name]] to link to other articles listed above where relevant."
        }
    );

    // Call LLM API
    let result = call_llm_for_wiki(provider_config, system_prompt, &user_content, model).await?;

    // Create updated article
    let now = Utc::now().to_rfc3339();
    let article = WikiArticle {
        id: input.existing_article.id.clone(),
        tag_id: input.tag_id.clone(),
        content: result.article_content.clone(),
        created_at: input.existing_article.created_at.clone(),
        updated_at: now,
        atom_count: input.atom_count,
    };

    // Extract all citations from the updated content
    // Combine existing chunks with new chunks for citation mapping
    let mut all_chunks: Vec<ChunkWithContext> = input
        .existing_citations
        .iter()
        .map(|c| ChunkWithContext {
            atom_id: c.atom_id.clone(),
            chunk_index: c.chunk_index.unwrap_or(0),
            content: c.excerpt.clone(),
            similarity_score: 1.0,
        })
        .collect();
    all_chunks.extend(input.new_chunks.clone());

    let citations = extract_citations(&article.id, &result.article_content, &all_chunks)?;

    Ok(WikiArticleWithCitations { article, citations })
}
