//! Wiki article synthesis
//!
//! This module handles generating and updating wiki articles for tags.
//! Strategies control both source selection and content generation;
//! shared utilities (save, load, citations, LLM calls) live here.

mod agentic;
pub(crate) mod centroid;
pub mod runner;
pub mod section_ops;

pub use section_ops::{apply_section_ops, WikiSectionOp, WikiSectionOpWire};

use crate::models::{
    ChunkWithContext, RelatedTag, SuggestedArticle, WikiArticle, WikiArticleStatus,
    WikiArticleSummary, WikiArticleVersion, WikiArticleWithCitations, WikiCitation, WikiLink,
    WikiVersionSummary,
};
use crate::providers::structured::{call_structured, StructuredCall};
use crate::providers::types::Message;
use crate::providers::ProviderConfig;
use crate::storage::StorageBackend;

use chrono::Utc;
use regex::Regex;
use rusqlite::Connection;
use serde::{de::DeserializeOwned, Deserialize};
use uuid::Uuid;

// ==================== Strategy Types ====================

/// Wiki generation strategy
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WikiStrategy {
    /// Default — centroid similarity chunk selection + single-shot LLM call
    Centroid,
    /// Agent-driven research loop + shared synthesis
    Agentic,
}

impl WikiStrategy {
    pub fn from_string(s: &str) -> Self {
        match s {
            "centroid" => WikiStrategy::Centroid,
            "agentic" => WikiStrategy::Agentic,
            _ => WikiStrategy::Centroid,
        }
    }
}

/// Context passed to strategy implementations
pub struct WikiStrategyContext {
    pub storage: StorageBackend,
    pub provider_config: ProviderConfig,
    pub wiki_model: String,
    pub tag_id: String,
    pub tag_name: String,
    pub linkable_article_names: Vec<(String, String)>,
    pub custom_generation_prompt: Option<String>,
    pub custom_update_prompt: Option<String>,
}

impl WikiStrategyContext {
    /// Returns the generation system prompt, using custom if set, otherwise the default.
    pub fn generation_prompt(&self) -> &str {
        self.custom_generation_prompt
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(WIKI_GENERATION_SYSTEM_PROMPT)
    }

    /// Returns the update system prompt, using custom if set, otherwise the default.
    pub fn update_prompt(&self) -> &str {
        self.custom_update_prompt
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(WIKI_UPDATE_SYSTEM_PROMPT)
    }

    /// Returns the section-ops update prompt. If a custom update prompt is set,
    /// it is prepended to the structural instructions so user tone/style preferences
    /// are respected while preserving the JSON schema contract.
    pub fn section_ops_prompt(&self) -> String {
        match self
            .custom_update_prompt
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            Some(custom) => format!("{}\n\n{}", custom, WIKI_UPDATE_SECTION_OPS_PROMPT),
            None => WIKI_UPDATE_SECTION_OPS_PROMPT.to_string(),
        }
    }

    /// Returns the maximum source material tokens for wiki generation.
    /// For providers with a known context length, budgets ~60% for source material.
    /// Falls back to MAX_WIKI_SOURCE_TOKENS for providers with large/unknown context.
    pub fn max_source_tokens(&self) -> usize {
        match self
            .provider_config
            .context_length_for_model(&self.wiki_model)
        {
            Some(ctx_len) => {
                // Reserve ~40% for system prompt, output, and structured output framing
                let budget = (ctx_len as f64 * 0.6) as usize;
                budget.min(MAX_WIKI_SOURCE_TOKENS)
            }
            None => MAX_WIKI_SOURCE_TOKENS,
        }
    }
}

/// Generate a wiki article using the given strategy.
pub async fn strategy_generate(
    strategy: &WikiStrategy,
    ctx: &WikiStrategyContext,
) -> Result<WikiArticleWithCitations, String> {
    match strategy {
        WikiStrategy::Centroid => centroid::generate(ctx).await,
        WikiStrategy::Agentic => agentic::generate(ctx).await,
    }
}

/// Update an existing wiki article using the given strategy.
/// Returns None if no update is needed (e.g., no new content).
pub async fn strategy_update(
    strategy: &WikiStrategy,
    ctx: &WikiStrategyContext,
    existing: &WikiArticleWithCitations,
) -> Result<Option<WikiArticleWithCitations>, String> {
    match strategy {
        WikiStrategy::Centroid => centroid::update(ctx, existing).await,
        WikiStrategy::Agentic => agentic::update(ctx, existing).await,
    }
}

/// Draft of a wiki update, produced by the propose path.
///
/// The applier has already been run — `merged_content` is the final article
/// text ready to diff and save. `ops` is retained for debuggability.
pub struct WikiProposalDraft {
    pub ops: Vec<WikiSectionOp>,
    pub merged_content: String,
    pub citations: Vec<WikiCitation>,
    pub new_atom_count: i32,
}

/// Result of attempting to build a wiki update proposal.
pub enum WikiProposalOutcome {
    Draft(WikiProposalDraft),
    /// The selector found no chunks to send to the LLM.
    NoUpdateChunks,
    /// The LLM reviewed update chunks and explicitly found no useful change.
    NoChange,
}

/// Propose an update to an existing wiki article using the given strategy.
///
/// Returns `None` if no update is warranted (no new atoms, empty ops, or the
/// LLM returns `NoChange`).
pub async fn strategy_propose(
    strategy: &WikiStrategy,
    ctx: &WikiStrategyContext,
    existing: &WikiArticleWithCitations,
) -> Result<Option<WikiProposalDraft>, String> {
    match strategy_propose_outcome(strategy, ctx, existing).await? {
        WikiProposalOutcome::Draft(draft) => Ok(Some(draft)),
        WikiProposalOutcome::NoUpdateChunks | WikiProposalOutcome::NoChange => Ok(None),
    }
}

/// Propose an update to an existing wiki article using the given strategy.
///
/// Composes two independent steps:
///
/// 1. Strategy-specific chunk selection (`select_update_chunks`): for Centroid,
///    new atoms since `existing.updated_at`; for Agentic, the research loop
///    with the existing article as context and already-cited chunks filtered out.
/// 2. Strategy-agnostic section-ops generation (`generate_section_ops_proposal`):
///    shared across strategies — the LLM emits a list of section operations, the
///    applier merges them into the existing content, and citations are extracted
///    from the merged output.
///
/// Returns a typed no-op outcome so callers can distinguish "nothing was sent
/// to the LLM" from "the LLM reviewed new chunks and returned `NoChange`".
pub async fn strategy_propose_outcome(
    strategy: &WikiStrategy,
    ctx: &WikiStrategyContext,
    existing: &WikiArticleWithCitations,
) -> Result<WikiProposalOutcome, String> {
    let Some((new_chunks, total_atom_count)) =
        select_update_chunks(strategy, ctx, existing).await?
    else {
        return Ok(WikiProposalOutcome::NoUpdateChunks);
    };

    // New-atom count is the delta against the baseline the live article was
    // built from. Matches the convention used by `get_wiki_status` and the
    // "N new atoms available" banner. Clamp to zero in case atoms were deleted
    // since the last accepted version.
    let new_atom_count = (total_atom_count - existing.article.atom_count).max(0);

    match generate_section_ops_proposal(ctx, existing, &new_chunks, new_atom_count).await? {
        Some(draft) => Ok(WikiProposalOutcome::Draft(draft)),
        None => Ok(WikiProposalOutcome::NoChange),
    }
}

/// Strategy-specific chunk selection for the propose path.
async fn select_update_chunks(
    strategy: &WikiStrategy,
    ctx: &WikiStrategyContext,
    existing: &WikiArticleWithCitations,
) -> Result<Option<(Vec<ChunkWithContext>, i32)>, String> {
    match strategy {
        WikiStrategy::Centroid => {
            // New atoms since the article's last updated_at, centroid-ranked.
            ctx.storage
                .get_wiki_update_chunks_sync(
                    &ctx.tag_id,
                    &existing.article.updated_at,
                    ctx.max_source_tokens(),
                )
                .await
                .map_err(|e| e.to_string())
        }
        WikiStrategy::Agentic => {
            // Run the research loop with the existing article as context, then
            // filter out chunks already cited in the existing article so we
            // only propose updates for net-new material.
            agentic::research_for_update(ctx, existing, true).await
        }
    }
}

/// Structured result from the section-ops LLM call. Deserializes the flat
/// wire shape emitted by the structured-output schema; the generator then
/// calls `WikiSectionOpWire::into_op()` on each entry to validate and convert
/// to the strict `WikiSectionOp` enum.
#[derive(Debug, Deserialize)]
pub(crate) struct WikiUpdateOpsResult {
    pub operations: Vec<WikiSectionOpWire>,
    #[allow(dead_code)]
    #[serde(default)]
    pub citations_used: Vec<i32>,
}

/// JSON Schema for `WikiUpdateOpsResult`.
///
/// Design constraints dictated by multi-provider support:
///
/// - **No `oneOf` discriminated unions.** OpenAI strict mode supports them
///   but some OpenRouter-routed providers and local models fall back oddly.
/// - **No nullable unions (`["string", "null"]`).** OpenAI/Anthropic handle
///   these, but smaller local models via Ollama are unreliable.
/// - **All fields required, all strings.** Empty string `""` is the sentinel
///   for "not applicable" — same convention as `extraction.rs` which is
///   proven to work across every provider in this codebase.
/// - **`additionalProperties: false`.** Required by OpenAI strict mode.
///
/// The LLM emits the flat shape; `WikiSectionOpWire::into_op()` validates
/// and converts to the strict `WikiSectionOp` enum.
pub(crate) fn section_ops_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "operations": {
                "type": "array",
                "description": "List of section operations to apply to the article, in order. Return a single operation with op=\"NoChange\" if nothing needs to change.",
                "items": {
                    "type": "object",
                    "properties": {
                        "op": {
                            "type": "string",
                            "enum": ["NoChange", "AppendToSection", "ReplaceSection", "InsertSection"],
                            "description": "Operation type."
                        },
                        "heading": {
                            "type": "string",
                            "description": "For AppendToSection/ReplaceSection: exact existing heading to target. For InsertSection: the new section's heading. For NoChange: empty string."
                        },
                        "after_heading": {
                            "type": "string",
                            "description": "For InsertSection only: exact existing heading to insert AFTER, or empty string to append at end of article. For all other ops: empty string."
                        },
                        "content": {
                            "type": "string",
                            "description": "New markdown content for the operation. Only NoChange may use empty content. AppendToSection, ReplaceSection, and InsertSection must provide non-empty markdown content with citations."
                        }
                    },
                    "required": ["op", "heading", "after_heading", "content"],
                    "additionalProperties": false
                }
            },
            "citations_used": {
                "type": "array",
                "items": { "type": "integer" },
                "description": "List of citation numbers referenced across the operations."
            }
        },
        "required": ["operations", "citations_used"],
        "additionalProperties": false
    })
}

/// Shared, strategy-agnostic section-ops generation.
///
/// Given the existing article and a set of new chunks, ask the LLM for a list
/// of structured section operations, apply them via the section_ops applier,
/// and extract citations from the merged result.
async fn generate_section_ops_proposal(
    ctx: &WikiStrategyContext,
    existing: &WikiArticleWithCitations,
    new_chunks: &[ChunkWithContext],
    new_atom_count: i32,
) -> Result<Option<WikiProposalDraft>, String> {
    // Build existing sources block (for the LLM to reference by citation number).
    let mut existing_sources = String::new();
    for citation in &existing.citations {
        existing_sources.push_str(&format!(
            "[{}] {}\n\n",
            citation.citation_index, citation.excerpt
        ));
    }

    // Build new sources block, continuing citation numbering.
    //
    // Start AFTER the largest existing citation index, not after `len()`.
    // Existing citations can have gaps (the LLM doesn't always use every index
    // it's offered, and dead indices aren't backfilled). Using `len + 1` can
    // collide with preserved high-index markers like `[47]` that survive in
    // untouched sections.
    let start_index = existing
        .citations
        .iter()
        .map(|c| c.citation_index)
        .max()
        .unwrap_or(0)
        + 1;
    let mut new_sources = String::new();
    for (i, chunk) in new_chunks.iter().enumerate() {
        new_sources.push_str(&format!(
            "[{}] {}\n\n",
            start_index + i as i32,
            chunk.content
        ));
    }

    // Build existing-articles list for cross-linking (matches the full-rewrite flow).
    let articles_section = if ctx.linkable_article_names.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = ctx
            .linkable_article_names
            .iter()
            .filter(|(tid, _)| tid != &ctx.tag_id)
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

    // Enumerate current section headings for the LLM to reference verbatim.
    let heading_list = extract_current_headings(&existing.article.content);
    let headings_block = if heading_list.is_empty() {
        "(no section headings — the article has no sections yet; use InsertSection with after_heading=\"\" to add one at the end)".to_string()
    } else {
        heading_list
            .iter()
            .map(|(level, h)| {
                // Indent sub-headings so the LLM can see the hierarchy.
                // Level 2 = no indent; each extra level adds two spaces.
                let indent = "  ".repeat((*level as usize).saturating_sub(2));
                format!("{}{}", indent, h)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let user_content = format!(
        "CURRENT ARTICLE:\n{}\n\n\
         CURRENT SECTION HEADINGS (use these values verbatim in your operations — do not paraphrase):\n{}\n\n\
         EXISTING SOURCES (already cited in the article — reuse these indices verbatim if you reference them):\n{}\n\
         NEW SOURCES TO INCORPORATE (cite as [{}] onwards):\n{}{}\n\
         Return structured section operations that incorporate the new sources into the article.{}",
        existing.article.content,
        headings_block,
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

    let prompt = ctx.section_ops_prompt();
    let result: WikiUpdateOpsResult = call_llm_for_wiki_typed(
        &ctx.provider_config,
        &prompt,
        &user_content,
        &ctx.wiki_model,
        "wiki_update_section_ops",
        section_ops_schema(),
    )
    .await?;

    // Convert the flat wire shape into the strict enum, validating required
    // fields per variant. Any invalid op (unknown discriminator, missing
    // required field) aborts the whole proposal — same posture as a
    // hallucinated heading.
    let ops: Vec<WikiSectionOp> = result
        .operations
        .into_iter()
        .map(|wire| wire.into_op())
        .collect::<Result<Vec<_>, String>>()
        .map_err(|e| {
            tracing::warn!(error = %e, "[wiki] Section-ops LLM returned an invalid operation");
            format!("LLM returned an invalid section operation: {}", e)
        })?;

    // No-op detection: empty ops, all NoChange, or a single NoChange.
    let has_meaningful_op = ops.iter().any(|o| !matches!(o, WikiSectionOp::NoChange));
    if !has_meaningful_op {
        tracing::info!("[wiki] Proposal LLM returned no-change; nothing to propose");
        return Ok(None);
    }

    // Apply ops to the existing content. Hallucinated heading → error propagates.
    let merged_content = apply_section_ops(&existing.article.content, &ops).map_err(|e| {
        tracing::warn!(error = %e, "[wiki] Section ops applier failed");
        e
    })?;

    // Resolve citation markers in the merged content by explicit index lookup.
    //
    // The legacy full-rewrite path uses `extract_citations` with positional
    // mapping (`[N]` → `chunks[N-1]`), which only works when the article's
    // citations happen to be contiguous 1..N. Section-ops preserves original
    // `[N]` markers byte-for-byte in untouched sections, so any gap in the
    // existing citation indices (e.g. `[4] [6] [15] [47]`) would either collide
    // with the new-source numbering or silently drop high-index markers. We
    // build an explicit `index → source` map instead: existing citations map
    // to themselves (preserving atom_id / chunk_index / excerpt), and new
    // chunks map to the indices we assigned them in the prompt.
    let existing_by_index: std::collections::HashMap<i32, &WikiCitation> = existing
        .citations
        .iter()
        .map(|c| (c.citation_index, c))
        .collect();
    let new_by_index: std::collections::HashMap<i32, &ChunkWithContext> = new_chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| (start_index + i as i32, chunk))
        .collect();

    let marker_re = Regex::new(r"\[(\d+)\]").map_err(|e| format!("regex: {}", e))?;
    let mut seen: std::collections::HashSet<i32> = std::collections::HashSet::new();
    let mut citations: Vec<WikiCitation> = Vec::new();
    for cap in marker_re.captures_iter(&merged_content) {
        let index: i32 = match cap.get(1).and_then(|m| m.as_str().parse().ok()) {
            Some(n) => n,
            None => continue,
        };
        if !seen.insert(index) {
            continue;
        }
        if let Some(existing_citation) = existing_by_index.get(&index) {
            // Preserve the original citation unchanged — same atom, same
            // chunk, same excerpt. Just mint a fresh row id for the new
            // wiki_citations insert on accept.
            citations.push(WikiCitation {
                id: Uuid::new_v4().to_string(),
                citation_index: index,
                atom_id: existing_citation.atom_id.clone(),
                chunk_index: existing_citation.chunk_index,
                excerpt: existing_citation.excerpt.clone(),
                source_url: existing_citation.source_url.clone(),
            });
        } else if let Some(chunk) = new_by_index.get(&index) {
            let excerpt = if chunk.content.len() > 300 {
                let truncate_pos = chunk
                    .content
                    .char_indices()
                    .take_while(|(i, _)| *i < 297)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                format!("{}...", &chunk.content[..truncate_pos])
            } else {
                chunk.content.clone()
            };
            citations.push(WikiCitation {
                id: Uuid::new_v4().to_string(),
                citation_index: index,
                atom_id: chunk.atom_id.clone(),
                chunk_index: Some(chunk.chunk_index),
                excerpt,
                // Populated by the read path's JOIN; not stored on the row.
                source_url: None,
            });
        } else {
            tracing::warn!(
                citation_index = index,
                "[wiki] Merged article references citation with no known source — dropping from citation list"
            );
        }
    }
    citations.sort_by_key(|c| c.citation_index);

    Ok(Some(WikiProposalDraft {
        ops,
        merged_content,
        citations,
        new_atom_count,
    }))
}

/// Extract the exact text of all `##` (level 2) headings in an article, in
/// document order. Used to tell the LLM which headings it can target.
///
/// Only level 2 is returned because `apply_section_ops` / `parse_sections` in
/// `section_ops.rs` only treat `##` as a section boundary — `###` and deeper
/// stay embedded in their parent section's body. Surfacing `###` headings to
/// the LLM would let it target a heading the applier can't resolve, which
/// discards the entire proposal as a hallucination.
fn extract_current_headings(content: &str) -> Vec<(u8, String)> {
    let mut headings = Vec::new();
    for line in content.lines() {
        let stripped = line.trim_start();
        let bytes = stripped.as_bytes();
        let mut hashes = 0;
        while hashes < bytes.len() && bytes[hashes] == b'#' {
            hashes += 1;
        }
        if hashes >= 2 && hashes < bytes.len() && bytes[hashes] == b' ' {
            headings.push((hashes as u8, stripped[hashes + 1..].trim().to_string()));
        }
    }
    headings
}

// ==================== Shared Constants ====================

/// Maximum source material tokens for wiki generation.
/// Leaves room for system prompt, article output, and structured output framing.
/// Most wiki models have 128K context; we budget ~80K for source material.
pub(crate) const MAX_WIKI_SOURCE_TOKENS: usize = 80_000;

pub(crate) const WIKI_GENERATION_SYSTEM_PROMPT: &str = r#"You are synthesizing a wiki article based on the user's personal knowledge base. Write a well-structured, informative article that summarizes what is known about the topic.

Guidelines:
- Use markdown formatting with ## for main sections and ### for subsections
- Every factual claim MUST have a citation using [N] notation
- Place citations immediately after the relevant statement
- If sources contain contradictions, note them
- Structure logically: overview first, then thematic sections
- Keep tone informative and neutral
- Do not invent information not present in the sources
- When mentioning topics that have their own articles in the knowledge base, use [[Topic Name]] wiki-link notation to cross-reference them
- Only use [[wiki links]] for topics listed in the EXISTING WIKI ARTICLES section provided
- Do not force wiki links where they don't fit naturally"#;

pub(crate) const WIKI_UPDATE_SYSTEM_PROMPT: &str = r#"You are updating an existing wiki article with new information from additional sources. Integrate the new information naturally into the existing article.

Guidelines:
- Maintain the existing structure where sensible
- Add new sections if needed for new topics
- Do not remove existing content unless directly contradicted by new sources
- Use [N] notation for citations, continuing from the existing numbering
- Every new factual claim MUST have a citation
- Keep tone consistent with the existing article
- When mentioning topics that have their own articles, use [[Topic Name]] wiki-link notation
- Only use [[wiki links]] for topics listed in the EXISTING WIKI ARTICLES section provided
- Do not force wiki links where they don't fit naturally"#;

pub(crate) const WIKI_UPDATE_SECTION_OPS_PROMPT: &str = r#"You are proposing updates to an existing wiki article based on new sources.

Return a list of structured operations that incorporate the new information while leaving untouched sections exactly as they are.

Every operation is an object with four fields: op, heading, after_heading, content. All four fields MUST be present in every operation. Use empty strings ("") for fields that don't apply to the chosen op.

Operations (value of the `op` field):
- "NoChange": the new sources don't warrant updating the article. Use empty strings for heading, after_heading, and content. Return this as the ONLY operation in the list if nothing needs to change.
- "AppendToSection": add new material to the end of an existing section. Set `heading` to the exact existing section heading. Set `content` to the new markdown to append. Leave `after_heading` as an empty string.
- "ReplaceSection": rewrite a section's body (use sparingly — only when existing content is directly contradicted or superseded). Set `heading` to the exact existing section heading. Set `content` to the new body. Leave `after_heading` as an empty string.
- "InsertSection": add a brand-new section (use only for genuinely new topics not covered elsewhere). Set `heading` to the new section's heading. Set `after_heading` to the exact existing heading you want to insert AFTER, or leave it empty ("") to append the new section at the end of the article. Set `content` to the new section body.

Rules:
- `heading` and `after_heading` values must EXACTLY match one of the headings listed under CURRENT SECTION HEADINGS when they reference existing sections. Do not paraphrase, reword, or change capitalization. Sub-headings appear indented under their parent in the list; use the exact heading text without any # prefix characters.
- Only NoChange may have empty content. AppendToSection, ReplaceSection, and InsertSection MUST have non-empty markdown content with citations. If you have no content to add for a section, do not emit an edit operation for that section. If there are no non-empty edits to make, return exactly one NoChange operation.
- Prefer AppendToSection over ReplaceSection. Prefer editing an existing section over creating a new one.
- Every new factual claim MUST have a [N] citation using the next-available citation numbers shown in the user message.
- Keep tone consistent with the existing article.
- When mentioning topics that have their own wiki articles, use [[Topic Name]] notation — only for topics listed in EXISTING WIKI ARTICLES.
- Never omit the heading, after_heading, or content fields. Use "" when they don't apply."#;

// ==================== Shared LLM Infrastructure ====================

#[derive(Deserialize)]
pub(crate) struct WikiGenerationResult {
    pub article_content: String,
    #[allow(dead_code)]
    pub citations_used: Vec<i32>,
}

/// JSON schema for wiki full-article generation. Extracted as a function so
/// it's callable from the lint regression tests.
pub(crate) fn wiki_generation_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "article_content": {
                "type": "string",
                "description": "The full wiki article in markdown format with [N] citations"
            },
            "citations_used": {
                "type": "array",
                "items": { "type": "integer" },
                "description": "List of citation numbers actually used in the article"
            }
        },
        "required": ["article_content", "citations_used"],
        "additionalProperties": false
    })
}

/// Call LLM provider for wiki generation (full-article rewrite schema).
/// Thin wrapper over [`call_llm_for_wiki_typed`].
pub(crate) async fn call_llm_for_wiki(
    provider_config: &ProviderConfig,
    system_prompt: &str,
    user_content: &str,
    model: &str,
) -> Result<WikiGenerationResult, String> {
    call_llm_for_wiki_typed::<WikiGenerationResult>(
        provider_config,
        system_prompt,
        user_content,
        model,
        "wiki_generation_result",
        wiki_generation_schema(),
    )
    .await
}

/// Generic LLM call for wiki operations. Thin wrapper over the shared
/// [`call_structured`] helper that adds wiki-specific tracing context so
/// the logs still say `[wiki]` for anyone watching them. The retry / parse
/// / prompt-based-fallback loop lives in the shared helper now.
pub(crate) async fn call_llm_for_wiki_typed<T: DeserializeOwned>(
    provider_config: &ProviderConfig,
    system_prompt: &str,
    user_content: &str,
    model: &str,
    schema_name: &'static str,
    schema: serde_json::Value,
) -> Result<T, String> {
    tracing::info!(
        model,
        input_chars = user_content.len(),
        schema = %schema_name,
        "[wiki] Starting generation"
    );

    let messages = vec![Message::system(system_prompt), Message::user(user_content)];

    // prompt_only: wiki articles and section rewrites are the longest
    // outputs in the app — the shape OpenRouter's structured-output layer
    // silently corrupted on reports (stream tail lost, JSON repaired around
    // the cut; see StructuredCall::prompt_only). Schema-in-prompt makes a
    // lost tail an unparseable response — a loud retry, not a stored stub.
    let call = StructuredCall::<T>::new(provider_config, model, &messages, schema_name, schema)
        .with_prompt_only(true);

    let started = std::time::Instant::now();
    match call_structured::<T>(call).await {
        Ok(result) => {
            tracing::info!(
                elapsed_secs = format_args!("{:.1}", started.elapsed().as_secs_f64()),
                "[wiki] Structured generation complete"
            );
            Ok(result)
        }
        Err(e) => {
            tracing::error!(error = %e, "[wiki] Structured generation failed");
            Err(e.to_compact_string())
        }
    }
}

// ==================== Shared Utilities ====================

/// Extract citations from article content and map to source chunks
pub(crate) fn extract_citations(
    _article_id: &str,
    content: &str,
    chunks: &[ChunkWithContext],
) -> Result<Vec<WikiCitation>, String> {
    let re = Regex::new(r"\[(\d+)\]").map_err(|e| format!("Failed to compile regex: {}", e))?;

    let mut citations: Vec<WikiCitation> = Vec::new();
    let mut seen_indices: std::collections::HashSet<i32> = std::collections::HashSet::new();

    for cap in re.captures_iter(content) {
        if let Some(num_match) = cap.get(1) {
            if let Ok(index) = num_match.as_str().parse::<i32>() {
                // Skip if we've already processed this citation index
                if seen_indices.contains(&index) {
                    continue;
                }
                seen_indices.insert(index);

                // Map to chunk (1-indexed)
                let chunk_idx = (index - 1) as usize;
                if chunk_idx < chunks.len() {
                    let chunk = &chunks[chunk_idx];
                    // Truncate excerpt to ~300 chars, respecting UTF-8 char boundaries
                    let excerpt = if chunk.content.len() > 300 {
                        // Find a safe character boundary near 297 bytes
                        let truncate_pos = chunk
                            .content
                            .char_indices()
                            .take_while(|(i, _)| *i < 297)
                            .last()
                            .map(|(i, c)| i + c.len_utf8())
                            .unwrap_or(0);
                        format!("{}...", &chunk.content[..truncate_pos])
                    } else {
                        chunk.content.clone()
                    };

                    citations.push(WikiCitation {
                        id: Uuid::new_v4().to_string(),
                        citation_index: index,
                        atom_id: chunk.atom_id.clone(),
                        chunk_index: Some(chunk.chunk_index),
                        excerpt,
                        // Populated by the read path's JOIN; not stored on the row.
                        source_url: None,
                    });
                }
            }
        }
    }

    // Sort by citation index
    citations.sort_by_key(|c| c.citation_index);

    Ok(citations)
}

/// Get all tag IDs in hierarchy (tag + all descendants) using recursive CTE
pub(crate) fn get_tag_hierarchy(conn: &Connection, tag_id: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(
            "WITH RECURSIVE descendant_tags(id) AS (
                SELECT ?1
                UNION ALL
                SELECT t.id FROM tags t
                INNER JOIN descendant_tags dt ON t.parent_id = dt.id
            )
            SELECT id FROM descendant_tags",
        )
        .map_err(|e| format!("Failed to prepare hierarchy query: {}", e))?;

    let tag_ids: Vec<String> = stmt
        .query_map([tag_id], |row| row.get(0))
        .map_err(|e| format!("Failed to query hierarchy: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect hierarchy: {}", e))?;

    Ok(tag_ids)
}

/// Count atoms with any of the given tags
pub(crate) fn count_atoms_with_tags(
    conn: &Connection,
    tag_ids: &[String],
    kinds: &crate::models::KindFilter,
) -> Result<i32, String> {
    let placeholders = tag_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let (kind_frag, kind_binds) = kinds.sqlite_in_clause("atoms.kind");
    let query = format!(
        "SELECT COUNT(DISTINCT atom_tags.atom_id) FROM atom_tags
         INNER JOIN atoms ON atoms.id = atom_tags.atom_id
         WHERE atom_tags.tag_id IN ({placeholders}) AND {kind_frag}"
    );
    // tag_ids first, then kind binds.
    let mut bind_refs: Vec<&dyn rusqlite::ToSql> =
        tag_ids.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    for v in &kind_binds {
        bind_refs.push(v as &dyn rusqlite::ToSql);
    }
    conn.query_row(
        &query,
        rusqlite::params_from_iter(bind_refs.iter()),
        |row| row.get(0),
    )
    .map_err(|e| format!("Failed to count atoms: {}", e))
}

/// Batch-fetch chunk details (atom_id, chunk_index, content) by chunk IDs.
pub(crate) fn batch_fetch_chunk_details(
    conn: &Connection,
    chunk_ids: &[&str],
) -> Result<std::collections::HashMap<String, (String, i32, String)>, String> {
    let mut map = std::collections::HashMap::new();
    // Batch in groups of 500 to stay under SQLite parameter limit
    for batch in chunk_ids.chunks(500) {
        let placeholders = batch.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT id, atom_id, chunk_index, content FROM atom_chunks WHERE id IN ({})",
            placeholders
        );
        let mut stmt = conn
            .prepare(&query)
            .map_err(|e| format!("Failed to prepare chunk details query: {}", e))?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(batch.iter()))
            .map_err(|e| format!("Failed to query chunk details: {}", e))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| format!("Failed to read row: {}", e))?
        {
            let id: String = row.get(0).map_err(|e| format!("Failed to get id: {}", e))?;
            let atom_id: String = row
                .get(1)
                .map_err(|e| format!("Failed to get atom_id: {}", e))?;
            let chunk_index: i32 = row
                .get(2)
                .map_err(|e| format!("Failed to get chunk_index: {}", e))?;
            let content: String = row
                .get(3)
                .map_err(|e| format!("Failed to get content: {}", e))?;
            map.insert(id, (atom_id, chunk_index, content));
        }
    }
    Ok(map)
}

// ==================== Shared Synthesis ====================

/// Synthesize a wiki article from a set of chunks.
/// Used by both centroid and agentic strategies after source selection.
pub(crate) async fn synthesize_article(
    provider_config: &ProviderConfig,
    tag_id: &str,
    tag_name: &str,
    chunks: &[ChunkWithContext],
    atom_count: i32,
    model: &str,
    linkable_article_names: &[(String, String)],
    system_prompt: &str,
) -> Result<WikiArticleWithCitations, String> {
    // Build source materials for prompt
    let mut source_materials = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        source_materials.push_str(&format!("[{}] {}\n\n", i + 1, chunk.content));
    }

    // Build existing articles list for cross-linking
    let articles_section = if linkable_article_names.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = linkable_article_names
            .iter()
            .filter(|(tid, _)| tid != tag_id)
            .map(|(_, name)| name.as_str())
            .collect();
        if names.is_empty() {
            String::new()
        } else {
            format!(
                "EXISTING WIKI ARTICLES IN THIS KNOWLEDGE BASE:\n{}\n\n",
                names.join(", ")
            )
        }
    };

    let user_content = format!(
        "Write a wiki article about \"{}\".\n\n{}\
         SOURCE MATERIALS:\n{}\
         Write the article now, citing sources with [N] notation.{}",
        tag_name,
        articles_section,
        source_materials,
        if articles_section.is_empty() {
            ""
        } else {
            " Use [[Article Name]] to link to other articles listed above where relevant."
        }
    );

    let result = call_llm_for_wiki(provider_config, system_prompt, &user_content, model).await?;

    let article_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let article = WikiArticle {
        id: article_id.clone(),
        tag_id: tag_id.to_string(),
        content: result.article_content.clone(),
        created_at: now.clone(),
        updated_at: now,
        atom_count,
    };

    let citations = extract_citations(&article_id, &result.article_content, chunks)?;

    Ok(WikiArticleWithCitations { article, citations })
}

// ==================== Database Operations ====================

/// Save a wiki article, its citations, and wiki links to the database.
/// Archives the existing article (if any) into wiki_article_versions before replacing it.
pub fn save_wiki_article(
    conn: &Connection,
    article: &WikiArticle,
    citations: &[WikiCitation],
    wiki_links: &[WikiLink],
) -> Result<(), String> {
    // Archive existing article before deleting
    archive_existing_article(conn, &article.tag_id)?;

    // Delete existing article for this tag (if any)
    conn.execute(
        "DELETE FROM wiki_articles WHERE tag_id = ?1",
        [&article.tag_id],
    )
    .map_err(|e| format!("Failed to delete existing article: {}", e))?;

    // Insert new article
    conn.execute(
        "INSERT INTO wiki_articles (id, tag_id, content, created_at, updated_at, atom_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            &article.id,
            &article.tag_id,
            &article.content,
            &article.created_at,
            &article.updated_at,
            article.atom_count
        ],
    )
    .map_err(|e| format!("Failed to insert article: {}", e))?;

    // Insert citations
    for citation in citations {
        conn.execute(
            "INSERT INTO wiki_citations (id, wiki_article_id, citation_index, atom_id, chunk_index, excerpt) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                &citation.id,
                &article.id,
                citation.citation_index,
                &citation.atom_id,
                citation.chunk_index,
                &citation.excerpt
            ],
        )
        .map_err(|e| format!("Failed to insert citation: {}", e))?;
    }

    // Insert wiki links
    for link in wiki_links {
        conn.execute(
            "INSERT INTO wiki_links (id, source_article_id, target_tag_name, target_tag_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                &link.id,
                &article.id,
                &link.target_tag_name,
                &link.target_tag_id,
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|e| format!("Failed to insert wiki link: {}", e))?;
    }

    Ok(())
}

/// Archive the current wiki article (if any) into wiki_article_versions
fn archive_existing_article(conn: &Connection, tag_id: &str) -> Result<(), String> {
    // Load existing article
    let existing: Option<(String, String, i32, String)> = conn
        .query_row(
            "SELECT id, content, atom_count, created_at FROM wiki_articles WHERE tag_id = ?1",
            [tag_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .ok();

    let (article_id, content, atom_count, created_at) = match existing {
        Some(e) => e,
        None => return Ok(()), // No existing article to archive
    };

    // Load citations for this article
    let mut stmt = conn
        .prepare(
            "SELECT id, citation_index, atom_id, chunk_index, excerpt FROM wiki_citations WHERE wiki_article_id = ?1 ORDER BY citation_index",
        )
        .map_err(|e| format!("Failed to prepare citation query: {}", e))?;
    let citations: Vec<WikiCitation> = stmt
        .query_map([&article_id], |row| {
            Ok(WikiCitation {
                id: row.get(0)?,
                citation_index: row.get(1)?,
                atom_id: row.get(2)?,
                chunk_index: row.get(3)?,
                excerpt: row.get(4)?,
                // Version archive read path; source_url not needed here.
                source_url: None,
            })
        })
        .map_err(|e| format!("Failed to query citations: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect citations: {}", e))?;

    // Compute next version number
    let next_version: i32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version_number), 0) + 1 FROM wiki_article_versions WHERE tag_id = ?1",
            [tag_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to compute version number: {}", e))?;

    // Serialize citations to JSON
    let citations_json = serde_json::to_string(&citations)
        .map_err(|e| format!("Failed to serialize citations: {}", e))?;

    // Insert version
    conn.execute(
        "INSERT INTO wiki_article_versions (id, tag_id, content, citations_json, atom_count, version_number, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            tag_id,
            content,
            citations_json,
            atom_count,
            next_version,
            created_at
        ],
    )
    .map_err(|e| format!("Failed to insert article version: {}", e))?;

    Ok(())
}

/// List version summaries for a tag, ordered by version_number descending
pub fn list_wiki_versions(
    conn: &Connection,
    tag_id: &str,
) -> Result<Vec<WikiVersionSummary>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, version_number, atom_count, created_at FROM wiki_article_versions WHERE tag_id = ?1 ORDER BY version_number DESC",
        )
        .map_err(|e| format!("Failed to prepare version query: {}", e))?;

    let versions = stmt
        .query_map([tag_id], |row| {
            Ok(WikiVersionSummary {
                id: row.get(0)?,
                version_number: row.get(1)?,
                atom_count: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(|e| format!("Failed to query versions: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect versions: {}", e))?;

    Ok(versions)
}

/// Get a single wiki article version by ID
pub fn get_wiki_version(
    conn: &Connection,
    version_id: &str,
) -> Result<Option<WikiArticleVersion>, String> {
    let result: Option<(String, String, String, String, i32, i32, String)> = conn
        .query_row(
            "SELECT id, tag_id, content, citations_json, atom_count, version_number, created_at FROM wiki_article_versions WHERE id = ?1",
            [version_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )
        .ok();

    match result {
        Some((id, tag_id, content, citations_json, atom_count, version_number, created_at)) => {
            let citations: Vec<WikiCitation> = serde_json::from_str(&citations_json)
                .map_err(|e| format!("Failed to deserialize citations: {}", e))?;

            Ok(Some(WikiArticleVersion {
                id,
                tag_id,
                content,
                citations,
                atom_count,
                version_number,
                created_at,
            }))
        }
        None => Ok(None),
    }
}

/// Load a wiki article with its citations from the database
pub fn load_wiki_article(
    conn: &Connection,
    tag_id: &str,
) -> Result<Option<WikiArticleWithCitations>, String> {
    // Get article
    let article: Option<WikiArticle> = conn
        .query_row(
            "SELECT id, tag_id, content, created_at, updated_at, atom_count FROM wiki_articles WHERE tag_id = ?1",
            [tag_id],
            |row| {
                Ok(WikiArticle {
                    id: row.get(0)?,
                    tag_id: row.get(1)?,
                    content: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    atom_count: row.get(5)?,
                })
            },
        )
        .ok();

    let article = match article {
        Some(a) => a,
        None => return Ok(None),
    };

    // Get citations, joining atoms for source_url so clients can render
    // citations differently based on the cited atom's origin (e.g. Obsidian
    // plugin rewriting them as wikilinks).
    let mut stmt = conn
        .prepare(
            "SELECT c.id, c.citation_index, c.atom_id, c.chunk_index, c.excerpt, a.source_url
             FROM wiki_citations c
             LEFT JOIN atoms a ON a.id = c.atom_id
             WHERE c.wiki_article_id = ?1
             ORDER BY c.citation_index",
        )
        .map_err(|e| format!("Failed to prepare citations query: {}", e))?;

    let citations: Vec<WikiCitation> = stmt
        .query_map([&article.id], |row| {
            Ok(WikiCitation {
                id: row.get(0)?,
                citation_index: row.get(1)?,
                atom_id: row.get(2)?,
                chunk_index: row.get(3)?,
                excerpt: row.get(4)?,
                source_url: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query citations: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect citations: {}", e))?;

    Ok(Some(WikiArticleWithCitations { article, citations }))
}

/// Get the status of a wiki article for a tag
pub fn get_article_status(conn: &Connection, tag_id: &str) -> Result<WikiArticleStatus, String> {
    // Count distinct atoms across this tag and all descendants using recursive CTE
    let current_atom_count: i32 = conn
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
        .map_err(|e| format!("Failed to count atoms: {}", e))?;

    // Get article info if exists
    let article_info: Option<(i32, String)> = conn
        .query_row(
            "SELECT atom_count, updated_at FROM wiki_articles WHERE tag_id = ?1",
            [tag_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    match article_info {
        Some((article_atom_count, updated_at)) => {
            let new_atoms = (current_atom_count - article_atom_count).max(0);
            Ok(WikiArticleStatus {
                has_article: true,
                article_atom_count,
                current_atom_count,
                new_atoms_available: new_atoms,
                updated_at: Some(updated_at),
            })
        }
        None => Ok(WikiArticleStatus {
            has_article: false,
            article_atom_count: 0,
            current_atom_count,
            new_atoms_available: 0,
            updated_at: None,
        }),
    }
}

/// Delete a wiki article for a tag
pub fn delete_article(conn: &Connection, tag_id: &str) -> Result<(), String> {
    conn.execute("DELETE FROM wiki_articles WHERE tag_id = ?1", [tag_id])
        .map_err(|e| format!("Failed to delete article: {}", e))?;
    Ok(())
}

/// Load all wiki articles with tag names for list view, sorted by importance
pub fn load_all_wiki_articles(conn: &Connection) -> Result<Vec<WikiArticleSummary>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT w.id, w.tag_id, t.name as tag_name, w.updated_at, w.atom_count,
                    (SELECT COUNT(*) FROM wiki_links wl WHERE wl.target_tag_id = w.tag_id) as inbound_links
             FROM wiki_articles w
             JOIN tags t ON w.tag_id = t.id
             ORDER BY inbound_links DESC, w.atom_count DESC, w.updated_at DESC",
        )
        .map_err(|e| format!("Failed to prepare wiki articles query: {}", e))?;

    let articles: Vec<WikiArticleSummary> = stmt
        .query_map([], |row| {
            Ok(WikiArticleSummary {
                id: row.get(0)?,
                tag_id: row.get(1)?,
                tag_name: row.get(2)?,
                updated_at: row.get(3)?,
                atom_count: row.get(4)?,
                inbound_links: row.get(5)?,
            })
        })
        .map_err(|e| format!("Failed to query wiki articles: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect wiki articles: {}", e))?;

    Ok(articles)
}

/// Extract [[wiki links]] from article content and resolve to known tags
pub fn extract_wiki_links(
    article_id: &str,
    content: &str,
    known_tags: &[(String, String)], // (tag_id, tag_name)
) -> Vec<WikiLink> {
    let re = match Regex::new(r"\[\[([^\]]+)\]\]") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut links: Vec<WikiLink> = Vec::new();
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for cap in re.captures_iter(content) {
        if let Some(name_match) = cap.get(1) {
            let link_name = name_match.as_str().trim().to_string();
            let lower_name = link_name.to_lowercase();

            if seen_names.contains(&lower_name) {
                continue;
            }
            seen_names.insert(lower_name.clone());

            // Try to resolve to a known tag (case-insensitive)
            let target_tag_id = known_tags
                .iter()
                .find(|(_, name)| name.to_lowercase() == lower_name)
                .map(|(tag_id, _)| tag_id.clone());

            links.push(WikiLink {
                id: Uuid::new_v4().to_string(),
                source_article_id: article_id.to_string(),
                target_tag_name: link_name,
                target_tag_id,
                has_article: false, // resolved dynamically at read time
            });
        }
    }

    links
}

/// Load wiki links for an article (outgoing cross-references)
pub fn load_wiki_links(conn: &Connection, tag_id: &str) -> Result<Vec<WikiLink>, String> {
    // Scalar subquery finds article_id via UNIQUE index on wiki_articles(tag_id).
    // If no article exists, the subquery returns NULL and the WHERE matches nothing —
    // SQLite short-circuits without touching wiki_links at all.
    let mut stmt = conn
        .prepare(
            "SELECT wl.id, wl.source_article_id, wl.target_tag_name,
                    COALESCE(wl.target_tag_id, t.id) as resolved_tag_id,
                    CASE WHEN wa.id IS NOT NULL THEN 1 ELSE 0 END as has_article
             FROM wiki_links wl
             LEFT JOIN tags t ON t.name = wl.target_tag_name COLLATE NOCASE AND wl.target_tag_id IS NULL
             LEFT JOIN wiki_articles wa ON wa.tag_id = COALESCE(wl.target_tag_id, t.id)
             WHERE wl.source_article_id = (SELECT id FROM wiki_articles WHERE tag_id = ?1)",
        )
        .map_err(|e| format!("Failed to prepare wiki links query: {}", e))?;

    let links: Vec<WikiLink> = stmt
        .query_map([tag_id], |row| {
            Ok(WikiLink {
                id: row.get(0)?,
                source_article_id: row.get(1)?,
                target_tag_name: row.get(2)?,
                target_tag_id: row.get(3)?,
                has_article: row.get::<_, i32>(4)? == 1,
            })
        })
        .map_err(|e| format!("Failed to query wiki links: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect wiki links: {}", e))?;

    Ok(links)
}

/// Get tags related to a given tag, ranked by semantic connectivity
///
/// Uses three signals:
/// 1. Semantic edges between atoms in different tags (weight: 0.4)
/// 2. Shared atoms (tagged with both tags) (weight: 0.3)
/// 3. Tag centroid embedding similarity (weight: 0.3)
pub fn get_related_tags(
    conn: &Connection,
    tag_id: &str,
    limit: usize,
) -> Result<Vec<RelatedTag>, String> {
    // Get hierarchy for exclusion set.
    let source_tag_ids = get_tag_hierarchy(conn, tag_id)?;
    if source_tag_ids.is_empty() {
        return Ok(Vec::new());
    }

    let exclude_set: std::collections::HashSet<&str> =
        source_tag_ids.iter().map(|s| s.as_str()).collect();

    let mut tags: Vec<RelatedTag> = Vec::new();
    let mut tag_map: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    // === Signal 1: Shared atoms (co-occurrence) — cheap self-join (~1ms) ===
    {
        let mut stmt = conn
            .prepare(
                "SELECT t.id, t.name, COUNT(DISTINCT at1.atom_id) as shared_count,
                        CASE WHEN wa.id IS NOT NULL THEN 1 ELSE 0 END as has_article
                 FROM atom_tags at1
                 JOIN atom_tags at2 ON at1.atom_id = at2.atom_id
                 JOIN tags t ON at2.tag_id = t.id
                 LEFT JOIN wiki_articles wa ON t.id = wa.tag_id
                 WHERE at1.tag_id IN (SELECT id FROM tags WHERE id = ?1 OR parent_id = ?1)
                   AND at2.tag_id NOT IN (SELECT id FROM tags WHERE id = ?1 OR parent_id = ?1)
                   AND t.parent_id IS NOT NULL
                 GROUP BY at2.tag_id
                 ORDER BY shared_count DESC
                 LIMIT ?2",
            )
            .map_err(|e| format!("Failed to prepare shared atoms query: {}", e))?;

        let shared_limit = (limit * 3).max(30) as i32;
        let rows: Vec<RelatedTag> = stmt
            .query_map(rusqlite::params![tag_id, shared_limit], |row| {
                let shared_atoms: i32 = row.get(2)?;
                Ok(RelatedTag {
                    tag_id: row.get(0)?,
                    tag_name: row.get(1)?,
                    score: (shared_atoms as f64) * 0.4,
                    shared_atoms,
                    semantic_edges: 0,
                    has_article: row.get::<_, i32>(3)? == 1,
                })
            })
            .map_err(|e| format!("Failed to query shared atoms: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect shared atoms: {}", e))?;

        for tag in rows {
            tag_map.insert(tag.tag_id.clone(), tags.len());
            tags.push(tag);
        }
    }

    // === Signal 2: Tag centroid embedding similarity (primary signal) ===
    let source_embedding: Option<Vec<u8>> = conn
        .query_row(
            "SELECT embedding FROM tag_embeddings WHERE tag_id = ?1",
            [tag_id],
            |row| row.get(0),
        )
        .ok();

    if let Some(ref source_blob) = source_embedding {
        let centroid_limit = (limit * 3).max(30) as i32;
        let mut vec_stmt = conn
            .prepare(
                "SELECT tag_id, distance
                 FROM vec_tags
                 WHERE embedding MATCH ?1
                 ORDER BY distance
                 LIMIT ?2",
            )
            .map_err(|e| format!("Failed to prepare vec_tags query: {}", e))?;

        let centroid_results: Vec<(String, f32)> = vec_stmt
            .query_map(rusqlite::params![source_blob, centroid_limit], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|e| format!("Failed to query vec_tags: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect vec_tags results: {}", e))?;

        let mut new_candidates: Vec<(String, f64)> = Vec::new();
        for (candidate_tag_id, distance) in &centroid_results {
            if exclude_set.contains(candidate_tag_id.as_str()) {
                continue;
            }
            let centroid_sim = crate::embedding::distance_to_similarity(*distance) as f64;
            if centroid_sim < 0.3 {
                continue;
            }
            let centroid_score = centroid_sim * 0.6;

            if let Some(&idx) = tag_map.get(candidate_tag_id) {
                tags[idx].score += centroid_score;
            } else {
                new_candidates.push((candidate_tag_id.clone(), centroid_score));
            }
        }

        // Batch lookup metadata for new centroid-only candidates
        if !new_candidates.is_empty() {
            let placeholders = new_candidates
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let query = format!(
                "SELECT t.id, t.name, CASE WHEN wa.id IS NOT NULL THEN 1 ELSE 0 END
                 FROM tags t
                 LEFT JOIN wiki_articles wa ON t.id = wa.tag_id
                 WHERE t.id IN ({}) AND t.parent_id IS NOT NULL",
                placeholders
            );
            let mut meta_stmt = conn
                .prepare(&query)
                .map_err(|e| format!("Failed to prepare centroid metadata query: {}", e))?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> = new_candidates
                .iter()
                .map(|(id, _)| id as &dyn rusqlite::types::ToSql)
                .collect();
            let meta_rows: Vec<(String, String, bool)> = meta_stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get::<_, i32>(2)? == 1))
                })
                .map_err(|e| format!("Failed to query centroid metadata: {}", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Failed to collect centroid metadata: {}", e))?;

            let score_map: std::collections::HashMap<&str, f64> = new_candidates
                .iter()
                .map(|(id, score)| (id.as_str(), *score))
                .collect();

            for (id, name, has_article) in meta_rows {
                let centroid_score = score_map.get(id.as_str()).copied().unwrap_or(0.0);
                tag_map.insert(id.clone(), tags.len());
                tags.push(RelatedTag {
                    tag_id: id,
                    tag_name: name,
                    score: centroid_score,
                    shared_atoms: 0,
                    semantic_edges: 0,
                    has_article,
                });
            }
        }
    }

    // === Signal 3: Content mentions ===
    // Tags whose names appear in this article's content (cheap string matching).
    let content_tags = find_tags_mentioned_in_article(conn, tag_id, &source_tag_ids, limit)?;
    for ct in content_tags {
        if !tag_map.contains_key(&ct.tag_id) {
            tag_map.insert(ct.tag_id.clone(), tags.len());
            tags.push(ct);
        }
    }

    // Sort by score and truncate
    tags.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    tags.truncate(limit);

    Ok(tags)
}

/// Find tags whose names appear in a wiki article's content
fn find_tags_mentioned_in_article(
    conn: &Connection,
    tag_id: &str,
    exclude_tag_ids: &[String],
    limit: usize,
) -> Result<Vec<RelatedTag>, String> {
    // Get article content — early return if no article (blank page)
    let content: Option<String> = conn
        .query_row(
            "SELECT content FROM wiki_articles WHERE tag_id = ?1",
            [tag_id],
            |row| row.get(0),
        )
        .ok();

    let content = match content {
        Some(c) => c,
        None => return Ok(Vec::new()),
    };
    let content_lower = content.to_lowercase();

    // Step 1: Fetch candidate tags cheaply (no correlated subquery for atom counts).
    // We filter by name match in Rust, so most rows are discarded — no point counting atoms for all of them.
    let placeholders = exclude_tag_ids
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT t.id, t.name,
                CASE WHEN wa.id IS NOT NULL THEN 1 ELSE 0 END as has_article
         FROM tags t
         LEFT JOIN wiki_articles wa ON t.id = wa.tag_id
         WHERE t.parent_id IS NOT NULL
           AND t.id NOT IN ({})
           AND length(t.name) >= 3
           AND t.name GLOB '*[^0-9]*'",
        placeholders
    );

    let mut stmt = conn
        .prepare(&query)
        .map_err(|e| format!("Failed to prepare content mention query: {}", e))?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = exclude_tag_ids
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    // Filter to only tags whose names appear as whole words in article content
    let matched_tags: Vec<(String, String, bool)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i32>(2)? == 1,
            ))
        })
        .map_err(|e| format!("Failed to query content mentions: {}", e))?
        .filter_map(|r| r.ok())
        .filter(|(_, name, _)| {
            let name_lower = name.to_lowercase();
            if let Some(pos) = content_lower.find(&name_lower) {
                let before_ok =
                    pos == 0 || !content_lower.as_bytes()[pos - 1].is_ascii_alphanumeric();
                let end = pos + name_lower.len();
                let after_ok = end >= content_lower.len()
                    || !content_lower.as_bytes()[end].is_ascii_alphanumeric();
                before_ok && after_ok
            } else {
                false
            }
        })
        .collect();

    if matched_tags.is_empty() {
        return Ok(Vec::new());
    }

    // Step 2: Batch-fetch atom counts only for matched tags (typically a handful).
    let count_placeholders = matched_tags
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let count_query = format!(
        "SELECT tag_id, COUNT(*) FROM atom_tags WHERE tag_id IN ({}) GROUP BY tag_id",
        count_placeholders
    );
    let mut count_stmt = conn
        .prepare(&count_query)
        .map_err(|e| format!("Failed to prepare atom count query: {}", e))?;
    let count_params: Vec<&dyn rusqlite::types::ToSql> = matched_tags
        .iter()
        .map(|(id, _, _)| id as &dyn rusqlite::types::ToSql)
        .collect();
    let count_map: std::collections::HashMap<String, i32> = count_stmt
        .query_map(count_params.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?))
        })
        .map_err(|e| format!("Failed to query atom counts: {}", e))?
        .filter_map(|r| r.ok())
        .collect();

    let mut mentioned: Vec<RelatedTag> = matched_tags
        .into_iter()
        .map(|(id, name, has_article)| {
            let atom_count = count_map.get(&id).copied().unwrap_or(0);
            RelatedTag {
                tag_id: id,
                tag_name: name,
                score: atom_count as f64 * 0.1,
                shared_atoms: 0,
                semantic_edges: 0,
                has_article,
            }
        })
        .collect();

    mentioned.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    mentioned.truncate(limit);

    Ok(mentioned)
}

/// Get suggested wiki articles: tags without articles ranked by demand + content richness
pub fn get_suggested_wiki_articles(
    conn: &Connection,
    limit: i32,
) -> Result<Vec<SuggestedArticle>, String> {
    let mut stmt = conn
        .prepare(
            "WITH link_mentions AS (
                -- Drive from wiki_links (small), not from all candidate tags
                SELECT tag_id, SUM(cnt) as link_count FROM (
                    SELECT wl.target_tag_id as tag_id, COUNT(*) as cnt
                    FROM wiki_links wl
                    WHERE wl.target_tag_id IS NOT NULL
                    GROUP BY wl.target_tag_id
                    UNION ALL
                    SELECT t2.id as tag_id, COUNT(*) as cnt
                    FROM wiki_links wl
                    JOIN tags t2 ON wl.target_tag_name = t2.name COLLATE NOCASE
                    WHERE wl.target_tag_id IS NULL
                    GROUP BY t2.id
                )
                GROUP BY tag_id
            )
            SELECT
                t.id,
                t.name,
                t.atom_count,
                COALESCE(lm.link_count, 0) as mention_count,
                t.atom_count * 1.0 + COALESCE(lm.link_count, 0) * 3.0 as score
            FROM tags t
            LEFT JOIN link_mentions lm ON lm.tag_id = t.id
            WHERE t.parent_id IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM wiki_articles wa WHERE wa.tag_id = t.id)
              AND t.name GLOB '*[^0-9]*'
              AND length(t.name) >= 2
              AND t.atom_count > 0
            ORDER BY score DESC
            LIMIT ?1",
        )
        .map_err(|e| format!("Failed to prepare suggestions query: {}", e))?;

    let suggestions: Vec<SuggestedArticle> = stmt
        .query_map([limit], |row| {
            Ok(SuggestedArticle {
                tag_id: row.get(0)?,
                tag_name: row.get(1)?,
                atom_count: row.get(2)?,
                mention_count: row.get(3)?,
                score: row.get(4)?,
            })
        })
        .map_err(|e| format!("Failed to query suggestions: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect suggestions: {}", e))?;

    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database as CoreDatabase;
    use crate::providers::structured::lint_schema;
    use tempfile::NamedTempFile;

    fn create_test_db() -> (CoreDatabase, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db = CoreDatabase::open_or_create(temp_file.path()).unwrap();
        (db, temp_file)
    }

    // ==================== Schema lint regression tests ====================
    //
    // These guard against someone editing a schema in a way that would cause
    // silent cross-provider divergence (wiki has been bitten by this before —
    // see the long comment on `section_ops_schema`). The linter lives in
    // `providers::structured::lint_schema`.

    #[test]
    fn lint_wiki_generation_schema() {
        lint_schema(&wiki_generation_schema())
            .expect("wiki_generation_schema must be portable across providers");
    }

    #[test]
    fn lint_wiki_section_ops_schema() {
        lint_schema(&section_ops_schema())
            .expect("section_ops_schema must be portable across providers");
    }

    fn insert_tag(conn: &Connection, id: &str, name: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO tags (id, name, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, name, now],
        )
        .unwrap();
    }

    fn insert_atom(conn: &Connection, id: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO atoms (id, content, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, "test content", now, now],
        )
        .unwrap();
    }

    #[test]
    fn test_save_and_load_wiki_article() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        // Create a tag and atom first
        insert_tag(&conn, "tag1", "TestTopic");
        insert_atom(&conn, "atom1");

        // Create article
        let now = chrono::Utc::now().to_rfc3339();
        let article = WikiArticle {
            id: "article1".to_string(),
            tag_id: "tag1".to_string(),
            content: "This is a test article with [1] citation.".to_string(),
            created_at: now.clone(),
            updated_at: now,
            atom_count: 5,
        };

        let citations = vec![WikiCitation {
            id: "citation1".to_string(),
            citation_index: 1,
            atom_id: "atom1".to_string(),
            chunk_index: Some(0),
            excerpt: "Source text here".to_string(),
            source_url: None,
        }];

        // Save
        save_wiki_article(&conn, &article, &citations, &[]).unwrap();

        // Load
        let loaded = load_wiki_article(&conn, "tag1").unwrap();
        assert!(loaded.is_some(), "Article should be found");

        let loaded = loaded.unwrap();
        assert_eq!(loaded.article.content, article.content);
        assert_eq!(loaded.citations.len(), 1);
        assert_eq!(loaded.citations[0].excerpt, "Source text here");
    }

    #[test]
    fn test_get_article_status_no_article() {
        let (db, _temp) = create_test_db();
        let conn = db.conn.lock().unwrap();

        // Create a tag without an article
        insert_tag(&conn, "tag1", "TestTopic");

        let status = get_article_status(&conn, "tag1").unwrap();

        assert!(!status.has_article, "Should have no article");
        assert_eq!(status.article_atom_count, 0);
        assert!(status.updated_at.is_none(), "Should have no update time");
    }

    #[test]
    fn test_extract_citations_basic() {
        let chunks = vec![
            ChunkWithContext {
                atom_id: "atom1".to_string(),
                chunk_index: 0,
                content: "First chunk content".to_string(),
                similarity_score: 0.9,
            },
            ChunkWithContext {
                atom_id: "atom2".to_string(),
                chunk_index: 0,
                content: "Second chunk content".to_string(),
                similarity_score: 0.85,
            },
        ];

        let content = "This is text [1] and more text [2].";
        let citations = extract_citations("article1", content, &chunks).unwrap();

        assert_eq!(citations.len(), 2, "Should find 2 citations");
        assert_eq!(citations[0].citation_index, 1);
        assert_eq!(citations[0].atom_id, "atom1");
        assert_eq!(citations[1].citation_index, 2);
        assert_eq!(citations[1].atom_id, "atom2");
    }

    #[test]
    fn test_extract_citations_deduplicates() {
        let chunks = vec![ChunkWithContext {
            atom_id: "atom1".to_string(),
            chunk_index: 0,
            content: "Chunk content".to_string(),
            similarity_score: 0.9,
        }];

        // Same citation appears multiple times
        let content = "Statement one [1] and statement two [1] and statement three [1].";
        let citations = extract_citations("article1", content, &chunks).unwrap();

        assert_eq!(
            citations.len(),
            1,
            "Should deduplicate repeated citation indices"
        );
        assert_eq!(citations[0].citation_index, 1);
    }
}
