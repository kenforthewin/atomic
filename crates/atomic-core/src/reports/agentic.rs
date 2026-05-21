//! Generalized agentic research loop for report runs.
//!
//! Reads from a source batch + parameterized context corpus, produces a
//! prose finding with `[N]` citation markers, and returns a typed
//! `RunOutput` the runner persists transactionally.
//!
//! The agent is intentionally given the same three tools the existing
//! daily briefing uses (`read_atom`, `semantic_search`, `done`). The
//! differences live in the citable-evidence map and the search filter:
//!
//! - Source atoms are pre-numbered `[1]..[N]` so `source_only` reports
//!   have a fixed citation surface.
//! - Under `source_and_context`, every `semantic_search` result is
//!   assigned the next available number on first appearance and surfaced
//!   to the agent alongside title/snippet. Repeat hits reuse the number.
//! - `semantic_search` results are post-filtered by the report's
//!   context scope (tag subtree, time window, kinds, self-exclusion).
//!   The agent never sees atoms outside scope.
//!
//! The structured-output JSON shape mirrors the briefing's
//! `BriefingGenerationResult` so the existing tolerant-parsing helper
//! (`call_structured`) does the heavy lifting.

use crate::error::AtomicCoreError;
use crate::models::{AtomWithTags, CitationPolicy, Report};
use crate::providers::structured::{call_structured, StructuredCall};
use crate::providers::types::{CompletionResponse, Message, MessageRole, ToolDefinition};
use crate::providers::{get_llm_provider, LlmConfig, ProviderConfig, ProviderType};
use crate::reports::scope::{ContextFilter, TimeWindow};
use crate::search::{SearchMode, SearchOptions};
use crate::AtomicCore;

use regex::Regex;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// Default cap on tool-calling iterations. Reports can opt up via the
/// `max_tool_iterations` column; the cap exists so a runaway agent
/// burning tool calls cannot melt down the LLM budget unbounded.
const DEFAULT_MAX_ITERATIONS: usize = 10;
/// Inline snippet length used in prompt construction and tool responses.
const SNIPPET_LEN: usize = 200;
/// Excerpt length stored in `report_finding_citations.excerpt` — slightly
/// longer than the prompt snippet because the UI may render it directly.
const EXCERPT_LEN: usize = 300;
const DEFAULT_SEARCH_LIMIT: i64 = 5;
const MAX_SEARCH_LIMIT: i64 = 10;
const DEFAULT_READ_LIMIT: i64 = 500;
const MAX_READ_LIMIT: i64 = 500;

/// One entry in the run's citable-evidence map. Source atoms are loaded
/// up front; context atoms (when citable) accrue during the agent loop.
#[derive(Debug, Clone)]
struct Citable {
    number: i32,
    atom_id: String,
    excerpt: String,
}

/// What the agent ultimately produced. The runner persists this — this
/// module never touches storage so it can be exercised against a mock
/// LLM without a DB.
#[derive(Debug)]
pub struct RunOutput {
    /// Final prose, with `[N]` citation markers.
    pub content: String,
    /// Resolved citations in marker-position order.
    pub citations: Vec<ResolvedCitation>,
}

#[derive(Debug, Clone)]
pub struct ResolvedCitation {
    pub position: i32,
    pub cited_atom_id: String,
    pub excerpt: String,
}

#[derive(Debug, Deserialize)]
struct ReportGenerationResult {
    finding_content: String,
    #[allow(dead_code)]
    #[serde(default)]
    citations_used: Vec<i32>,
}

fn report_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "finding_content": {
                "type": "string",
                "description": "The finding in markdown, with [N] citation markers."
            },
            "citations_used": {
                "type": "array",
                "items": { "type": "integer" },
                "description": "List of citation numbers actually used."
            }
        },
        "required": ["finding_content", "citations_used"],
        "additionalProperties": false
    })
}

const SYSTEM_PROMPT_SCAFFOLD: &str = "You are running a scheduled research report over a personal knowledge base.

You will receive a numbered list of source atoms (your primary evidence) and a research prompt describing what to investigate. You may use the provided tools to read individual atoms in full and to search the broader corpus for context.

Tools:
- read_atom(atom_id, limit?, offset?): Read a window of lines from an atom's markdown.
- semantic_search(query, limit): Search the configured context corpus. Returns titles and snippets. Each result has a citation number — for `source_only` reports these numbers are not citable; for `source_and_context` reports they are. The tool response will tell you which.
- done(): Signal that you have enough material and will now write the report.

Citation conventions:
- Cite using [N] inline markers. The N refers to the numbered position in the source list (and, under source_and_context, the numbers assigned to search results as they are surfaced).
- Do not invent citation numbers. Only cite atoms you actually saw via the source list or a tool response.
- Skip atoms that aren't relevant. Length should match what the research prompt asks for.

Call done() before writing the final report.";

fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let boundary = s
        .char_indices()
        .take_while(|(i, _)| *i < max)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let mut out = s[..boundary].to_string();
    out.push_str("...");
    out
}

fn snippet_for(atom: &AtomWithTags) -> String {
    let src = if !atom.atom.snippet.is_empty() {
        atom.atom.snippet.as_str()
    } else {
        atom.atom.content.as_str()
    };
    let cleaned: String = src
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    truncate_on_char_boundary(cleaned.trim(), SNIPPET_LEN)
}

fn excerpt_for(atom: &AtomWithTags) -> String {
    let src = if !atom.atom.snippet.is_empty() {
        atom.atom.snippet.as_str()
    } else {
        atom.atom.content.as_str()
    };
    truncate_on_char_boundary(src.trim(), EXCERPT_LEN)
}

fn report_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "read_atom",
            "Read a window of lines from an atom's markdown content. Page through with offset for long atoms.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "atom_id": { "type": "string" },
                    "limit": { "type": "integer", "default": 500 },
                    "offset": { "type": "integer", "default": 0 }
                },
                "required": ["atom_id"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::new(
            "semantic_search",
            "Search the context corpus configured for this report. Each result has a citation number; whether you may cite it is shown in the response.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 5 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::new(
            "done",
            "Signal that research is complete. Call this before writing the final report.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
    ]
}

fn build_user_prompt(report: &Report, source: &[AtomWithTags], total_in_scope: i32) -> String {
    let mut out = String::new();
    out.push_str("RESEARCH PROMPT:\n");
    out.push_str(&report.research_prompt);
    out.push_str("\n\n");

    if source.is_empty() {
        out.push_str("(no source atoms — this should be unreachable; the runner short-circuits empty scopes)\n");
        return out;
    }

    out.push_str(&format!(
        "SOURCE ATOMS ({} of {} in scope):\n",
        source.len(),
        total_in_scope
    ));
    if total_in_scope as usize > source.len() {
        out.push_str("(showing the newest within the configured cap; older atoms truncated)\n\n");
    }
    for (i, atom) in source.iter().enumerate() {
        let title = if atom.atom.title.is_empty() {
            "(untitled)".to_string()
        } else {
            atom.atom.title.clone()
        };
        out.push_str(&format!(
            "[{}] {}\n    {}\n    (atom id: {})\n\n",
            i + 1,
            title,
            snippet_for(atom),
            atom.atom.id,
        ));
    }
    out.push_str(&format!(
        "Citation policy: {}\n",
        match report.citation_policy {
            CitationPolicy::SourceOnly =>
                "source_only — only the [N] above may be cited; search results are background only.",
            CitationPolicy::SourceAndContext =>
                "source_and_context — search results will be assigned [N] numbers and become citable.",
        }
    ));
    out
}

async fn handle_read_atom(core: &AtomicCore, args: &serde_json::Value) -> String {
    let Some(atom_id) = args.get("atom_id").and_then(|v| v.as_str()) else {
        return "Error: atom_id is required".to_string();
    };
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_READ_LIMIT)
        .clamp(1, MAX_READ_LIMIT) as usize;
    let offset = args
        .get("offset")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0) as usize;

    let atom = match core.get_atom(atom_id).await {
        Ok(Some(a)) => a,
        Ok(None) => return format!("Error: no atom found with id {}", atom_id),
        Err(e) => return format!("Error fetching atom {}: {}", atom_id, e),
    };

    let title = if atom.atom.title.is_empty() {
        "(untitled)"
    } else {
        atom.atom.title.as_str()
    };
    let lines: Vec<&str> = atom.atom.content.lines().collect();
    let total_lines = lines.len();
    let start = offset.min(total_lines);
    let end = (start + limit).min(total_lines);
    let has_more = end < total_lines;

    let mut out = format!(
        "# {}\n(lines {}-{} of {})\n\n",
        title,
        start + 1,
        end,
        total_lines
    );
    out.push_str(&lines[start..end].join("\n"));
    if has_more {
        out.push_str(&format!(
            "\n\n(More content available. Call read_atom again with offset={} to continue.)",
            end
        ));
    }
    out
}

fn passes_context_filter(atom: &AtomWithTags, ctx: &ContextFilter) -> bool {
    if ctx.excluded_atom_ids.contains(&atom.atom.id) {
        return false;
    }
    match &ctx.time_window {
        None => {}
        Some(TimeWindow::Before(cutoff)) => {
            if atom.atom.created_at.as_str() >= cutoff.as_str() {
                return false;
            }
        }
        Some(TimeWindow::After(cutoff)) => {
            if atom.atom.created_at.as_str() <= cutoff.as_str() {
                return false;
            }
        }
    }
    // Kind filter — `Only(vec![])` is defensively "match nothing".
    match &ctx.kinds {
        crate::models::KindFilter::All => {}
        crate::models::KindFilter::Only(kinds) => {
            if kinds.is_empty() || !kinds.contains(&atom.atom.kind) {
                return false;
            }
        }
    }
    true
}

/// Pre-compute the set of atom ids allowed by the context tag scope.
/// Returns `None` when no tag scope is configured (every atom passes).
async fn build_context_tag_scope_set(
    core: &AtomicCore,
    ctx: &ContextFilter,
) -> Result<Option<HashSet<String>>, AtomicCoreError> {
    if ctx.tag_ids.is_empty() {
        return Ok(None);
    }
    // Full subtree, no time bound, no caps — we're building a membership
    // predicate, not the source batch. Kinds are applied at the
    // post-filter stage so this set stays purely structural.
    let atoms = core
        .storage()
        .list_atoms_for_report_scope_sync(&ctx.tag_ids, None, &crate::models::KindFilter::All, None)
        .await?;
    Ok(Some(atoms.into_iter().map(|a| a.atom.id).collect()))
}

struct AgentState {
    messages: Vec<Message>,
    done_called: bool,
    /// Map atom_id → Citable. Owned, ordered by `number`.
    citables: HashMap<String, Citable>,
    next_citation_number: i32,
    citation_policy: CitationPolicy,
}

impl AgentState {
    fn from_source(report: &Report, source: &[AtomWithTags]) -> Self {
        let mut citables = HashMap::new();
        for (i, atom) in source.iter().enumerate() {
            let number = (i + 1) as i32;
            citables.insert(
                atom.atom.id.clone(),
                Citable {
                    number,
                    atom_id: atom.atom.id.clone(),
                    excerpt: excerpt_for(atom),
                },
            );
        }
        let next_citation_number = source.len() as i32 + 1;
        AgentState {
            messages: Vec::new(),
            done_called: false,
            citables,
            next_citation_number,
            citation_policy: report.citation_policy,
        }
    }

    /// Return the citation number for `atom`, assigning a new one if this
    /// is a fresh context atom under `source_and_context`.
    fn citation_for_search_result(&mut self, atom: &AtomWithTags) -> Option<i32> {
        if let Some(c) = self.citables.get(&atom.atom.id) {
            return Some(c.number);
        }
        if self.citation_policy == CitationPolicy::SourceAndContext {
            let number = self.next_citation_number;
            self.next_citation_number += 1;
            self.citables.insert(
                atom.atom.id.clone(),
                Citable {
                    number,
                    atom_id: atom.atom.id.clone(),
                    excerpt: excerpt_for(atom),
                },
            );
            Some(number)
        } else {
            None
        }
    }
}

async fn handle_semantic_search(
    core: &AtomicCore,
    state: &mut AgentState,
    ctx: &ContextFilter,
    tag_scope_set: &Option<HashSet<String>>,
    args: &serde_json::Value,
) -> String {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if query.is_empty() {
        return "Error: query is required".to_string();
    }
    // Over-fetch slightly so post-filter losses don't leave the agent
    // with an empty result set on tag-scoped reports.
    let requested = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT) as i32;
    let fetch = (requested.saturating_mul(3)).min(MAX_SEARCH_LIMIT as i32);

    let options = SearchOptions::new(query.clone(), SearchMode::Semantic, fetch);
    let results = match core.search(options).await {
        Ok(r) => r,
        Err(e) => return format!("Search error: {}", e),
    };

    let mut shown: Vec<(i32, AtomWithTags, f32, String)> = Vec::new();
    for r in results {
        if !passes_context_filter(&r.atom, ctx) {
            continue;
        }
        if let Some(set) = tag_scope_set {
            if !set.contains(&r.atom.atom.id) {
                continue;
            }
        }
        let snippet = truncate_on_char_boundary(
            &r.matching_chunk_content
                .chars()
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect::<String>(),
            SNIPPET_LEN,
        );
        // source_only + non-source atom returns None → surface as
        // uncitable background context (encoded as 0 in the marker).
        let number = state.citation_for_search_result(&r.atom).unwrap_or(0);
        shown.push((number, r.atom.clone(), r.similarity_score, snippet));
        if shown.len() >= requested as usize {
            break;
        }
    }

    if shown.is_empty() {
        return "No results in context scope.".to_string();
    }

    let mut out = String::new();
    for (number, atom, score, snippet) in &shown {
        let title = if atom.atom.title.is_empty() {
            "(untitled)"
        } else {
            atom.atom.title.as_str()
        };
        let citation_marker = if *number > 0 {
            format!("[{number}] citable")
        } else {
            "(context only, not citable)".to_string()
        };
        out.push_str(&format!(
            "{}. {}\n   {}\n   (atom id: {}, score: {:.2})\n   {}\n\n",
            citation_marker, title, snippet, atom.atom.id, score, snippet
        ));
    }
    out
}

async fn resolve_model(core: &AtomicCore) -> Result<(ProviderConfig, String), AtomicCoreError> {
    let settings = core.get_settings().await?;
    let config = ProviderConfig::from_settings(&settings);
    let model = match config.provider_type {
        ProviderType::Ollama => config.llm_model().to_string(),
        ProviderType::OpenAICompat => config.llm_model().to_string(),
        ProviderType::OpenRouter => settings
            .get("wiki_model")
            .cloned()
            .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string()),
    };
    Ok((config, model))
}

async fn run_research(
    core: &AtomicCore,
    state: &mut AgentState,
    ctx: &ContextFilter,
    tag_scope_set: &Option<HashSet<String>>,
    provider_config: &ProviderConfig,
    model: &str,
    max_iters: usize,
) -> Result<(), AtomicCoreError> {
    let tools = report_tools();
    let llm_config = LlmConfig::new(model);
    let provider = get_llm_provider(provider_config)
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    for iteration in 0..max_iters {
        tracing::debug!(
            iteration = iteration + 1,
            max = max_iters,
            "[reports/agentic] Research iteration"
        );

        let response: CompletionResponse = provider
            .complete_with_tools(&state.messages, &tools, &llm_config)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("report research LLM call failed: {e}"))
            })?;

        let tool_calls = match response.tool_calls {
            Some(ref tcs) if !tcs.is_empty() => tcs.clone(),
            _ => break,
        };

        state.messages.push(Message {
            role: MessageRole::Assistant,
            content: if response.content.is_empty() {
                None
            } else {
                Some(response.content.clone())
            },
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
            name: None,
        });

        let mut done_this_round = false;
        for tc in &tool_calls {
            let name = tc.get_name().unwrap_or("");
            let args: serde_json::Value = tc
                .get_arguments()
                .and_then(|a| serde_json::from_str(a).ok())
                .unwrap_or(serde_json::json!({}));

            let result = match name {
                "read_atom" => handle_read_atom(core, &args).await,
                "semantic_search" => {
                    handle_semantic_search(core, state, ctx, tag_scope_set, &args).await
                }
                "done" => {
                    done_this_round = true;
                    state.done_called = true;
                    "Acknowledged. Write the report in your next message.".to_string()
                }
                _ => format!("Unknown tool: {}", name),
            };
            state
                .messages
                .push(Message::tool_result(tc.id.clone(), result));
        }

        if done_this_round {
            break;
        }
    }
    Ok(())
}

async fn final_pass(
    provider_config: &ProviderConfig,
    model: &str,
    messages: &[Message],
) -> Result<String, AtomicCoreError> {
    let call = StructuredCall::<ReportGenerationResult>::new(
        provider_config,
        model,
        messages,
        "report_generation_result",
        report_schema(),
    );
    match call_structured::<ReportGenerationResult>(call).await {
        Ok(result) => Ok(result.finding_content),
        Err(e) => Err(AtomicCoreError::DatabaseOperation(format!(
            "final report pass failed: {}",
            e.to_compact_string()
        ))),
    }
}

/// Resolve each `[N]` marker in `content` to a `(position, atom_id,
/// excerpt)` tuple. Markers that don't map to a known citable are dropped
/// with a warning — same behavior as the briefing's extractor. Position
/// is 1-indexed by appearance order in the prose, so the same atom cited
/// twice yields two rows with different positions but the same atom_id.
fn extract_citations(content: &str, citables: &HashMap<String, Citable>) -> Vec<ResolvedCitation> {
    let re = match Regex::new(r"\[(\d+)\]") {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "[reports/agentic] citation regex compile");
            return vec![];
        }
    };
    let by_number: HashMap<i32, &Citable> = citables.values().map(|c| (c.number, c)).collect();
    let mut out: Vec<ResolvedCitation> = Vec::new();
    let mut position: i32 = 1;
    for cap in re.captures_iter(content) {
        let Some(m) = cap.get(1) else { continue };
        let Ok(n) = m.as_str().parse::<i32>() else {
            continue;
        };
        let Some(c) = by_number.get(&n) else {
            tracing::warn!(
                citation_number = n,
                "[reports/agentic] Agent produced unknown citation; dropping"
            );
            continue;
        };
        out.push(ResolvedCitation {
            position,
            cited_atom_id: c.atom_id.clone(),
            excerpt: c.excerpt.clone(),
        });
        position += 1;
    }
    out
}

/// Run the agent against `source` + `ctx` and return the produced
/// content + resolved citations. Caller is responsible for persistence.
pub async fn run(
    core: &AtomicCore,
    report: &Report,
    source: &[AtomWithTags],
    total_in_scope: i32,
    ctx: &ContextFilter,
) -> Result<RunOutput, AtomicCoreError> {
    let (provider_config, model) = resolve_model(core).await?;
    let max_iters = report
        .max_tool_iterations
        .map(|n| n.max(1) as usize)
        .unwrap_or(DEFAULT_MAX_ITERATIONS);

    let tag_scope_set = build_context_tag_scope_set(core, ctx).await?;

    let mut state = AgentState::from_source(report, source);
    state.messages.push(Message::system(format!(
        "{SYSTEM_PROMPT_SCAFFOLD}\n\n---\nReport-specific instructions follow."
    )));
    state.messages.push(Message::user(build_user_prompt(
        report,
        source,
        total_in_scope,
    )));

    run_research(
        core,
        &mut state,
        ctx,
        &tag_scope_set,
        &provider_config,
        &model,
        max_iters,
    )
    .await?;

    state.messages.push(Message::user(
        "Now write the final report. Respond with a JSON object matching the \
         report_generation_result schema: set `finding_content` to markdown \
         prose with [N] citation markers, and `citations_used` to the list of \
         numbers you referenced. Do not call tools."
            .to_string(),
    ));

    let content = final_pass(&provider_config, &model, &state.messages).await?;
    let citations = extract_citations(&content, &state.citables);

    Ok(RunOutput { content, citations })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Atom, AtomKind};

    fn mock_atom(id: &str, title: &str, body: &str) -> AtomWithTags {
        AtomWithTags {
            atom: Atom {
                id: id.to_string(),
                content: body.to_string(),
                title: title.to_string(),
                snippet: body.to_string(),
                source_url: None,
                source: None,
                published_at: None,
                created_at: "2026-04-11T00:00:00Z".to_string(),
                updated_at: "2026-04-11T00:00:00Z".to_string(),
                embedding_status: "complete".to_string(),
                tagging_status: "complete".to_string(),
                embedding_error: None,
                tagging_error: None,
                kind: AtomKind::Captured,
            },
            tags: vec![],
        }
    }

    fn mock_report(policy: CitationPolicy) -> Report {
        Report {
            id: "r1".into(),
            name: "test".into(),
            description: None,
            research_prompt: "investigate".into(),
            source_scope_tag_ids: vec![],
            source_scope_window: None,
            source_include_kinds: vec![AtomKind::Captured],
            context_scope_mode: crate::models::ContextScopeMode::All,
            context_scope_tag_ids: vec![],
            context_scope_window: None,
            context_include_kinds: vec![AtomKind::Captured],
            citation_policy: policy,
            max_source_atoms: None,
            max_source_tokens: None,
            max_tool_iterations: None,
            schedule: "0 0 * * * *".into(),
            schedule_tz: None,
            enabled: true,
            output_atom_tags: vec![],
            last_run_at: None,
            last_finding_atom_id: None,
            last_error: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn source_atoms_numbered_one_indexed() {
        let report = mock_report(CitationPolicy::SourceOnly);
        let source = vec![
            mock_atom("a1", "one", "first"),
            mock_atom("a2", "two", "second"),
        ];
        let state = AgentState::from_source(&report, &source);
        assert_eq!(state.citables.get("a1").unwrap().number, 1);
        assert_eq!(state.citables.get("a2").unwrap().number, 2);
        assert_eq!(state.next_citation_number, 3);
    }

    #[test]
    fn source_only_refuses_to_cite_context_results() {
        let report = mock_report(CitationPolicy::SourceOnly);
        let source = vec![mock_atom("a1", "one", "first")];
        let mut state = AgentState::from_source(&report, &source);
        let new = mock_atom("a-new", "context", "body");
        assert!(state.citation_for_search_result(&new).is_none());
        assert!(!state.citables.contains_key("a-new"));
    }

    #[test]
    fn source_and_context_assigns_next_number_on_first_appearance() {
        let report = mock_report(CitationPolicy::SourceAndContext);
        let source = vec![mock_atom("a1", "one", "first")];
        let mut state = AgentState::from_source(&report, &source);
        let new = mock_atom("a-new", "context", "body");
        assert_eq!(state.citation_for_search_result(&new), Some(2));
        // Reuses the same number on second appearance.
        assert_eq!(state.citation_for_search_result(&new), Some(2));
        // Third atom gets 3.
        let other = mock_atom("a-other", "other", "body");
        assert_eq!(state.citation_for_search_result(&other), Some(3));
    }

    #[test]
    fn extract_citations_maps_marker_to_known_atom() {
        let report = mock_report(CitationPolicy::SourceOnly);
        let source = vec![
            mock_atom("a1", "one", "first"),
            mock_atom("a2", "two", "second"),
        ];
        let state = AgentState::from_source(&report, &source);
        let content = "Intro [1]. Detail [2]. More [1].";
        let cites = extract_citations(content, &state.citables);
        assert_eq!(cites.len(), 3);
        assert_eq!(cites[0].position, 1);
        assert_eq!(cites[0].cited_atom_id, "a1");
        assert_eq!(cites[1].position, 2);
        assert_eq!(cites[1].cited_atom_id, "a2");
        assert_eq!(cites[2].position, 3);
        assert_eq!(cites[2].cited_atom_id, "a1");
    }

    #[test]
    fn extract_citations_drops_unknown_markers() {
        let report = mock_report(CitationPolicy::SourceOnly);
        let source = vec![mock_atom("a1", "one", "first")];
        let state = AgentState::from_source(&report, &source);
        let content = "Valid [1]. Hallucinated [9].";
        let cites = extract_citations(content, &state.citables);
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].cited_atom_id, "a1");
    }

    #[test]
    fn passes_context_filter_excludes_listed_ids() {
        let atom = mock_atom("ex", "title", "body");
        let ctx = ContextFilter {
            tag_ids: vec![],
            time_window: None,
            kinds: crate::models::KindFilter::All,
            excluded_atom_ids: vec!["ex".to_string()],
        };
        assert!(!passes_context_filter(&atom, &ctx));
    }

    #[test]
    fn passes_context_filter_before_window() {
        let atom = mock_atom("a", "t", "b");
        // atom.created_at = "2026-04-11T00:00:00Z"
        let ctx = ContextFilter {
            tag_ids: vec![],
            time_window: Some(TimeWindow::Before("2026-04-10T00:00:00Z".into())),
            kinds: crate::models::KindFilter::All,
            excluded_atom_ids: vec![],
        };
        assert!(!passes_context_filter(&atom, &ctx));
        let ctx_after = ContextFilter {
            time_window: Some(TimeWindow::Before("2026-04-12T00:00:00Z".into())),
            ..ctx
        };
        assert!(passes_context_filter(&atom, &ctx_after));
    }
}
