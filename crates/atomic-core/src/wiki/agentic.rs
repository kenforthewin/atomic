//! Agentic wiki generation strategy
//!
//! Uses an AI agent with search tools to curate source material,
//! then synthesizes the article from the agent's selected chunks.

use crate::chunking::count_tokens;
use crate::models::{ChunkSearchResult, ChunkWithContext, WikiArticleWithCitations};
use crate::providers::types::{CompletionResponse, Message, ToolDefinition};
use crate::providers::{get_embedding_provider, get_llm_provider, LlmConfig};
use crate::storage::StorageBackend;

use std::collections::HashSet;

use super::{synthesize_article, WikiStrategyContext};

// ==================== Constants ====================

const MAX_RESEARCH_ITERATIONS: usize = 15;
const DEFAULT_SEARCH_LIMIT: i32 = 15;

// ==================== Research State ====================

struct ResearchContext {
    /// All chunks returned by searches, indexed by position (session ID = C{index+1})
    returned_chunks: Vec<ChunkSearchResult>,
    /// Set of chunk_ids already returned (for filtering future searches)
    seen_chunk_ids: HashSet<String>,
    /// Indices into returned_chunks that the agent selected
    selected_indices: HashSet<usize>,
    /// Conversation messages for the agent
    messages: Vec<Message>,
    /// Running token count of selected chunks
    selected_tokens: usize,
}

impl ResearchContext {
    fn new(system_prompt: String, user_prompt: String) -> Self {
        Self {
            returned_chunks: Vec::new(),
            seen_chunk_ids: HashSet::new(),
            selected_indices: HashSet::new(),
            messages: vec![Message::system(system_prompt), Message::user(user_prompt)],
            selected_tokens: 0,
        }
    }

    /// Get the selected chunks as ChunkWithContext for synthesis
    fn selected_chunks(&self) -> Vec<ChunkWithContext> {
        let mut chunks: Vec<(usize, ChunkWithContext)> = self
            .selected_indices
            .iter()
            .filter_map(|&idx| {
                self.returned_chunks.get(idx).map(|cr| {
                    (
                        idx,
                        ChunkWithContext {
                            atom_id: cr.atom_id.clone(),
                            chunk_index: cr.chunk_index,
                            content: cr.content.clone(),
                            similarity_score: cr.score,
                        },
                    )
                })
            })
            .collect();
        // Sort by selection order (index in returned_chunks)
        chunks.sort_by_key(|(idx, _)| *idx);
        chunks.into_iter().map(|(_, c)| c).collect()
    }
}

// ==================== Tool Definitions ====================

fn research_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "search",
            "Search the knowledge base for chunks relevant to the wiki article. \
             Use diverse queries to explore different facets of the topic. \
             Returns chunks with session IDs (C1, C2, ...) that you can select.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query to find relevant content"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 15, max: 30)",
                        "default": 15
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::new(
            "select",
            "Select specific chunks by their session IDs to include as source material. \
             Only select chunks that contain substantive, citable information for the article.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "chunk_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Session IDs of chunks to select, e.g. [\"C1\", \"C4\", \"C7\"]"
                    }
                },
                "required": ["chunk_ids"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::new(
            "done",
            "Signal that research is complete. Call this when you have gathered \
             comprehensive source material for the article.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
    ]
}

// ==================== Tool Handlers ====================

async fn handle_search(
    rc: &mut ResearchContext,
    storage: &StorageBackend,
    provider_config: &crate::providers::ProviderConfig,
    scope_tag_ids: &[String],
    args: &serde_json::Value,
) -> String {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_SEARCH_LIMIT as i64)
        .min(30) as i32;

    if query.is_empty() {
        return "Error: query is required".to_string();
    }

    tracing::debug!(query = %query, limit, "[wiki/agentic] search");

    // Wiki synthesis is captured-content-only: agent-emitted findings
    // (kind = 'report') must not become wiki source material, or the
    // encyclopedia ends up citing its own research journal.
    let kinds = crate::models::KindFilter::only(crate::models::AtomKind::Captured);

    // Perform hybrid search: keyword + vector, merged via RRF
    let keyword_results = match storage
        .keyword_search_chunks_sync(&query, limit * 2, scope_tag_ids, None, &kinds)
        .await
    {
        Ok(r) => r,
        Err(e) => return format!("Keyword search error: {}", e),
    };

    // Try to generate query embedding for vector search
    let vector_results = match get_embedding_provider(provider_config) {
        Ok(provider) => {
            let embed_config = provider_config.embedding_config();
            match provider.embed_batch(&[query.clone()], &embed_config).await {
                Ok(embeddings) if !embeddings.is_empty() && !embeddings[0].is_empty() => {
                    match storage
                        .vector_search_chunks_sync(
                            &embeddings[0],
                            limit * 2,
                            0.3,
                            scope_tag_ids,
                            None,
                            &kinds,
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(_) => Vec::new(),
                    }
                }
                _ => Vec::new(),
            }
        }
        Err(_) => Vec::new(),
    };

    // Merge results using RRF-like approach: convert to SemanticSearchResult temporarily is overkill,
    // just do a simple dedup by chunk_id with best score
    let mut combined: Vec<ChunkSearchResult> = Vec::new();
    let mut seen_in_merge: HashSet<String> = HashSet::new();

    // Interleave: take from both lists by rank
    let max_len = keyword_results.len().max(vector_results.len());
    for i in 0..max_len {
        if let Some(kr) = keyword_results.get(i) {
            if !seen_in_merge.contains(&kr.chunk_id) {
                seen_in_merge.insert(kr.chunk_id.clone());
                combined.push(kr.clone());
            }
        }
        if let Some(vr) = vector_results.get(i) {
            if !seen_in_merge.contains(&vr.chunk_id) {
                seen_in_merge.insert(vr.chunk_id.clone());
                combined.push(vr.clone());
            }
        }
    }
    combined.truncate(limit as usize);

    // Filter out already-seen chunks
    let new_results: Vec<ChunkSearchResult> = combined
        .into_iter()
        .filter(|r| !rc.seen_chunk_ids.contains(&r.chunk_id))
        .collect();

    if new_results.is_empty() {
        tracing::debug!("[wiki/agentic] 0 new results");
        return "No new results found for this query.".to_string();
    }

    let start_idx = rc.returned_chunks.len();
    tracing::debug!(
        count = new_results.len(),
        range_start = start_idx + 1,
        range_end = start_idx + new_results.len(),
        "[wiki/agentic] new results"
    );
    let mut output = String::new();

    for (i, chunk) in new_results.into_iter().enumerate() {
        let session_id = format!("C{}", start_idx + i + 1);
        output.push_str(&format!(
            "{} (score: {:.2}, atom: {})\n{}\n\n",
            session_id,
            chunk.score,
            &chunk.atom_id[..8.min(chunk.atom_id.len())],
            chunk.content
        ));
        rc.seen_chunk_ids.insert(chunk.chunk_id.clone());
        rc.returned_chunks.push(chunk);
    }

    output
}

fn handle_select(
    rc: &mut ResearchContext,
    args: &serde_json::Value,
    max_source_tokens: usize,
) -> String {
    let chunk_ids = match args.get("chunk_ids").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        None => return "Error: chunk_ids array is required".to_string(),
    };

    if chunk_ids.is_empty() {
        return "Error: chunk_ids must not be empty".to_string();
    }

    tracing::debug!(chunk_ids = %chunk_ids.join(", "), "[wiki/agentic] select");

    let mut newly_selected = 0;
    let mut errors = Vec::new();

    for id_str in &chunk_ids {
        // Parse "C3" → index 2
        let idx = match id_str
            .strip_prefix('C')
            .or_else(|| id_str.strip_prefix('c'))
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) if n >= 1 && n <= rc.returned_chunks.len() => n - 1,
            _ => {
                errors.push(format!("{} is not a valid session ID", id_str));
                continue;
            }
        };

        if rc.selected_indices.contains(&idx) {
            continue; // Already selected, skip silently
        }

        let tokens = count_tokens(&rc.returned_chunks[idx].content);
        rc.selected_indices.insert(idx);
        rc.selected_tokens += tokens;
        newly_selected += 1;
    }

    let mut msg = format!(
        "Selected {} new chunks (total selected: {}, ~{} tokens)",
        newly_selected,
        rc.selected_indices.len(),
        rc.selected_tokens
    );

    if !errors.is_empty() {
        msg.push_str(&format!("\nWarnings: {}", errors.join("; ")));
    }

    if rc.selected_tokens > max_source_tokens {
        msg.push_str(&format!(
            "\nNote: selected tokens ({}) exceed budget ({}). Consider being more selective.",
            rc.selected_tokens, max_source_tokens
        ));
    }

    msg
}

fn handle_done(rc: &ResearchContext) -> String {
    format!(
        "Research complete. Selected {} chunks (~{} tokens) from {} total returned.",
        rc.selected_indices.len(),
        rc.selected_tokens,
        rc.returned_chunks.len()
    )
}

// ==================== Research Loop ====================

async fn run_research(
    rc: &mut ResearchContext,
    storage: &StorageBackend,
    scope_tag_ids: &[String],
    provider_config: &crate::providers::ProviderConfig,
    model: &str,
    max_source_tokens: usize,
) -> Result<(), String> {
    let tools = research_tools();
    // Explicit output budget (see providers::structured::DEFAULT_MAX_OUTPUT_TOKENS
    // for why it must never be left unset on long-output calls).
    let llm_config = LlmConfig::new(model).with_params(
        crate::providers::types::GenerationParams::new()
            .with_max_tokens(crate::providers::structured::DEFAULT_MAX_OUTPUT_TOKENS),
    );
    let provider = get_llm_provider(provider_config).map_err(|e| e.to_string())?;

    for iteration in 0..MAX_RESEARCH_ITERATIONS {
        tracing::debug!(
            iteration = iteration + 1,
            max_iterations = MAX_RESEARCH_ITERATIONS,
            selected = rc.selected_indices.len(),
            tokens = rc.selected_tokens,
            "[wiki/agentic] Research iteration"
        );

        let response: CompletionResponse = provider
            .complete_with_tools(&rc.messages, &tools, &llm_config)
            .await
            .map_err(|e| format!("Research LLM call failed: {}", e))?;

        // Check for tool calls
        let tool_calls = match response.tool_calls {
            Some(ref tcs) if !tcs.is_empty() => tcs.clone(),
            _ => {
                // No tool calls — agent is done (or just sent text)
                if !response.content.is_empty() {
                    tracing::debug!(
                        "[wiki/agentic] Agent sent text without tools, ending research"
                    );
                }
                break;
            }
        };

        // Add assistant message with tool calls
        rc.messages.push(Message {
            role: crate::providers::types::MessageRole::Assistant,
            content: if response.content.is_empty() {
                None
            } else {
                Some(response.content.clone())
            },
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
            name: None,
        });

        let mut research_done = false;

        for tc in &tool_calls {
            let name = tc.get_name().unwrap_or("");
            let args: serde_json::Value = tc
                .get_arguments()
                .and_then(|a| serde_json::from_str(a).ok())
                .unwrap_or(serde_json::json!({}));

            let result = match name {
                "search" => handle_search(rc, storage, provider_config, scope_tag_ids, &args).await,
                "select" => handle_select(rc, &args, max_source_tokens),
                "done" => {
                    research_done = true;
                    handle_done(rc)
                }
                _ => format!("Unknown tool: {}", name),
            };

            rc.messages
                .push(Message::tool_result(tc.id.clone(), result));
        }

        if research_done {
            tracing::debug!("[wiki/agentic] Agent called done, ending research");
            break;
        }

        // Safety: stop if selected tokens exceed budget
        if rc.selected_tokens > max_source_tokens * 2 {
            tracing::warn!(
                selected_tokens = rc.selected_tokens,
                "[wiki/agentic] Selected tokens far exceed budget, stopping research"
            );
            break;
        }
    }

    Ok(())
}

// ==================== Budget Enforcement ====================

/// Trim selected chunks to fit within the token budget.
/// Takes chunks in order (agent's selection order) until the budget is hit.
fn trim_to_budget(
    chunks: Vec<ChunkWithContext>,
    max_source_tokens: usize,
) -> Vec<ChunkWithContext> {
    let mut total_tokens = 0;
    let mut trimmed = Vec::new();
    for chunk in chunks {
        let tokens = count_tokens(&chunk.content);
        if total_tokens + tokens > max_source_tokens && !trimmed.is_empty() {
            break;
        }
        total_tokens += tokens;
        trimmed.push(chunk);
    }
    trimmed
}

// ==================== Research Prompts ====================

fn research_system_prompt(tag_name: &str) -> String {
    format!(
        r#"You are a research agent curating source material for a wiki article about "{tag_name}".

Your job is to search the knowledge base, review results, and select the best chunks to use as sources for the article. You have three tools:

- **search(query, limit)**: Search for relevant content. Use diverse queries to explore different facets of the topic. Each search returns new results with session IDs (C1, C2, ...). Previously returned chunks are automatically filtered out.
- **select(chunk_ids)**: Accept specific chunks as source material by their session IDs (e.g. ["C1", "C4", "C7"]). Only select chunks that contain substantive, citable information.
- **done()**: Signal that you have gathered comprehensive source material.

Guidelines:
- Make at least 3-5 searches with different angles/queries to get broad coverage
- Review each result critically — select only chunks with substantive, unique information
- Skip chunks that are redundant, low-quality, vague, or off-topic
- Aim for comprehensive coverage of the topic's key aspects
- Call done() when you have sufficient material for a well-sourced article
- You do NOT write the article — you only curate the sources"#
    )
}

fn research_user_prompt_generate(tag_name: &str) -> String {
    format!(
        "Find and curate source material for a wiki article about \"{}\". \
         Search for content covering different aspects of this topic, \
         review the results, and select the best chunks.",
        tag_name
    )
}

fn research_user_prompt_update(tag_name: &str, existing_article: &str) -> String {
    format!(
        "Find and curate source material for updating the wiki article about \"{tag_name}\". \
         The existing article is shown below — focus on finding NEW information, \
         filling gaps, or correcting outdated content.\n\n\
         EXISTING ARTICLE:\n{existing_article}\n\n\
         Search for content that adds new perspectives or information not already covered."
    )
}

// ==================== Public API ====================

/// Generate a wiki article using the agentic research strategy.
pub(crate) async fn generate(
    ctx: &WikiStrategyContext,
) -> Result<WikiArticleWithCitations, String> {
    let max_tokens = ctx.max_source_tokens();
    tracing::info!(tag_name = %ctx.tag_name, budget_tokens = max_tokens, "[wiki/agentic] Starting agentic research");

    // Get scope tag IDs and atom count
    let scope_tag_ids = ctx
        .storage
        .get_tag_hierarchy_impl(&ctx.tag_id)
        .await
        .map_err(|e| e.to_string())?;
    let atom_count = ctx
        .storage
        .count_atoms_with_tags_impl(
            &scope_tag_ids,
            &crate::models::KindFilter::only(crate::models::AtomKind::Captured),
        )
        .await
        .map_err(|e| e.to_string())?;

    if atom_count == 0 {
        return Err("No content found for this tag".to_string());
    }

    // Run research
    let mut rc = ResearchContext::new(
        research_system_prompt(&ctx.tag_name),
        research_user_prompt_generate(&ctx.tag_name),
    );

    run_research(
        &mut rc,
        &ctx.storage,
        &scope_tag_ids,
        &ctx.provider_config,
        &ctx.wiki_model,
        max_tokens,
    )
    .await?;

    let raw_chunks = rc.selected_chunks();
    tracing::info!(
        chunks = raw_chunks.len(),
        tokens = rc.selected_tokens,
        "[wiki/agentic] Research complete"
    );

    if raw_chunks.is_empty() {
        return Err("Agent did not select any source chunks".to_string());
    }

    let chunks = trim_to_budget(raw_chunks, max_tokens);
    if chunks.len() < rc.selected_indices.len() {
        tracing::info!(
            chunks = chunks.len(),
            "[wiki/agentic] Trimmed to fit token budget"
        );
    }

    // Synthesize article from selected chunks
    tracing::info!("[wiki/agentic] Synthesizing article...");
    synthesize_article(
        &ctx.provider_config,
        &ctx.tag_id,
        &ctx.tag_name,
        &chunks,
        atom_count,
        &ctx.wiki_model,
        &ctx.linkable_article_names,
        ctx.generation_prompt(),
    )
    .await
}

/// Run the research loop against an existing article and return the selected
/// chunks + atom count. Used by both the legacy full-rewrite update path and
/// the new propose path.
///
/// When `filter_existing` is true, any selected chunk whose `(atom_id, chunk_index)`
/// is already present in the existing article's citations is dropped — the
/// propose path only wants net-new sources so citation numbering stays clean.
/// The legacy full-rewrite path passes `false` (preserving its current behavior
/// of feeding everything to synthesis).
pub(crate) async fn research_for_update(
    ctx: &WikiStrategyContext,
    existing: &WikiArticleWithCitations,
    filter_existing: bool,
) -> Result<Option<(Vec<ChunkWithContext>, i32)>, String> {
    let max_tokens = ctx.max_source_tokens();
    tracing::info!(
        tag_name = %ctx.tag_name,
        budget_tokens = max_tokens,
        filter_existing,
        "[wiki/agentic] Starting agentic research for update"
    );

    let scope_tag_ids = ctx
        .storage
        .get_tag_hierarchy_impl(&ctx.tag_id)
        .await
        .map_err(|e| e.to_string())?;
    let atom_count = ctx
        .storage
        .count_atoms_with_tags_impl(
            &scope_tag_ids,
            &crate::models::KindFilter::only(crate::models::AtomKind::Captured),
        )
        .await
        .map_err(|e| e.to_string())?;

    let mut rc = ResearchContext::new(
        research_system_prompt(&ctx.tag_name),
        research_user_prompt_update(&ctx.tag_name, &existing.article.content),
    );

    run_research(
        &mut rc,
        &ctx.storage,
        &scope_tag_ids,
        &ctx.provider_config,
        &ctx.wiki_model,
        max_tokens,
    )
    .await?;

    let raw_chunks = rc.selected_chunks();
    tracing::info!(
        chunks = raw_chunks.len(),
        tokens = rc.selected_tokens,
        "[wiki/agentic] Update research complete"
    );

    if raw_chunks.is_empty() {
        tracing::info!("[wiki/agentic] No chunks selected, no update needed");
        return Ok(None);
    }

    // Optional filter: drop chunks that already appear as existing citations.
    // Match on (atom_id, chunk_index) — the same key used when extracting
    // citations from the LLM output.
    let filtered = if filter_existing {
        let cited: std::collections::HashSet<(String, i32)> = existing
            .citations
            .iter()
            .map(|c| (c.atom_id.clone(), c.chunk_index.unwrap_or(0)))
            .collect();
        let before = raw_chunks.len();
        let filtered: Vec<ChunkWithContext> = raw_chunks
            .into_iter()
            .filter(|c| !cited.contains(&(c.atom_id.clone(), c.chunk_index)))
            .collect();
        if filtered.len() < before {
            tracing::info!(
                before,
                after = filtered.len(),
                "[wiki/agentic] Filtered out already-cited chunks"
            );
        }
        if filtered.is_empty() {
            tracing::info!(
                "[wiki/agentic] All selected chunks were already cited, no update needed"
            );
            return Ok(None);
        }
        filtered
    } else {
        raw_chunks
    };

    let chunks = trim_to_budget(filtered, max_tokens);
    if chunks.len() < rc.selected_indices.len() {
        tracing::info!(
            chunks = chunks.len(),
            "[wiki/agentic] Trimmed to fit token budget"
        );
    }

    Ok(Some((chunks, atom_count)))
}


