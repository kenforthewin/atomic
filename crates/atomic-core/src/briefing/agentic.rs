//! Agentic daily-briefing generation.
//!
//! Mirrors `wiki::agentic`: an LLM is given a small tool set (read_atom,
//! semantic_search, done), runs a short research loop, then writes the
//! final briefing in a tool-free pass. The `[N]` citation markers in the
//! final text are extracted into [`super::BriefingCitation`] rows indexed
//! against the *initial* new-atoms list — references the agent pulled in
//! via `semantic_search` are for context only and cannot be cited.

use crate::models::AtomWithTags;
use crate::providers::structured::{call_structured, StructuredCall};
use crate::providers::types::{CompletionResponse, Message, MessageRole, ToolDefinition};
use crate::providers::{get_llm_provider, LlmConfig, ProviderConfig, ProviderType};
use crate::search::{SearchMode, SearchOptions};
use crate::AtomicCore;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;

/// Hard cap on tool-calling iterations. Smaller than wiki's 15 because a
/// daily briefing doesn't need deep research — most runs should terminate
/// in a handful of iterations.
const MAX_RESEARCH_ITERATIONS: usize = 10;
const SNIPPET_LEN: usize = 200;
const EXCERPT_LEN: usize = 300;
const DEFAULT_SEARCH_LIMIT: i64 = 5;
const MAX_SEARCH_LIMIT: i64 = 10;

/// Line pagination defaults for `read_atom`. Matches the MCP server's
/// `read_atom` tool (crates/atomic-server/src/mcp/server.rs) so the agent sees
/// the same bounded-window pattern across both entry points.
const DEFAULT_READ_LIMIT: i64 = 500;
const MAX_READ_LIMIT: i64 = 500;

/// Structured-output envelope for the final briefing pass. Mirrors
/// `WikiGenerationResult` in the wiki module so both synthesis paths use the
/// same "prose + citation list" shape.
#[derive(Debug, Deserialize)]
struct BriefingGenerationResult {
    briefing_content: String,
    #[allow(dead_code)]
    #[serde(default)]
    citations_used: Vec<i32>,
}

pub(crate) fn briefing_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "briefing_content": {
                "type": "string",
                "description": "The 2-3 paragraph briefing in markdown, with [N] citation markers referring to the initial new-atoms list."
            },
            "citations_used": {
                "type": "array",
                "items": { "type": "integer" },
                "description": "List of citation numbers actually used in briefing_content."
            }
        },
        "required": ["briefing_content", "citations_used"],
        "additionalProperties": false
    })
}

const SYSTEM_PROMPT: &str = r#"You are writing a short daily briefing of newly captured notes for a personal knowledge base.

The user will provide a numbered list of atoms (notes) added since the last briefing. Your job is to synthesize these new atoms into a 2-3 paragraph briefing that highlights what's noteworthy, what themes emerge, and where these new notes connect to existing knowledge.

You have two tools:

- **read_atom(atom_id, limit?, offset?)**: Read a window of lines from an atom's markdown content. Use this when the title and snippet aren't enough to summarize. Returns up to `limit` lines starting at `offset` (default: first 500 lines). For long atoms, page through with offset — do not try to read everything.
- **semantic_search(query, limit)**: Search the full knowledge base (new + existing atoms) to find related material. Use this sparingly — typically only when a new atom references something that sounds like it should connect to older notes.
- **done()**: Signal that you have enough material and will now write the briefing.

Guidelines:
- Keep the briefing to 2-3 short paragraphs. Do not write a long digest.
- Use [N] inline citation markers to cite specific new atoms. The N should correspond to the numbered position of the atom in the initial list (1-indexed). Every citation must map to an atom from that list.
- You may only cite atoms from the initial new-atoms list, not atoms returned by semantic_search. Search is for context only.
- After you have gathered enough context, call done() and write the final briefing in your next message.
- Skip atoms that aren't noteworthy. You are not required to cite every atom.
- Write in the user's voice: concise, direct, mildly analytical, no filler.

Do NOT write the briefing until you have called done() at least once."#;

// ==================== Tool Definitions ====================

fn briefing_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "read_atom",
            "Read a window of lines from an atom's markdown content. Use when the title and snippet aren't enough. For large atoms, page through with offset.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "atom_id": {
                        "type": "string",
                        "description": "UUID of the atom to read"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of lines to return (default 500, max 500)",
                        "default": 500
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line offset for pagination, 0-indexed (default 0)",
                        "default": 0
                    }
                },
                "required": ["atom_id"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::new(
            "semantic_search",
            "Search the full knowledge base for related material. Returns atom titles and snippets for context only — you cannot cite these atoms.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language query"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results (default 5, max 10)",
                        "default": 5
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        ToolDefinition::new(
            "done",
            "Signal that research is complete. Call this before writing the final briefing.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
    ]
}

// ==================== Prompt construction ====================

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
    // Prefer the stored snippet if present; fall back to truncated content.
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

fn build_user_prompt(since: &DateTime<Utc>, new_atoms: &[AtomWithTags], total_new: i32) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "The following {} atoms were added since {}. Summarize them in a 2-3 paragraph briefing.\n\n",
        new_atoms.len(),
        since.to_rfc3339()
    ));
    if (total_new as usize) > new_atoms.len() {
        out.push_str(&format!(
            "NOTE: {} atoms were added in this period. You are only seeing the {} most recent.\n\n",
            total_new,
            new_atoms.len()
        ));
    }
    out.push_str("NEW ATOMS:\n");
    for (i, atom) in new_atoms.iter().enumerate() {
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
    out
}

// ==================== Tool handlers ====================

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
    let returned = end - start;
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
            "\n\n(Atom content continues. Call read_atom again with offset={} to read more.)",
            end
        ));
    }
    let _ = returned; // surfaced via the "lines X-Y of Z" header
    out
}

async fn handle_semantic_search(core: &AtomicCore, args: &serde_json::Value) -> String {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if query.is_empty() {
        return "Error: query is required".to_string();
    }
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT) as i32;

    let options = SearchOptions::new(query.clone(), SearchMode::Semantic, limit);
    let results = match core.search(options).await {
        Ok(r) => r,
        Err(e) => return format!("Search error: {}", e),
    };

    if results.is_empty() {
        return "No results.".to_string();
    }

    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        let title = if r.atom.atom.title.is_empty() {
            "(untitled)"
        } else {
            r.atom.atom.title.as_str()
        };
        let snippet = truncate_on_char_boundary(
            &r.matching_chunk_content
                .chars()
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect::<String>(),
            SNIPPET_LEN,
        );
        out.push_str(&format!(
            "{}. {}\n   (atom id: {}, score: {:.2})\n   {}\n\n",
            i + 1,
            title,
            r.atom.atom.id,
            r.similarity_score,
            snippet,
        ));
    }
    out
}

// ==================== Research loop ====================

struct AgentState {
    messages: Vec<Message>,
    done_called: bool,
}

async fn resolve_model(core: &AtomicCore) -> Result<(ProviderConfig, String, Option<String>), String> {
    let settings = core
        .get_settings()
        .await
        .map_err(|e| format!("Failed to load settings: {}", e))?;
    let config = ProviderConfig::from_settings(&settings);
    // Mirror build_wiki_strategy_context: local providers use their own llm_model,
    // OpenRouter uses the wiki_model setting (the briefing shares it — intentional).
    let model = match config.provider_type {
        ProviderType::Ollama => config.llm_model().to_string(),
        ProviderType::OpenAICompat => config.llm_model().to_string(),
        ProviderType::OpenRouter => settings
            .get("wiki_model")
            .cloned()
            .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string()),
    };
    let custom_prompt = settings
        .get("briefing_prompt")
        .filter(|s| !s.is_empty())
        .cloned();
    Ok((config, model, custom_prompt))
}

async fn run_research(
    core: &AtomicCore,
    state: &mut AgentState,
    provider_config: &ProviderConfig,
    model: &str,
) -> Result<(), String> {
    let tools = briefing_tools();
    let llm_config = LlmConfig::new(model);
    let provider = get_llm_provider(provider_config).map_err(|e| e.to_string())?;

    for iteration in 0..MAX_RESEARCH_ITERATIONS {
        tracing::debug!(
            iteration = iteration + 1,
            max = MAX_RESEARCH_ITERATIONS,
            "[briefing/agentic] Research iteration"
        );

        let response: CompletionResponse = provider
            .complete_with_tools(&state.messages, &tools, &llm_config)
            .await
            .map_err(|e| format!("Briefing research LLM call failed: {}", e))?;

        let tool_calls = match response.tool_calls {
            Some(ref tcs) if !tcs.is_empty() => tcs.clone(),
            _ => {
                if !response.content.is_empty() {
                    tracing::debug!(
                        "[briefing/agentic] Agent produced text without tools, ending research"
                    );
                }
                break;
            }
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
                "semantic_search" => handle_semantic_search(core, &args).await,
                "done" => {
                    done_this_round = true;
                    state.done_called = true;
                    "Acknowledged. Write the briefing in your next message.".to_string()
                }
                _ => format!("Unknown tool: {}", name),
            };

            state
                .messages
                .push(Message::tool_result(tc.id.clone(), result));
        }

        if done_this_round {
            tracing::debug!("[briefing/agentic] done() called, exiting research loop");
            break;
        }
    }

    Ok(())
}

/// Final pass: hand the accumulated research conversation to the shared
/// `call_structured` helper. Everything about retries, tolerant parsing, and
/// the prompt-based fallback lives there — this function is now just glue.
async fn final_briefing_call(
    provider_config: &ProviderConfig,
    model: &str,
    messages: &[Message],
) -> Result<String, String> {
    let call = StructuredCall::<BriefingGenerationResult>::new(
        provider_config,
        model,
        messages,
        "briefing_generation_result",
        briefing_schema(),
    );

    match call_structured::<BriefingGenerationResult>(call).await {
        Ok(result) => Ok(result.briefing_content),
        Err(e) => {
            tracing::error!(error = %e, "[briefing] Final structured pass failed");
            Err(e.to_compact_string())
        }
    }
}

// ==================== Citation extraction ====================

/// Extract `[N]` markers from the briefing and map each to an atom in the
/// initial new-atoms list (1-indexed). Citations that don't map are dropped
/// with a warning — this is the spec'd behavior for "agent cites something
/// it doesn't have."
fn extract_citations(content: &str, new_atoms: &[AtomWithTags]) -> Vec<(i32, String, String)> {
    let re = match Regex::new(r"\[(\d+)\]") {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "[briefing] Failed to compile citation regex");
            return vec![];
        }
    };

    let mut out: Vec<(i32, String, String)> = Vec::new();
    let mut seen: HashSet<i32> = HashSet::new();

    for cap in re.captures_iter(content) {
        let Some(m) = cap.get(1) else { continue };
        let Ok(idx) = m.as_str().parse::<i32>() else {
            continue;
        };
        if !seen.insert(idx) {
            continue;
        }
        let pos = (idx - 1) as usize;
        let Some(atom) = new_atoms.get(pos) else {
            tracing::warn!(
                citation_index = idx,
                atom_count = new_atoms.len(),
                "[briefing] Agent produced citation out of range; dropping"
            );
            continue;
        };
        let source = if !atom.atom.snippet.is_empty() {
            atom.atom.snippet.as_str()
        } else {
            atom.atom.content.as_str()
        };
        let excerpt = truncate_on_char_boundary(source.trim(), EXCERPT_LEN);
        out.push((idx, atom.atom.id.clone(), excerpt));
    }

    out.sort_by_key(|(idx, _, _)| *idx);
    out
}

// ==================== Public entry ====================

/// Run the briefing agent and return `(content, citations)`. Citations are
/// `(index, atom_id, excerpt)` tuples ready to be persisted.
pub(crate) async fn generate(
    core: &AtomicCore,
    since: &DateTime<Utc>,
    new_atoms: &[AtomWithTags],
    total_new: i32,
) -> Result<(String, Vec<(i32, String, String)>), String> {
    let (provider_config, model, custom_system_prompt) = resolve_model(core).await?;
    tracing::info!(model = %model, atoms = new_atoms.len(), "[briefing/agentic] Running agent");

    let user_prompt = build_user_prompt(since, new_atoms, total_new);

    let system = custom_system_prompt.as_deref().unwrap_or(SYSTEM_PROMPT);
    let mut state = AgentState {
        messages: vec![
            Message::system(system.to_string()),
            Message::user(user_prompt),
        ],
        done_called: false,
    };

    run_research(core, &mut state, &provider_config, &model).await?;

    if !state.done_called {
        tracing::debug!(
            "[briefing/agentic] Agent ended research without calling done(); proceeding to final pass anyway"
        );
    }

    // Nudge the model to produce the final briefing. The final call passes a
    // structured-output schema so the response is a JSON object with a
    // `briefing_content` field — the model needs to know this, otherwise it
    // will write raw markdown and the parse will fail.
    state.messages.push(Message::user(
        "Now write the final briefing. Respond with a JSON object matching the \
         briefing_generation_result schema: set `briefing_content` to 2-3 short \
         paragraphs of markdown with [N] citation markers, and set `citations_used` \
         to the list of citation numbers you referenced. Do not call any tools."
            .to_string(),
    ));

    let content = final_briefing_call(&provider_config, &model, &state.messages).await?;

    let citations = extract_citations(&content, new_atoms);
    tracing::info!(
        citations = citations.len(),
        "[briefing/agentic] Briefing generated"
    );

    Ok((content, citations))
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::structured::lint_schema;

    #[test]
    fn lint_briefing_schema_is_portable() {
        lint_schema(&briefing_schema()).expect("briefing_schema must be portable across providers");
    }

    #[test]
    fn truncate_on_char_boundary_ascii() {
        assert_eq!(truncate_on_char_boundary("hello", 3), "hel...");
    }

    #[test]
    fn truncate_on_char_boundary_under_limit() {
        assert_eq!(truncate_on_char_boundary("hi", 10), "hi");
    }

    #[test]
    fn truncate_on_char_boundary_multibyte() {
        // "héllo" is 6 bytes (é is 2 bytes). Truncating at byte offset 3
        // must not split the é codepoint — we should land at a safe boundary.
        let out = truncate_on_char_boundary("héllo", 3);
        assert!(out.ends_with("..."));
        // The non-ellipsis prefix must be valid UTF-8 (implicit — it's a String),
        // and it should contain either "h" or "hé" but never a broken é.
        let without_ellipsis = out.trim_end_matches("...");
        assert!(!without_ellipsis.is_empty());
    }

    #[test]
    fn extract_citations_maps_to_initial_list() {
        let atoms = vec![
            mock_atom("a-1", "first", "body 1"),
            mock_atom("a-2", "second", "body 2"),
            mock_atom("a-3", "third", "body 3"),
        ];
        let content = "The first atom introduces X [1]. The third expands on it [3].";
        let citations = extract_citations(content, &atoms);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].0, 1);
        assert_eq!(citations[0].1, "a-1");
        assert_eq!(citations[1].0, 3);
        assert_eq!(citations[1].1, "a-3");
    }

    #[test]
    fn extract_citations_dedupes_repeats() {
        let atoms = vec![mock_atom("a-1", "only", "body")];
        // Same atom cited twice should only appear once in the output.
        let content = "See [1] and also [1].";
        let citations = extract_citations(content, &atoms);
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].0, 1);
    }

    #[test]
    fn extract_citations_drops_out_of_range() {
        // Agent hallucinated [5] when only 2 atoms exist — must be dropped
        // without panicking.
        let atoms = vec![
            mock_atom("a-1", "first", "body 1"),
            mock_atom("a-2", "second", "body 2"),
        ];
        let content = "Valid [1] and [2], bogus [5] and [99].";
        let citations = extract_citations(content, &atoms);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].0, 1);
        assert_eq!(citations[1].0, 2);
    }

    #[test]
    fn extract_citations_handles_no_citations() {
        let atoms = vec![mock_atom("a-1", "only", "body")];
        let content = "A plain paragraph with no markers at all.";
        let citations = extract_citations(content, &atoms);
        assert!(citations.is_empty());
    }

    fn mock_atom(id: &str, title: &str, snippet: &str) -> AtomWithTags {
        use crate::models::Atom;
        AtomWithTags {
            atom: Atom {
                id: id.to_string(),
                content: snippet.to_string(),
                title: title.to_string(),
                snippet: snippet.to_string(),
                source_url: None,
                source: None,
                published_at: None,
                created_at: "2026-04-11T00:00:00Z".to_string(),
                updated_at: "2026-04-11T00:00:00Z".to_string(),
                embedding_status: "complete".to_string(),
                tagging_status: "complete".to_string(),
                embedding_error: None,
                tagging_error: None,
                is_locked: false,
            },
            tags: vec![],
        }
    }
}
