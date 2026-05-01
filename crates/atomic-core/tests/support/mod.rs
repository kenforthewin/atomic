//! Shared infrastructure for pipeline integration tests.
//!
//! Stands up a local HTTP server that speaks enough of the OpenAI API to
//! drive the `OpenAICompat` provider end-to-end (`/v1/embeddings`,
//! `/v1/chat/completions`). Tests point `AtomicCore` at this mock by writing
//! the usual settings — the real reqwest client, real request serialization,
//! and real response parsing all run untouched. Only the network peer is fake.
//!
//! Also exposes a `Backend` switch so the same pipeline test runs against
//! both SQLite (always) and Postgres (when `ATOMIC_TEST_DATABASE_URL` is set
//! and the `postgres` feature is enabled).

#![allow(dead_code)] // Referenced by multiple test binaries; some helpers are per-test.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use atomic_core::AtomicCore;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Embedding dimension used by the mock. Must match the default
/// `openai_compat_embedding_dimension` + the SQLite `vec_chunks float[1536]`
/// schema so no dimension reconciliation kicks in mid-test.
pub const EMBED_DIM: usize = 1536;

/// Similarity threshold used by the pipeline when building semantic edges.
/// Kept here so tests can sanity-check that crafted atom pairs fall on the
/// correct side of the cutoff (see `embedding.rs::compute_semantic_edges...`).
pub const EDGE_SIMILARITY_THRESHOLD: f32 = 0.5;

// ==================== Mock AI server ====================

/// Local HTTP server mimicking OpenAI's `/v1/embeddings` and
/// `/v1/chat/completions`. Holds the server handle for lifetime management.
pub struct MockAiServer {
    server: MockServer,
    counters: Arc<MockAiCounters>,
}

#[derive(Default)]
struct MockAiCounters {
    embedding_requests: AtomicUsize,
    chat_requests: AtomicUsize,
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

        // Tag extraction goes through the non-streaming `complete` path with
        // a `response_format: json_schema` payload. The responder inspects
        // the request body so the same mock can serve any structured call —
        // for tagging we return a deterministic {"tags":[...]} shape.
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

    pub fn reset_counts(&self) {
        self.counters.embedding_requests.store(0, Ordering::Relaxed);
        self.counters.chat_requests.store(0, Ordering::Relaxed);
    }

    /// Mount a higher-priority chat-completion handler that returns a fixed `content`.
    /// Because wiremock matches later-mounted mocks first, this overrides the default
    /// `ChatResponder` for all subsequent requests.
    pub async fn mock_chat_completion(&self, content: impl Into<String>) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(FixedChatResponder {
                content: content.into(),
                counters: Arc::clone(&self.counters),
            })
            .with_priority(1)
            .mount(&self.server)
            .await;
    }
}

/// Bag-of-words style unit-vector embedder. Two texts sharing words land at
/// the same positions → high cosine similarity → edge crosses the 0.5
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

struct EmbedResponder {
    counters: Arc<MockAiCounters>,
}

impl Respond for EmbedResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        self.counters
            .embedding_requests
            .fetch_add(1, Ordering::Relaxed);
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
        let body: Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return ResponseTemplate::new(400),
        };

        // Inspect the requested schema name so this responder can serve
        // more than just tag extraction as the test matrix grows.
        let schema_name = body
            .pointer("/response_format/json_schema/name")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let request_text = body.to_string().to_lowercase();

        let content = match schema_name {
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
            // Default: empty content, still valid JSON for callers that
            // tolerate-parse. Individual tests can assert on the request
            // shape they care about.
            _ => "{}".to_string(),
        };

        ResponseTemplate::new(200).set_body_json(json!({
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
        }))
    }
}

struct FixedChatResponder {
    content: String,
    counters: Arc<MockAiCounters>,
}

impl Respond for FixedChatResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.counters.chat_requests.fetch_add(1, Ordering::Relaxed);
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": self.content},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
    }
}

// ==================== Backend switch + test harness ====================

pub enum Backend {
    Sqlite,
    #[cfg(feature = "postgres")]
    Postgres,
}

/// Per-test resources that must outlive the `AtomicCore`. Drop order matters
/// — the temp dir needs to live until after the core is dropped (SQLite has
/// the DB file open). For Postgres, holding nothing extra is fine.
pub struct CoreHandle {
    pub core: AtomicCore,
    _tempdir: Option<TempDir>,
}

/// Build an `AtomicCore` on the chosen backend and wire it up to the mock:
///
/// 1. Open a fresh DB (SQLite temp dir / Postgres truncated).
/// 2. Seed settings pointing at the mock's base URL with the
///    `openai_compat` provider selected.
/// 3. Seed a single auto-tag-target category ("Topics") so the tagging
///    path runs instead of short-circuiting on an empty tag tree.
///
/// Postgres: returns `None` if `ATOMIC_TEST_DATABASE_URL` isn't set so callers
/// can gracefully skip the test on CI configurations without a database.
pub async fn setup_core(backend: Backend, mock_url: &str) -> Option<CoreHandle> {
    let (core, tempdir) = match backend {
        Backend::Sqlite => {
            let dir = TempDir::new().expect("create tempdir");
            let core =
                AtomicCore::open_or_create(dir.path().join("pipeline.db")).expect("open sqlite");
            (core, Some(dir))
        }
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let url = std::env::var("ATOMIC_TEST_DATABASE_URL").ok()?;
            // Fresh schema per test run — truncate leaves the schema intact
            // but wipes seeded tags/settings so `open_postgres` re-seeds.
            truncate_postgres_for_test(&url).await;
            let core = AtomicCore::open_postgres(&url, "pipeline_test", None)
                .await
                .expect("open postgres");
            (core, None)
        }
    };

    // Point the pipeline at the mock HTTP server.
    for (k, v) in [
        ("provider", "openai_compat"),
        ("openai_compat_base_url", mock_url),
        ("openai_compat_api_key", "test-key"),
        ("openai_compat_embedding_model", "mock-embed"),
        ("openai_compat_llm_model", "mock-llm"),
        ("openai_compat_embedding_dimension", "1536"),
        ("auto_tagging_enabled", "true"),
    ] {
        core.set_setting(k, v).await.expect("seed test setting");
    }

    // Ensure at least one top-level auto-tag target exists so
    // `get_tag_tree_for_llm` returns a non-empty tree and the tagging path
    // actually runs. For SQLite we start with an empty tags table; for
    // Postgres `open_postgres` seeds default categories but leaves the
    // is_autotag_target flag off.
    core.configure_autotag_targets(&["Topics".to_string()], &[])
        .await
        .expect("configure autotag targets");

    Some(CoreHandle {
        core,
        _tempdir: tempdir,
    })
}

#[cfg(feature = "postgres")]
pub async fn truncate_postgres_for_test(url: &str) {
    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("connect truncate pool");
    // Same list as storage_tests.rs — keeps both files in sync.
    let _ = sqlx::raw_sql(
        "TRUNCATE atoms, tags, atom_tags, atom_chunks, atom_positions, atom_pipeline_jobs, \
         semantic_edges, atom_clusters, tag_embeddings, \
         wiki_articles, wiki_citations, wiki_links, wiki_article_versions, atom_links, \
         conversations, conversation_tags, chat_messages, chat_tool_calls, chat_citations, \
         feeds, feed_tags, feed_items, settings, \
         briefing_citations, briefings, oauth_codes, oauth_clients, api_tokens \
         CASCADE",
    )
    .execute(&pool)
    .await;
}

// ==================== Pipeline completion awaiter ====================

/// Event channel returned to a test so it can await specific pipeline
/// milestones without sprinkling `sleep`s.
pub type EventRx = UnboundedReceiver<atomic_core::EmbeddingEvent>;

/// Make an `on_event` callback that forwards every event into a channel.
/// Returns the callback (to hand to `create_atom`) and the receiver (to poll
/// in the test). The callback is `Arc`-backed because `create_atom`'s bound
/// is `Fn + Send + Sync + 'static`.
pub fn event_collector() -> (
    impl Fn(atomic_core::EmbeddingEvent) + Send + Sync + Clone + 'static,
    EventRx,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = Arc::new(tx);
    let cb = move |ev| {
        let _ = tx.send(ev);
    };
    (cb, rx)
}

/// Wait until both `EmbeddingComplete` and a terminal tagging event
/// (`TaggingComplete` / `TaggingSkipped` / `TaggingFailed`) have fired for
/// `atom_id`. Returns the captured events so tests can assert on payloads.
pub async fn await_pipeline(rx: &mut EventRx, atom_id: &str) -> Vec<atomic_core::EmbeddingEvent> {
    use atomic_core::EmbeddingEvent;

    let mut captured = Vec::new();
    let mut embedding_done = false;
    let mut tagging_done = false;

    // A generous budget — the mock responds instantly, but CI runners can
    // stall under load. Fails loudly instead of hanging forever.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

    while !(embedding_done && tagging_done) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "pipeline did not complete for {atom_id} within 15s. Captured: {:?}",
                captured
            );
        }

        let ev = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => panic!("event channel closed before pipeline finished"),
            Err(_) => panic!(
                "timed out waiting for pipeline events for {atom_id}. Captured: {:?}",
                captured
            ),
        };

        let matches_target = match &ev {
            EmbeddingEvent::Started { atom_id: id }
            | EmbeddingEvent::EmbeddingComplete { atom_id: id }
            | EmbeddingEvent::EmbeddingFailed { atom_id: id, .. }
            | EmbeddingEvent::TaggingComplete { atom_id: id, .. }
            | EmbeddingEvent::TaggingSkipped { atom_id: id }
            | EmbeddingEvent::TaggingFailed { atom_id: id, .. } => id == atom_id,
            EmbeddingEvent::BatchProgress { .. }
            | EmbeddingEvent::PipelineQueueStarted { .. }
            | EmbeddingEvent::PipelineQueueProgress { .. }
            | EmbeddingEvent::PipelineQueueCompleted { .. } => false,
        };

        if matches_target {
            match &ev {
                EmbeddingEvent::EmbeddingComplete { .. } => embedding_done = true,
                EmbeddingEvent::EmbeddingFailed { error, .. } => {
                    panic!("embedding failed for {atom_id}: {error}")
                }
                EmbeddingEvent::TaggingComplete { .. } | EmbeddingEvent::TaggingSkipped { .. } => {
                    tagging_done = true
                }
                EmbeddingEvent::TaggingFailed { error, .. } => {
                    panic!("tagging failed for {atom_id}: {error}")
                }
                _ => {}
            }
            captured.push(ev);
        }
    }

    captured
}
