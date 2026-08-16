//! Chat: the conversational surface over the knowledge base.
//!
//! This module owns what makes a chat turn a *chat* turn — the events a
//! caller sees, the UI context a turn can carry, the system prompt, and the
//! persistence tail. The loop itself, the tool registry, and citations live
//! in [`crate::agent_runtime`]; chat's tools live in [`crate::chat_tools`].
//! Events reach consumers (Tauri, HTTP server) through a callback, the same
//! pattern `EmbeddingEvent` uses.

use crate::agent_runtime::{
    truncate_messages_to_context, AgentRun, CitationAdmission, CitationLedger, CitationSource,
    RunConfig, RunError, RunEvent, RunEventSink, StopReason, Termination, ToolCallRecord,
};
use crate::chat_tools::ChatToolset;
use crate::embedding::EmbeddingEvent;
use crate::models::{
    AtomWithTags, ChatCitation, ChatMessage, ChatMessageWithContext, ChatToolCall,
};
use crate::providers::types::{GenerationParams, Message, MessageRole};
use crate::providers::{ProviderConfig, ProviderType};
use crate::storage::StorageBackend;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

pub(crate) type ChatEventCallback = Arc<dyn Fn(ChatEvent) + Send + Sync + 'static>;

/// Cooperative cancellation handle for one in-flight chat turn. Raise the
/// flag and the agent loop stops at its next checkpoint — between iterations,
/// between sequential tool executions, and mid-stream — then persists and
/// returns whatever text it had already produced. Hosts own the registry
/// (see `atomic-server`'s cancellation registry); the loop only reads it.
pub type ChatCancel = crate::agent_runtime::CancelFlag;

// ==================== Chat Events ====================

/// Events emitted during the chat agent loop.
/// Consumers (Tauri, HTTP server) bridge these to their own event systems.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    /// One incremental chunk of assistant text, emitted as the provider
    /// streams it. Consumers append; the authoritative full text arrives with
    /// [`ChatEvent::Complete`].
    StreamDelta {
        conversation_id: String,
        content: String,
    },
    /// Tool execution started
    ToolStart {
        conversation_id: String,
        tool_call_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    /// Tool execution completed. `failed` mirrors the persisted
    /// `chat_tool_calls.status`, so a tool that errored reads as failed while
    /// the turn is still streaming — not only after the final message lands.
    ToolComplete {
        conversation_id: String,
        tool_call_id: String,
        results_count: i32,
        failed: bool,
    },
    /// Full message completed
    Complete {
        conversation_id: String,
        message: ChatMessageWithContext,
    },
    /// Canvas action requested by the agent (executed on frontend)
    CanvasAction {
        conversation_id: String,
        action: String,
        params: serde_json::Value,
    },
    /// Atom created by a chat tool
    AtomCreated {
        conversation_id: String,
        atom: AtomWithTags,
    },
    /// Atom updated by a chat tool
    AtomUpdated {
        conversation_id: String,
        atom: AtomWithTags,
    },
    /// Embedding/tagging pipeline event for an atom mutated by a chat tool
    AtomPipelineEvent {
        conversation_id: String,
        event: EmbeddingEvent,
    },
    /// Conversation metadata changed outside a request/response cycle —
    /// today, the auto-generated title landing after the turn completed.
    ConversationUpdated {
        conversation_id: String,
        title: String,
    },
    /// Error during chat
    Error {
        conversation_id: String,
        error: String,
    },
}

// ==================== UI Context ====================

/// Context about the user's current app view, passed from the frontend with a
/// chat turn. This stays compact so the agent can explicitly retrieve full
/// content through tools instead of receiving hidden prompt content.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PageContext {
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub atom_id: Option<String>,
    #[serde(default)]
    pub atom_title: Option<String>,
    #[serde(default)]
    pub atom_snippet: Option<String>,
    #[serde(default)]
    pub wiki_tag_id: Option<String>,
    #[serde(default)]
    pub wiki_tag_name: Option<String>,
    #[serde(default)]
    pub selected_tag_id: Option<String>,
}

fn get_page_context_system_prompt() -> &'static str {
    r#"

You can inspect the user's current Atomic UI context with get_current_page_context.
Use it before answering when the user refers to "this", "current", "open", "visible", or the note/page they are reading. If it returns an atom_id and you need more than the snippet, call get_atom with that atom_id before answering."#
}

// ==================== Canvas Context ====================

/// Context about the canvas state, passed from the frontend when chatting from the canvas.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CanvasContext {
    pub clusters: Vec<CanvasClusterSummary>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CanvasClusterSummary {
    pub label: String,
    pub atom_count: i32,
}

const MAX_CANVAS_CLUSTERS: usize = 30;

fn get_canvas_system_prompt(ctx: &CanvasContext) -> String {
    // Take the largest clusters by atom count
    let mut sorted: Vec<&CanvasClusterSummary> = ctx.clusters.iter().collect();
    sorted.sort_by(|a, b| b.atom_count.cmp(&a.atom_count));
    let cluster_list: Vec<String> = sorted
        .iter()
        .take(MAX_CANVAS_CLUSTERS)
        .map(|c| format!("- \"{}\" ({} atoms)", c.label, c.atom_count))
        .collect();
    format!(
        r#"

You are also viewing the user's knowledge graph canvas. The following topic clusters are visible:
{}

You have canvas interaction tools available:
- zoom_to_cluster: Animate the camera to center on a cluster. Use when discussing a topic area.
- focus_atom: Zoom to a specific atom and show its preview. Use after searching to highlight results on the canvas.

The canvas shows a single view at a time — each navigation call replaces the previous one. If the user asks about multiple clusters or atoms, pick the most relevant one or navigate sequentially, not simultaneously.

Use these tools proactively when they would help the user navigate their knowledge visually."#,
        cluster_list.join("\n")
    )
}

// ==================== System Prompt ====================

fn get_system_prompt(scope_description: &str) -> String {
    format!(
        r#"You are a helpful AI assistant with access to the user's personal knowledge base. Your role is to answer questions by searching through and referencing the user's stored information.

{}

Guidelines:
- Use search_atoms to find relevant information before answering, unless another available tool more directly addresses the user's request
- The knowledge base has structure beyond individual atoms: list_tags shows how it is organized, get_wiki reads the synthesized article for a tag, and list_reports/get_finding surface what scheduled research has already turned up. Reach for these when the question is about a topic as a whole rather than a specific note
- Only call create_atom or edit_atom when the user explicitly asks you to create or modify an atom
- Prefer targeted edit_atom operations. Use replace_all only for intentional full-content replacement
- When you create a new atom, include [[atom_id]] in the final response so the user can open it
- If the initial search doesn't find enough, try different search queries
- When you use information from a tool result, cite it with the [N] number that result gave you
- Be honest if you cannot find information - do not make things up
- Keep responses concise but informative
- If the user asks about something not in their knowledge base, say so

When citing sources:
- Use the exact [N] numbers the tools gave you. Never renumber them, and never cite a number no tool showed you
- Place citations immediately after the relevant claim
- You can cite the same source multiple times if needed
- The user only sees the sources you cite, so cite everything your answer relies on"#,
        scope_description
    )
}

// ==================== Turn Assembly ====================

/// Convert ChatMessage models from storage into provider Message format for the API.
fn chat_messages_to_provider_messages(
    messages: Vec<crate::models::ChatMessageWithContext>,
) -> Vec<Message> {
    messages
        .into_iter()
        .map(|m| {
            let role = match m.message.role.as_str() {
                "system" => MessageRole::System,
                "assistant" => MessageRole::Assistant,
                "tool" => MessageRole::Tool,
                _ => MessageRole::User,
            };
            Message {
                role,
                content: Some(m.message.content),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }
        })
        .collect()
}

/// Tool-calling rounds a single turn may take before the loop gives up and
/// salvages whatever answer it has.
const MAX_ITERATIONS: usize = 10;

/// Appended to the assistant message when the user stopped the turn.
const STOPPED_MARKER: &str = "*(stopped)*";

/// Appended to the assistant message when the tool-call budget ran out.
const ITERATION_CAP_NOTE: &str = "*(reached tool-call limit — answer may be incomplete)*";

/// Attach a trailing note to a partial answer, keeping the note readable when
/// there is no text to attach it to.
fn with_note(text: &str, note: &str) -> String {
    let text = text.trim_end();
    if text.is_empty() {
        note.to_string()
    } else {
        format!("{}\n\n{}", text, note)
    }
}

/// UI context a turn can carry. Absent for headless callers, which is also
/// what decides whether the page and canvas tools are registered at all.
#[derive(Default)]
struct TurnContext {
    page: Option<PageContext>,
    canvas: Option<CanvasContext>,
    canvas_cache: Option<crate::CanvasCache>,
}

/// Provider config and model for a chat turn. OpenRouter has a dedicated
/// chat-model setting; local providers use their single configured LLM.
fn resolve_chat_model(
    settings: &HashMap<String, String>,
) -> Result<(ProviderConfig, String), String> {
    let provider_config = ProviderConfig::from_settings(settings);

    if provider_config.provider_type == ProviderType::OpenRouter
        && provider_config.openrouter_api_key.is_none()
    {
        return Err("OpenRouter API key not configured. Please set it in Settings.".to_string());
    }

    let model = match provider_config.provider_type {
        ProviderType::Ollama => provider_config.llm_model().to_string(),
        ProviderType::OpenAICompat => provider_config.llm_model().to_string(),
        ProviderType::OrcaRouter => provider_config.llm_model().to_string(),
        ProviderType::OpenRouter => settings
            .get("chat_model")
            .cloned()
            .unwrap_or_else(|| crate::providers::DEFAULT_AGENTIC_MODEL.to_string()),
    };

    Ok((provider_config, model))
}

/// Bridge runtime progress onto the conversation's event stream.
fn chat_event_sink(on_event: &ChatEventCallback, conversation_id: &str) -> RunEventSink {
    let on_event = Arc::clone(on_event);
    let conversation_id = conversation_id.to_string();
    Arc::new(move |event| {
        let conversation_id = conversation_id.clone();
        on_event(match event {
            RunEvent::Delta { text } => ChatEvent::StreamDelta {
                conversation_id,
                content: text,
            },
            RunEvent::ToolStart {
                tool_call_id,
                tool_name,
                input,
            } => ChatEvent::ToolStart {
                conversation_id,
                tool_call_id,
                tool_name,
                tool_input: input,
            },
            RunEvent::ToolComplete {
                tool_call_id,
                results_count,
                failed,
                ..
            } => ChatEvent::ToolComplete {
                conversation_id,
                tool_call_id,
                results_count,
                failed,
            },
        });
    })
}

fn to_chat_tool_calls(records: Vec<ToolCallRecord>) -> Vec<ChatToolCall> {
    records
        .into_iter()
        .map(|record| ChatToolCall {
            id: record.id,
            message_id: String::new(), // Set when saving
            tool_name: record.name,
            tool_input: record.input,
            tool_output: Some(serde_json::Value::String(record.output)),
            status: if record.failed { "failed" } else { "complete" }.to_string(),
            created_at: record.started_at,
            completed_at: Some(record.completed_at),
        })
        .collect()
}

/// Turn the run's cited evidence into the citation rows to persist. Only
/// what the answer actually cites is stored — the Sources row is what the
/// answer rests on, not a log of everything the agent read.
///
/// Wiki citations carry their tag's name so the message this turn returns
/// can open the article straight away; reloading the conversation resolves
/// the same name through the read path's join.
async fn to_chat_citations(
    storage: &StorageBackend,
    ledger: &CitationLedger,
    content: &str,
) -> Vec<ChatCitation> {
    let mut citations = Vec::new();
    for citable in ledger.cited_in(content) {
        let source_title = match citable.source {
            CitationSource::Wiki => storage
                .get_tag_sync(&citable.source_id)
                .await
                .unwrap_or_default()
                .map(|tag| tag.name),
            CitationSource::Atom | CitationSource::Finding => None,
        };
        citations.push(ChatCitation {
            id: Uuid::new_v4().to_string(),
            message_id: String::new(), // Set when saving
            citation_index: citable.number,
            atom_id: citable.source_id,
            chunk_index: citable.chunk_index,
            excerpt: citable.excerpt,
            relevance_score: None,
            source_type: citable.source.as_str().to_string(),
            source_title,
        });
    }
    citations
}

/// Run one chat turn end to end: persist the user message, assemble the
/// prompt and tools, run the agent, then persist and announce the answer.
#[allow(clippy::too_many_arguments)] // One turn; each argument is a distinct context channel.
async fn send_turn(
    storage: StorageBackend,
    conversation_id: &str,
    content: &str,
    on_event: ChatEventCallback,
    external_settings: Option<HashMap<String, String>>,
    inline_pipeline: bool,
    ui: TurnContext,
    cancel: Option<ChatCancel>,
) -> Result<ChatMessageWithContext, String> {
    // Resolve settings (from the caller's resolved map if provided,
    // otherwise the storage layer's global tier — provider config is
    // deployment-wide)
    let settings_map = match external_settings {
        Some(s) => s,
        None => storage
            .get_global_settings_sync()
            .await
            .map_err(|e| e.to_string())?,
    };
    let (provider_config, model) = resolve_chat_model(&settings_map)?;

    // Save user message
    storage
        .save_message_sync(conversation_id, "user", content)
        .await
        .map_err(|e| e.to_string())?;

    // Get conversation context
    let scope = storage
        .get_conversation_scope_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    let scope_description = storage
        .get_scope_description_sync(&scope)
        .await
        .map_err(|e| e.to_string())?;

    // Get conversation messages via get_conversation_sync and convert to provider format
    let conversation = storage
        .get_conversation_sync(conversation_id)
        .await
        .map_err(|e| e.to_string())?;
    // A conversation with no title yet is a candidate for one once this turn
    // lands; anything already titled skips the work entirely.
    let (messages, untitled) = match conversation {
        Some(conv) => (
            chat_messages_to_provider_messages(conv.messages),
            conv.conversation.title.is_none(),
        ),
        None => (Vec::new(), false),
    };

    // Build message history for API, with UI context appended to the system prompt
    let custom_chat_prefix = settings_map
        .get("chat_prompt")
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str());
    let base_system = get_system_prompt(&scope_description);
    let mut system_prompt = match custom_chat_prefix {
        Some(prefix) => format!("{prefix}\n\n{base_system}"),
        None => base_system,
    };
    if ui.page.is_some() {
        system_prompt.push_str(get_page_context_system_prompt());
    }
    if let Some(ref canvas) = ui.canvas {
        system_prompt.push_str(&get_canvas_system_prompt(canvas));
    }
    let mut api_messages = vec![Message::system(system_prompt)];
    api_messages.extend(messages);

    // Truncate to fit context window for providers with limited context
    let context_length = provider_config.context_length_for_model(&model);
    let api_messages = truncate_messages_to_context(api_messages, context_length);

    let tools = ChatToolset {
        storage: storage.clone(),
        conversation_id: conversation_id.to_string(),
        scope,
        settings: Some(settings_map.clone()),
        inline_pipeline,
        canvas_cache: ui.canvas_cache,
        page_context: ui.page,
        canvas_context: ui.canvas,
        on_event: Arc::clone(&on_event),
    }
    .into_registry();
    // Chat lets any tool make what it surfaces citable; the answer's markers
    // decide what survives.
    let ledger = CitationLedger::new(CitationAdmission::Open);

    let title_settings = untitled.then(|| settings_map.clone());
    let outcome = AgentRun {
        config: RunConfig {
            model,
            params: GenerationParams::new()
                .with_temperature(0.7)
                .with_max_tokens(4000),
            max_iterations: MAX_ITERATIONS,
            termination: Termination::NoToolCalls,
            streaming: true,
            salvage_on_cap: true,
            context_length,
        },
        provider_config: &provider_config,
        tools: &tools,
        citations: &ledger,
        messages: api_messages,
        cancel,
        events: Some(chat_event_sink(&on_event, conversation_id)),
    }
    .execute()
    .await
    .map_err(|error| match error {
        // A dead provider has to reach the UI over the event stream, not
        // only as the failed return of a request nobody may be reading.
        RunError::Provider(error) => {
            let error = format!("API request failed: {}", error);
            on_event(ChatEvent::Error {
                conversation_id: conversation_id.to_string(),
                error: error.clone(),
            });
            error
        }
        RunError::Setup(error) => format!("Failed to create streaming provider: {}", error),
    })?;

    let content = match outcome.stop {
        StopReason::Answered | StopReason::Sentinel => outcome.content,
        StopReason::Cancelled => with_note(&outcome.content, STOPPED_MARKER),
        StopReason::IterationCap => with_note(&outcome.content, ITERATION_CAP_NOTE),
    };
    let mut result = ChatMessageWithContext {
        citations: to_chat_citations(&storage, &ledger, &content).await,
        message: ChatMessage {
            id: Uuid::new_v4().to_string(),
            conversation_id: conversation_id.to_string(),
            role: "assistant".to_string(),
            content,
            created_at: Utc::now().to_rfc3339(),
            message_index: 0, // Set when saving
        },
        tool_calls: to_chat_tool_calls(outcome.tool_calls),
    };

    finish_turn(
        &storage,
        conversation_id,
        &mut result,
        &on_event,
        title_settings,
    )
    .await?;

    Ok(result)
}

/// Persist the assistant turn, tell the caller it landed, and name the
/// conversation if it still has no title.
///
/// The tail of a turn is identical whatever context went into it, and the
/// title hook has to hang off every path a first exchange can take.
/// `title_settings` is `None` when the conversation was already titled when
/// the turn started.
async fn finish_turn(
    storage: &StorageBackend,
    conversation_id: &str,
    result: &mut ChatMessageWithContext,
    on_event: &ChatEventCallback,
    title_settings: Option<HashMap<String, String>>,
) -> Result<(), String> {
    let saved_msg = storage
        .save_message_sync(conversation_id, "assistant", &result.message.content)
        .await
        .map_err(|e| e.to_string())?;

    result.message.id = saved_msg.id.clone();
    result.message.message_index = saved_msg.message_index;

    for tool_call in &mut result.tool_calls {
        tool_call.message_id = saved_msg.id.clone();
    }
    storage
        .save_tool_calls_sync(&saved_msg.id, &result.tool_calls)
        .await
        .map_err(|e| e.to_string())?;

    for citation in &mut result.citations {
        citation.message_id = saved_msg.id.clone();
    }
    storage
        .save_citations_sync(&saved_msg.id, &result.citations)
        .await
        .map_err(|e| e.to_string())?;

    on_event(ChatEvent::Complete {
        conversation_id: conversation_id.to_string(),
        message: result.clone(),
    });

    if let Some(settings) = title_settings {
        crate::chat_title::spawn_generation(
            storage.clone(),
            settings,
            conversation_id.to_string(),
            Arc::clone(on_event),
        );
    }

    Ok(())
}

// ==================== Public API ====================

/// Send a chat message and run the agent loop.
///
/// The `on_event` callback is invoked with streaming deltas, tool call events,
/// and completion/error events. This is the same pattern as `EmbeddingEvent`.
///
/// Returns the final assistant message with tool calls and citations.
pub async fn send_chat_message<F>(
    storage: StorageBackend,
    conversation_id: &str,
    content: &str,
    on_event: F,
) -> Result<ChatMessageWithContext, String>
where
    F: Fn(ChatEvent) + Send + Sync + 'static,
{
    send_chat_message_with_settings(
        storage,
        conversation_id,
        content,
        on_event,
        None,
        true,
        None,
    )
    .await
}

/// Like `send_chat_message` but with externally-provided settings (from
/// registry). `inline_pipeline` controls whether atom mutations made by the
/// agent's tools execute their embedding/tagging jobs in-process (`true`,
/// the default behavior) or leave them in the durable ledger for the host's
/// dedicated pipeline worker (see `AtomicCore::set_inline_pipeline`).
/// `cancel`, when supplied, lets the host stop the turn mid-flight; the
/// partial answer is still persisted and returned.
#[allow(clippy::too_many_arguments)] // Public chat entry; each argument is a distinct context channel.
pub async fn send_chat_message_with_settings<F>(
    storage: StorageBackend,
    conversation_id: &str,
    content: &str,
    on_event: F,
    external_settings: Option<HashMap<String, String>>,
    inline_pipeline: bool,
    cancel: Option<ChatCancel>,
) -> Result<ChatMessageWithContext, String>
where
    F: Fn(ChatEvent) + Send + Sync + 'static,
{
    send_turn(
        storage,
        conversation_id,
        content,
        Arc::new(on_event),
        external_settings,
        inline_pipeline,
        TurnContext::default(),
        cancel,
    )
    .await
}

/// Like `send_chat_message_with_settings` but with optional UI context for
/// page-aware and canvas-aware tools.
#[allow(clippy::too_many_arguments)] // Public chat entry; each argument is a distinct context channel.
pub async fn send_chat_message_with_canvas<F>(
    storage: StorageBackend,
    conversation_id: &str,
    content: &str,
    on_event: F,
    external_settings: Option<HashMap<String, String>>,
    inline_pipeline: bool,
    canvas_context: Option<CanvasContext>,
    page_context: Option<PageContext>,
    canvas_cache: Option<crate::CanvasCache>,
    cancel: Option<ChatCancel>,
) -> Result<ChatMessageWithContext, String>
where
    F: Fn(ChatEvent) + Send + Sync + 'static,
{
    send_turn(
        storage,
        conversation_id,
        content,
        Arc::new(on_event),
        external_settings,
        inline_pipeline,
        TurnContext {
            page: page_context,
            canvas: canvas_context,
            canvas_cache,
        },
        cancel,
    )
    .await
}
