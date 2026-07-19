//! Wiremock-backed mock of the OpenAI-compat `/v1/embeddings` and
//! `/v1/chat/completions` endpoints.
//!
//! The provider in `atomic-core/src/providers/openai_compat.rs` is the real
//! reqwest client — `MockAiServer::start` just stands up an HTTP listener
//! that speaks the protocol it expects. Tests configure `AtomicCore` to
//! point at `base_url()`, then exercise the full pipeline (chunk → embed →
//! tag → edges) against deterministic responses.
//!
//! ## Mock responder modes
//!
//! [`ChatResponder`] currently emits **tag extraction** results keyed off
//! the request's `response_format.json_schema.name`. Slice 3 will extend
//! this with wiki-article and chat-tool-call modes.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Embedding dimension used by the mock. Must stay in lockstep with the
/// default `openai_compat_embedding_dimension` setting and the SQLite
/// `vec_chunks float[1536]` schema so no dimension reconciliation kicks
/// in mid-test.
pub const EMBED_DIM: usize = 1536;

/// Similarity threshold used by the pipeline when building semantic edges.
/// Exposed here so tests can sanity-check that crafted atom pairs fall on
/// the correct side of the cutoff (see
/// `atomic_core::embedding::compute_semantic_edges...`).
pub const EDGE_SIMILARITY_THRESHOLD: f32 = 0.5;

/// Local HTTP server mimicking OpenAI's `/v1/embeddings` and
/// `/v1/chat/completions`. Holds the server handle for lifetime management.
pub struct MockAiServer {
    server: MockServer,
    counters: Arc<MockAiCounters>,
}

/// An injectable failure response, served instead of the normal payload
/// while set (see [`MockAiServer::set_embedding_failure`] /
/// [`MockAiServer::set_chat_failure`]). Lets tests exercise providers'
/// status-code handling — retry/backoff behavior, rate-limit hints,
/// billing rejections — against the real HTTP clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectedFailure {
    /// HTTP 429, optionally carrying a `Retry-After: <secs>` header.
    RateLimited { retry_after_secs: Option<u64> },
    /// HTTP 402 with a provider-style error body.
    PaymentRequired,
    /// HTTP 401 with a provider-style error body (expired/revoked API key).
    Unauthorized,
}

impl InjectedFailure {
    fn response(self) -> ResponseTemplate {
        match self {
            InjectedFailure::RateLimited { retry_after_secs } => {
                let mut response = ResponseTemplate::new(429).set_body_json(json!({
                    "error": { "message": "mock rate limit exceeded" }
                }));
                if let Some(secs) = retry_after_secs {
                    response = response.insert_header("Retry-After", secs.to_string().as_str());
                }
                response
            }
            InjectedFailure::PaymentRequired => ResponseTemplate::new(402).set_body_json(json!({
                "error": { "message": "mock insufficient credits" }
            })),
            InjectedFailure::Unauthorized => ResponseTemplate::new(401).set_body_json(json!({
                "error": { "message": "mock invalid api key" }
            })),
        }
    }
}

#[derive(Default)]
struct MockAiCounters {
    embedding_requests: AtomicUsize,
    chat_requests: AtomicUsize,
    /// The `model` field of every `/v1/chat/completions` request body, in
    /// arrival order — lets tests assert *which* model an operation selected,
    /// not just that a call happened.
    chat_models: Mutex<Vec<String>>,
    /// When set, `/v1/embeddings` serves this failure instead of embeddings.
    embedding_failure: Mutex<Option<InjectedFailure>>,
    /// When set, `/v1/chat/completions` serves this failure instead of a
    /// completion.
    chat_failure: Mutex<Option<InjectedFailure>>,
    /// When set, every `/v1/chat/completions` response (success or injected
    /// failure) is held for this long before being sent — latency injection
    /// for tests that need requests to genuinely overlap in flight.
    chat_delay: Mutex<Option<std::time::Duration>>,
}

impl MockAiServer {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let counters = Arc::new(MockAiCounters::default());

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(EmbedResponder {
                counters: counters.clone(),
            })
            .mount(&server)
            .await;

        // Tag extraction goes through the non-streaming `complete` path
        // with a `response_format: json_schema` payload. The responder
        // inspects the request body so the same mock can serve any
        // structured call — for tagging we return a deterministic
        // `{"tags":[...]}` shape.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ChatResponder {
                counters: counters.clone(),
            })
            .mount(&server)
            .await;

        Self { server, counters }
    }

    /// Base URL the `OpenAICompatProvider` should hit. No `/v1` suffix —
    /// the provider normalizes the URL itself.
    pub fn base_url(&self) -> String {
        self.server.uri()
    }

    pub fn embedding_request_count(&self) -> usize {
        self.counters.embedding_requests.load(Ordering::Relaxed)
    }

    pub fn chat_request_count(&self) -> usize {
        self.counters.chat_requests.load(Ordering::Relaxed)
    }

    /// The `model` requested by each chat-completions call so far, in
    /// arrival order.
    pub fn chat_request_models(&self) -> Vec<String> {
        self.counters
            .chat_models
            .lock()
            .expect("chat_models lock")
            .clone()
    }

    /// Make `/v1/embeddings` fail with `failure` until cleared with `None`.
    /// Requests are still counted while failing.
    pub fn set_embedding_failure(&self, failure: Option<InjectedFailure>) {
        *self
            .counters
            .embedding_failure
            .lock()
            .expect("embedding_failure lock") = failure;
    }

    /// Make `/v1/chat/completions` fail with `failure` until cleared with
    /// `None`. Requests are still counted while failing.
    pub fn set_chat_failure(&self, failure: Option<InjectedFailure>) {
        *self
            .counters
            .chat_failure
            .lock()
            .expect("chat_failure lock") = failure;
    }

    /// Hold every `/v1/chat/completions` response for `delay` until cleared
    /// with `None`. Lets tests keep several chat requests concurrently
    /// in flight (e.g. concurrency-cap assertions) without racing the
    /// responder.
    pub fn set_chat_delay(&self, delay: Option<std::time::Duration>) {
        *self.counters.chat_delay.lock().expect("chat_delay lock") = delay;
    }

    pub fn reset_counts(&self) {
        self.counters.embedding_requests.store(0, Ordering::Relaxed);
        self.counters.chat_requests.store(0, Ordering::Relaxed);
        self.counters
            .chat_models
            .lock()
            .expect("chat_models lock")
            .clear();
    }
}

/// Bag-of-words style unit-vector embedder. Two texts sharing words land
/// at the same positions → high cosine similarity → edge crosses the 0.5
/// threshold. Disjoint texts end up near-orthogonal.
fn embed_text(text: &str) -> Vec<f32> {
    let mut vec = vec![0.0f32; EMBED_DIM];
    for word in text.split_whitespace() {
        let normalized: String = word
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if normalized.is_empty() {
            continue;
        }
        let mut h = DefaultHasher::new();
        normalized.hash(&mut h);
        let idx = (h.finish() as usize) % EMBED_DIM;
        vec[idx] += 1.0;
    }
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    } else {
        // Empty/punctuation-only input — put a constant at position 0 so
        // every row still has a valid unit vector.
        vec[0] = 1.0;
    }
    vec
}

/// Build a streaming `chat/completions` response. Detects whether the agent
/// is on its first turn (no prior tool results) or has tool results in its
/// message log, and emits the matching SSE stream:
///
/// - First turn: a single `tool_calls` delta requesting `search_atoms` with
///   a query plucked from the most recent user message. Closes with
///   `finish_reason: tool_calls`.
/// - Tool results present: a single content delta with deterministic text,
///   closes with `finish_reason: stop`.
///
/// The provider parser is line-oriented (`data: ...\n`) and accepts the
/// stream as a single body payload, so we don't need true chunked transfer
/// to satisfy it.
fn streaming_chat_response(body: &Value) -> ResponseTemplate {
    let has_tool_results = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|msgs| {
            msgs.iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        })
        .unwrap_or(false);

    let sse_body = if has_tool_results {
        // Second leg: agent has tool results, emit final assistant text.
        let chunks = [
            json!({
                "choices": [{
                    "delta": { "content": "Mock assistant reply grounded in the search results." },
                    "finish_reason": null,
                }]
            }),
            json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "stop",
                }]
            }),
        ];
        sse_concat(&chunks)
    } else {
        // First leg: ask the runtime to run `search_atoms`. The query is
        // lifted from the most recent user message so the search hits the
        // seeded atoms verbatim. The tool-call id must be unique per
        // response — the runtime persists tool calls by this id, and
        // concurrent conversations would otherwise collide on it.
        let query = latest_user_query(body).unwrap_or_else(|| "atomic".to_string());
        let arguments = json!({ "query": query, "limit": 5 }).to_string();
        static TOOL_CALL_SEQ: AtomicUsize = AtomicUsize::new(0);
        let call_id = format!(
            "call_mock_search_{}",
            TOOL_CALL_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let chunks = [
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": "search_atoms",
                                "arguments": arguments,
                            }
                        }]
                    },
                    "finish_reason": null,
                }]
            }),
            json!({
                "choices": [{
                    "delta": {},
                    "finish_reason": "tool_calls",
                }]
            }),
        ];
        sse_concat(&chunks)
    };

    ResponseTemplate::new(200)
        .insert_header("Content-Type", "text/event-stream")
        .set_body_raw(sse_body.into_bytes(), "text/event-stream")
}

fn sse_concat(chunks: &[Value]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str("data: ");
        out.push_str(&chunk.to_string());
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn latest_user_query(body: &Value) -> Option<String> {
    let messages = body.get("messages")?.as_array()?;
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(|v| v.as_str()) == Some("user") {
            return msg
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

/// Count `[N]` markers in the LLM request's user message — used by the
/// wiki-generation responder to figure out how many numbered sources were
/// embedded in the prompt so it can cite at least one of them.
fn count_numbered_sources(body: &Value) -> i32 {
    let mut max_seen = 0i32;
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                for cap in content.split('[').skip(1) {
                    if let Some(end) = cap.find(']') {
                        if let Ok(n) = cap[..end].parse::<i32>() {
                            if n > max_seen {
                                max_seen = n;
                            }
                        }
                    }
                }
            }
        }
    }
    max_seen
}

/// Wiki incremental updates label new sources with indices that start
/// strictly *after* the existing citations. Recover that starting index
/// from the prompt — it's the first marker following the
/// `NEW SOURCES TO INCORPORATE (cite as [N]` substring.
fn first_new_source_index(body: &Value) -> Option<i32> {
    let messages = body.get("messages")?.as_array()?;
    for msg in messages {
        if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
            // The prompt text explicitly tells the LLM where new sources
            // start: "NEW SOURCES TO INCORPORATE (cite as [N] onwards)".
            if let Some(anchor) = content.find("NEW SOURCES TO INCORPORATE (cite as [") {
                let tail = &content[anchor + "NEW SOURCES TO INCORPORATE (cite as [".len()..];
                if let Some(end) = tail.find(']') {
                    if let Ok(n) = tail[..end].parse::<i32>() {
                        return Some(n);
                    }
                }
            }
        }
    }
    None
}

struct EmbedResponder {
    counters: Arc<MockAiCounters>,
}

impl Respond for EmbedResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.counters
            .embedding_requests
            .fetch_add(1, Ordering::Relaxed);
        if let Some(failure) = *self
            .counters
            .embedding_failure
            .lock()
            .expect("embedding_failure lock")
        {
            return failure.response();
        }
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return ResponseTemplate::new(400),
        };
        let Some(inputs) = body.get("input").and_then(|v| v.as_array()) else {
            return ResponseTemplate::new(400);
        };
        let data: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let text = text.as_str().unwrap_or_default();
                json!({
                    "object": "embedding",
                    "index": index,
                    "embedding": embed_text(text),
                })
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": data,
            "model": body.get("model").cloned().unwrap_or(Value::Null),
        }))
    }
}

struct ChatResponder {
    counters: Arc<MockAiCounters>,
}

impl Respond for ChatResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.counters.chat_requests.fetch_add(1, Ordering::Relaxed);
        // Latency injection applies to every chat response uniformly —
        // success, injected failure, or malformed-request 400.
        let delay = *self.counters.chat_delay.lock().expect("chat_delay lock");
        let with_delay = |response: ResponseTemplate| match delay {
            Some(d) => response.set_delay(d),
            None => response,
        };
        if let Some(failure) = *self
            .counters
            .chat_failure
            .lock()
            .expect("chat_failure lock")
        {
            return with_delay(failure.response());
        }
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return with_delay(ResponseTemplate::new(400)),
        };
        if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
            self.counters
                .chat_models
                .lock()
                .expect("chat_models lock")
                .push(model.to_string());
        }

        // Streaming chat (agent loop). Detected by `stream: true` plus a
        // `tools` array — the chat agent always sends tools, while wiki /
        // tagging only stream when explicitly enabled (they don't, today).
        // Branch on whether the message log already contains tool results:
        //   - no tool results yet → emit a tool_calls SSE that requests
        //     `search_atoms`.
        //   - tool results present → emit a final text-content SSE.
        let is_streaming = body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let has_tools = body
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|t| !t.is_empty())
            .unwrap_or(false);
        if is_streaming && has_tools {
            return with_delay(streaming_chat_response(&body));
        }

        // Non-streaming chat with tools — used by the reports agentic loop.
        // The agent expects either a tool call (research) or a content
        // response with no tool calls (loop terminator). We short-circuit
        // by calling `done` immediately, which keeps the research phase
        // out of the report e2e tests (search-based tool flow is already
        // covered by slice 3c's chat suite).
        if !is_streaming && has_tools {
            return with_delay(ResponseTemplate::new(200).set_body_json(json!({
                "id": "mock-cmpl",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_done",
                            "type": "function",
                            "function": {
                                "name": "done",
                                "arguments": "{}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })));
        }

        // Inspect the requested schema name so this responder can serve
        // more than just tag extraction as the test matrix grows.
        //
        // prompt_only callers (wiki generation/updates, the report final
        // pass) send no `response_format` — the schema rides in an appended
        // instruction message instead — so when the pointer comes up empty,
        // identify the call by property names that appear in exactly one
        // schema.
        let request_text = body.to_string().to_lowercase();
        let schema_name = body
            .pointer("/response_format/json_schema/name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if request_text.contains("after_heading") {
                    "wiki_update_section_ops".to_string()
                } else if request_text.contains("article_content") {
                    "wiki_generation_result".to_string()
                } else if request_text.contains("finding_content") {
                    "report_generation_result".to_string()
                } else {
                    String::new()
                }
            });

        let content = match schema_name.as_str() {
            "extraction_result" => {
                let tag_name = if request_text.contains("biology") {
                    "Biology"
                } else if request_text.contains("cooking") || request_text.contains("pasta") {
                    "Cooking"
                } else {
                    "Physics"
                };
                json!({
                    "tags": [
                        { "name": tag_name, "parent_name": "Topics" },
                    ]
                })
                .to_string()
            }
            // Wiki full-article generation. The wiki prompt embeds source
            // chunks as `[1] excerpt...\n[2] excerpt...\n` etc.; the
            // citation extractor parses `\[(\d+)\]` against that source
            // list.
            //
            // Two marker-driven modes for the ledger e2e tests, keyed on
            // the tag name (which lands in the prompt as `Write a wiki
            // article about "{tag_name}"`):
            //
            // - `WikiFail...` → 400. Non-retryable at the provider layer
            //   (`is_retryable` excludes 400), so the failure surfaces
            //   immediately and the `task_runs` retry/backoff machinery —
            //   not the provider's internal retry — owns recovery.
            // - `WikiSlow...` → normal article after a delay, long enough
            //   that a concurrent regeneration request deterministically
            //   observes the first one's live lease.
            //
            // The legacy `update_wiki` path reuses this schema for a full
            // rewrite. Detect the update prompt shape ("NEW SOURCES TO
            // INCORPORATE") and emit a *different* body that pins the new
            // source index — otherwise the update returns content
            // byte-identical to the original generation and tests can't
            // distinguish the two.
            "wiki_generation_result" => {
                if request_text.contains("wikifail") {
                    return with_delay(ResponseTemplate::new(400).set_body_json(json!({
                        "error": { "message": "mock wiki generation failure" }
                    })));
                }
                let n = count_numbered_sources(&body);
                if let Some(new_index) = first_new_source_index(&body) {
                    // Update path: cite the new source so the test can
                    // verify the freshly added atom is integrated.
                    json!({
                        "article_content": format!(
                            "# Mock Wiki\n\nUpdated article body integrating new source. [1] [{new_index}]"
                        ),
                        "citations_used": [1, new_index],
                    })
                    .to_string()
                } else {
                    let cited: Vec<i32> = (1..=n.min(2).max(1)).collect();
                    let markers = cited
                        .iter()
                        .map(|i| format!("[{i}]"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    json!({
                        "article_content": format!(
                            "# Mock Wiki\n\nThis is a deterministic mock article body. {markers}"
                        ),
                        "citations_used": cited,
                    })
                    .to_string()
                }
            }
            // Tag compaction. The route hands the LLM a flat list of
            // tag rows (`tag_id | name | parent_name | atom_count`). We
            // emit a single merge of the conventional test tag pair
            // ("MockLoser" → "MockWinner") when both names appear in the
            // prompt; otherwise return an empty array so the route
            // reports `tags_merged: 0` without touching real data.
            "merge_result" => {
                if request_text.contains("mockwinner") && request_text.contains("mockloser") {
                    json!({
                        "merges": [{
                            "winner_name": "MockWinner",
                            "loser_name": "MockLoser",
                            "reason": "Deterministic mock merge for the compaction test."
                        }]
                    })
                    .to_string()
                } else {
                    json!({ "merges": [] }).to_string()
                }
            }
            // Report final pass. The agent embeds source atoms as
            // `Source [N]: ...` blocks; cite the first one and
            // `extract_citations` resolves the marker against the citables
            // table so the finding atom gets a non-empty citation row.
            "report_generation_result" => json!({
                "finding_content": "# Mock Finding\n\nA deterministic mock finding body. [1]",
                "citations_used": [1],
            })
            .to_string(),
            // Wiki incremental update: emit a single AppendToSection op
            // pinned to the heading the existing article uses, referencing
            // the first new-source index. Tests assert that the update
            // resolves a citation pointing at the freshly added atom.
            "wiki_update_section_ops" => {
                let new_index = first_new_source_index(&body).unwrap_or(2);
                json!({
                    "operations": [
                        {
                            "op": "AppendToSection",
                            "heading": "Mock Wiki",
                            "after_heading": "",
                            "content": format!(
                                "Additional mock context referencing the new source. [{new_index}]"
                            ),
                        }
                    ],
                    "citations_used": [new_index],
                })
                .to_string()
            }
            // Default: empty content, still valid JSON for callers that
            // tolerate-parse. Individual tests can assert on the request
            // shape they care about.
            _ => "{}".to_string(),
        };

        let mut response = ResponseTemplate::new(200).set_body_json(json!({
            "id": "mock-cmpl",
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": content,
                    },
                    "finish_reason": "stop",
                }
            ],
        }));
        // Slow-wiki mode: hold the response long enough that overlapping
        // regeneration requests genuinely race the in-flight one's lease.
        if schema_name == "wiki_generation_result" && request_text.contains("wikislow") {
            response = response.set_delay(std::time::Duration::from_millis(1500));
        }
        with_delay(response)
    }
}
