//! Report runs: what makes a scheduled research run a *report*.
//!
//! The loop itself, the tool registry, and the citation ledger live in
//! [`crate::agent_runtime`]. This module owns the report-shaped parts: the
//! three tools a run gets (`read_atom`, `semantic_search`, `done`), the
//! prompt the source batch is rendered into, the citation policy, and the
//! final pass that turns the gathered transcript into prose. It never
//! touches storage directly, so it can be exercised against a mock LLM.
//!
//! Citations are numbered evidence, and the two policies differ only in what
//! the run admits:
//!
//! - Source atoms are seeded `[1]..[N]` in list order, so `source_only`
//!   reports have a fixed citation surface.
//! - Under `source_and_context`, every `semantic_search` result is assigned
//!   the next available number on first appearance and surfaced to the agent
//!   alongside title/snippet. Repeat hits reuse the number.
//! - `semantic_search` results are post-filtered by the report's context
//!   scope (tag subtree, time window, kinds, self-exclusion). The agent never
//!   sees atoms outside scope.
//!
//! The final pass uses the long-form markdown contract
//! (`call_long_form_markdown`): the report body is plain markdown with a
//! `CITATIONS_USED:` trailer, never JSON — see that helper's docs for the
//! two truncation incidents that ruled JSON envelopes out.

use crate::agent_runtime::{
    AgentRun, AgentTool, CitationAdmission, CitationLedger, CitationSource, RunConfig, RunError,
    Termination, ToolContext, ToolRegistry, ToolResult,
};
use crate::error::AtomicCoreError;
use crate::models::{AtomWithTags, CitationPolicy, Report};
use crate::providers::types::{GenerationParams, Message, ToolDefinition};
use crate::providers::{ProviderConfig, ProviderType};
use crate::reports::scope::{ContextFilter, TimeWindow};
use crate::search::{SearchMode, SearchOptions};
use crate::AtomicCore;

use async_trait::async_trait;

use std::collections::HashSet;

/// Default cap on tool-calling iterations. Reports can override via the
/// `max_tool_iterations` column; the cap exists so a runaway agent
/// burning tool calls can't melt down the LLM budget unbounded, but it
/// needs enough headroom that a thorough investigation (5-10 searches
/// + read_atom paging across multiple long atoms) doesn't get cut off
/// mid-research. A normal daily briefing finishes in <10; contradiction
/// scans typically take 15-25. 20 keeps the floor above the common case
/// while bounding prompt growth before `final_pass` — every extra
/// iteration appends tool-call + result messages, and once the prompt
/// gets large enough the provider's per-completion budget shrinks,
/// which surfaces as truncated findings.
const DEFAULT_MAX_ITERATIONS: usize = 20;
/// Inline snippet length used in prompt construction and tool responses.
const SNIPPET_LEN: usize = 200;
/// Excerpt length stored in `report_finding_citations.excerpt` — slightly
/// longer than the prompt snippet because the UI may render it directly.
const EXCERPT_LEN: usize = 300;
const DEFAULT_SEARCH_LIMIT: i64 = 5;
const MAX_SEARCH_LIMIT: i64 = 10;
const DEFAULT_READ_LIMIT: i64 = 500;
const MAX_READ_LIMIT: i64 = 500;
/// The tool the agent calls to end research. Named once because it is both a
/// registry entry and the run's termination condition.
const DONE_TOOL: &str = "done";

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

/// One `[N]` marker the finished report actually wrote, resolved back to the
/// atom it points at.
///
/// `position` carries the **marker number N**, not the order of appearance:
/// the dashboard renders `[N]` in the prose and looks the citation up by that
/// same N (`citationMap.get(citation_index)`), so the storage column is the
/// lookup key. Repeated markers collapse to one row — the composite PK
/// `(finding_atom_id, cited_atom_id, position)` would reject the second
/// insert, and one row per `(finding, marker)` renders every occurrence.
#[derive(Debug, Clone)]
pub struct ResolvedCitation {
    pub position: i32,
    pub cited_atom_id: String,
    pub excerpt: String,
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

/// A tool failure whose wording predates the runtime's `Error: ` prefix.
/// Reports surface no tool status anywhere — the flag exists for the
/// transcript — but the text is part of the prompt the model reads next, so
/// it stays verbatim.
fn failed_verbatim(output: String) -> ToolResult {
    ToolResult {
        output,
        results_count: 0,
        failed: true,
    }
}

fn build_user_prompt(report: &Report, source: &[AtomWithTags], total_in_scope: i32) -> String {
    let mut out = String::new();
    out.push_str("RESEARCH PROMPT:\n");
    out.push_str(&report.research_prompt);
    out.push_str("\n\n");

    // Universal citation directive. The system scaffold also covers
    // citations, but restating it in the user message — right next to
    // the research prompt and the source list — both raises the
    // model's attention to it and frees individual prompts (templates,
    // user-authored) from having to re-state it. The system scaffold
    // carries the longer-form rules (no-invented-numbers,
    // source_and_context semantics); this is the inline reminder.
    //
    // Policy-aware. `source_only` is the strict case where only the
    // source-list numbers are citable; the directive can be tight. For
    // `source_and_context` we have to also mention search-assigned
    // numbers — otherwise this very reminder contradicts the policy
    // and tells the model to suppress citations it's explicitly
    // configured to make. The two-line form is by design: the canonical
    // policy statement still appears at the bottom of the user prompt
    // (after the source list); this is the in-prompt nudge.
    match report.citation_policy {
        CitationPolicy::SourceOnly => {
            out.push_str("Cite source atoms with [N] inline markers using the bracketed numbers from the source list below.\n\n");
        }
        CitationPolicy::SourceAndContext => {
            out.push_str("Cite with [N] inline markers — numbers come from the source list below and from semantic_search results as they are surfaced.\n\n");
        }
    }

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

// ==================== read_atom ====================

struct ReadAtom {
    core: AtomicCore,
}

#[async_trait]
impl AgentTool for ReadAtom {
    fn definition(&self) -> ToolDefinition {
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
        )
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ToolContext<'_>) -> ToolResult {
        let Some(atom_id) = args.get("atom_id").and_then(|v| v.as_str()) else {
            return ToolResult::failed("atom_id is required");
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

        let atom = match self.core.get_atom(atom_id).await {
            Ok(Some(a)) => a,
            Ok(None) => return ToolResult::failed(format!("no atom found with id {}", atom_id)),
            Err(e) => return failed_verbatim(format!("Error fetching atom {}: {}", atom_id, e)),
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
        ToolResult::ok(out, 1)
    }
}

// ==================== semantic_search ====================

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

/// Search over the report's context corpus. Scope is frozen at run start —
/// the agent picks the query, never what it is allowed to see.
struct SemanticSearch {
    core: AtomicCore,
    ctx: ContextFilter,
    tag_scope_set: Option<HashSet<String>>,
}

#[async_trait]
impl AgentTool for SemanticSearch {
    fn definition(&self) -> ToolDefinition {
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
        )
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ToolContext<'_>) -> ToolResult {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if query.is_empty() {
            return ToolResult::failed("query is required");
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
        let results = match self.core.search(options).await {
            Ok(r) => r,
            Err(e) => return failed_verbatim(format!("Search error: {}", e)),
        };

        let mut shown: Vec<(i32, AtomWithTags, f32, String)> = Vec::new();
        for r in results {
            if !passes_context_filter(&r.atom, &self.ctx) {
                continue;
            }
            if let Some(set) = &self.tag_scope_set {
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
            // A `source_only` run admits no new evidence, so a non-source
            // atom gets no number back and is surfaced as uncitable
            // background (encoded as 0 in the marker).
            let number = ctx
                .citations
                .register(
                    CitationSource::Atom,
                    &r.atom.atom.id,
                    None,
                    excerpt_for(&r.atom),
                )
                .unwrap_or(0);
            shown.push((number, r.atom.clone(), r.similarity_score, snippet));
            if shown.len() >= requested as usize {
                break;
            }
        }

        if shown.is_empty() {
            return ToolResult::ok("No results in context scope.", 0);
        }

        let count = shown.len() as i32;
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
        ToolResult::ok(out, count)
    }
}

// ==================== done ====================

/// The run's sentinel: calling it ends research and hands the transcript to
/// [`final_pass`].
struct Done;

#[async_trait]
impl AgentTool for Done {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            DONE_TOOL,
            "Signal that research is complete. Call this before writing the final report.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn execute(&self, _args: &serde_json::Value, _ctx: &ToolContext<'_>) -> ToolResult {
        ToolResult::ok("Acknowledged. Write the report in your next message.", 0)
    }
}

// ==================== Run assembly ====================

fn report_tools(
    core: &AtomicCore,
    ctx: &ContextFilter,
    tag_scope_set: Option<HashSet<String>>,
) -> ToolRegistry {
    ToolRegistry::new()
        .with(ReadAtom { core: core.clone() })
        .with(SemanticSearch {
            core: core.clone(),
            ctx: ctx.clone(),
            tag_scope_set,
        })
        .with(Done)
}

/// Number the source batch `[1]..[N]` in list order, matching the numbering
/// [`build_user_prompt`] shows the model. `source_only` admits nothing
/// further, so those numbers are the run's entire citation surface;
/// `source_and_context` lets `semantic_search` mint more.
fn seeded_ledger(report: &Report, source: &[AtomWithTags]) -> CitationLedger {
    let ledger = CitationLedger::new(match report.citation_policy {
        CitationPolicy::SourceOnly => CitationAdmission::SeededOnly,
        CitationPolicy::SourceAndContext => CitationAdmission::Open,
    });
    for atom in source {
        ledger.seed(CitationSource::Atom, &atom.atom.id, None, excerpt_for(atom));
    }
    ledger
}

async fn resolve_model(core: &AtomicCore) -> Result<(ProviderConfig, String), AtomicCoreError> {
    let settings = core.settings_for_ai().await?;
    let config = ProviderConfig::from_settings(&settings);
    let model = match config.provider_type {
        ProviderType::Ollama => config.llm_model().to_string(),
        ProviderType::OpenAICompat => config.llm_model().to_string(),
        ProviderType::OrcaRouter => config.llm_model().to_string(),
        ProviderType::OpenRouter => settings
            .get("wiki_model")
            .cloned()
            .unwrap_or_else(|| crate::providers::DEFAULT_AGENTIC_MODEL.to_string()),
    };
    Ok((config, model))
}

async fn final_pass(
    provider_config: &ProviderConfig,
    model: &str,
    messages: &[Message],
) -> Result<String, AtomicCoreError> {
    // Markdown + trailer, not JSON: report bodies are long-form prose,
    // the shape JSON transports fail on (see
    // providers::structured::call_long_form_markdown). The trailer doubles
    // as a structural completeness check.
    match crate::providers::structured::call_long_form_markdown(
        provider_config,
        model,
        messages,
        "report_finding",
        None,
    )
    .await
    {
        Ok(out) => Ok(out.content),
        Err(e) => Err(AtomicCoreError::DatabaseOperation(format!(
            "final report pass failed: {}",
            e.to_compact_string()
        ))),
    }
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
    let max_iterations = report
        .max_tool_iterations
        .map(|n| n.max(1) as usize)
        .unwrap_or(DEFAULT_MAX_ITERATIONS);

    let tag_scope_set = build_context_tag_scope_set(core, ctx).await?;
    let tools = report_tools(core, ctx, tag_scope_set);
    let ledger = seeded_ledger(report, source);

    let outcome = AgentRun {
        config: RunConfig {
            model: model.clone(),
            // Explicit output budget (see
            // providers::structured::DEFAULT_MAX_OUTPUT_TOKENS for why it
            // must never be left unset on long-output calls).
            params: GenerationParams::new()
                .with_max_tokens(crate::providers::structured::DEFAULT_MAX_OUTPUT_TOKENS),
            max_iterations,
            termination: Termination::Sentinel(DONE_TOOL.to_string()),
            streaming: false,
            // The report is written by `final_pass` over the whole
            // transcript, so an exhausted budget needs no salvage answer.
            salvage_on_cap: false,
            // Reports send their history as-is; the iteration cap is the
            // only bound on prompt growth.
            context_length: None,
        },
        provider_config: &provider_config,
        tools: &tools,
        citations: &ledger,
        messages: vec![
            Message::system(format!(
                "{SYSTEM_PROMPT_SCAFFOLD}\n\n---\nReport-specific instructions follow."
            )),
            Message::user(build_user_prompt(report, source, total_in_scope)),
        ],
        cancel: None,
        events: None,
    }
    .execute()
    .await
    .map_err(|error| match error {
        RunError::Setup(error) => AtomicCoreError::DatabaseOperation(error),
        RunError::Provider(error) => {
            AtomicCoreError::DatabaseOperation(format!("report research LLM call failed: {error}"))
        }
    })?;

    let mut messages = outcome.messages;
    messages.push(Message::user(
        "Now write the final report as markdown prose with [N] citation \
         markers. Do not call tools."
            .to_string(),
    ));

    let content = final_pass(&provider_config, &model, &messages).await?;
    let citations = ledger
        .cited_in(&content)
        .into_iter()
        .map(|citable| ResolvedCitation {
            position: citable.number,
            cited_atom_id: citable.source_id,
            excerpt: citable.excerpt,
        })
        .collect();

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
    fn source_only_seeds_the_batch_and_refuses_everything_else() {
        let source = vec![
            mock_atom("a1", "one", "first"),
            mock_atom("a2", "two", "second"),
        ];
        let ledger = seeded_ledger(&mock_report(CitationPolicy::SourceOnly), &source);
        let seeded = ledger.snapshot();
        assert_eq!(seeded.len(), 2);
        assert_eq!((seeded[0].number, seeded[0].source_id.as_str()), (1, "a1"));
        assert_eq!((seeded[1].number, seeded[1].source_id.as_str()), (2, "a2"));
        assert_eq!(
            ledger.register(CitationSource::Atom, "a-new", None, "context"),
            None,
            "search results are background only under source_only"
        );
    }

    #[test]
    fn source_and_context_numbers_new_evidence_after_the_batch() {
        let source = vec![mock_atom("a1", "one", "first")];
        let ledger = seeded_ledger(&mock_report(CitationPolicy::SourceAndContext), &source);
        assert_eq!(
            ledger.register(CitationSource::Atom, "a-new", None, "context"),
            Some(2)
        );
        assert_eq!(
            ledger.register(CitationSource::Atom, "a-new", None, "context"),
            Some(2),
            "a repeat hit reuses its number"
        );
        assert_eq!(
            ledger.register(CitationSource::Atom, "a-other", None, "context"),
            Some(3)
        );
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
