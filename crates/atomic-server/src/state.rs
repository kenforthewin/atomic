//! Application state and server event types

use crate::export_jobs::ExportJobManager;
use crate::log_buffer::LogBuffer;
use atomic_core::{AtomicCore, DatabaseManager};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, Mutex as AsyncMutex};

const SETUP_CLAIM_LIMIT: usize = 10;
const SETUP_CLAIM_WINDOW: Duration = Duration::from_secs(60);

/// Hashed setup token configured through ATOMIC_SETUP_TOKEN.
pub struct SetupToken {
    hash: String,
}

impl SetupToken {
    pub fn from_raw(raw: String) -> Option<Self> {
        let token = raw.trim();
        if token.is_empty() {
            return None;
        }
        Some(Self {
            hash: hash_setup_token(token),
        })
    }

    pub fn verify(&self, candidate: &str) -> bool {
        hash_setup_token(candidate.trim()) == self.hash
    }
}

fn hash_setup_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Small in-memory limiter for the public setup claim endpoint.
pub struct SetupClaimLimiter {
    attempts: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl SetupClaimLimiter {
    pub fn new() -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
        }
    }

    pub fn check(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut attempts = match self.attempts.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        let entries = attempts.entry(ip).or_default();
        while entries
            .front()
            .is_some_and(|at| now.duration_since(*at) > SETUP_CLAIM_WINDOW)
        {
            entries.pop_front();
        }
        if entries.len() >= SETUP_CLAIM_LIMIT {
            return false;
        }
        entries.push_back(now);
        true
    }
}

/// Shared application state for all route handlers
pub struct AppState {
    pub manager: Arc<DatabaseManager>,
    pub event_tx: broadcast::Sender<ServerEvent>,
    /// Public URL for OAuth discovery (set via --public-url CLI flag)
    pub public_url: Option<String>,
    /// In-memory ring buffer for recent log lines (for user export)
    pub log_buffer: LogBuffer,
    /// Background database export jobs and temporary artifacts.
    pub export_jobs: ExportJobManager,
    /// Optional setup token required for first-run claims.
    pub setup_token: Option<SetupToken>,
    /// Explicit unsafe opt-out from requiring ATOMIC_SETUP_TOKEN for setup claims.
    pub dangerously_skip_setup_token: bool,
    /// Serializes setup claims inside this process.
    pub setup_claim_lock: AsyncMutex<()>,
    /// Rate-limits setup claim attempts by client IP.
    pub setup_claim_limiter: SetupClaimLimiter,
}

impl AppState {
    /// Resolve which database core to use for a request.
    /// Checks X-Atomic-Database header, then ?db= query param, then falls back to active.
    /// Delegates to [`crate::db_extractor::resolve_core`], which owns the
    /// per-request selection rules for any manager.
    ///
    /// **Warning:** this resolves against `self.manager` only — it never
    /// consults the
    /// [`RequestDatabaseManager`](crate::db_extractor::RequestDatabaseManager)
    /// request extension, so it silently ignores a composing layer's
    /// per-request manager override. Handler code should use the
    /// [`Db`](crate::db_extractor::Db) extractor (or
    /// [`request_manager`](crate::db_extractor::request_manager) +
    /// `resolve_core`) instead; reach for this method only when you
    /// explicitly want *this* state's manager regardless of the request's
    /// composition context.
    pub async fn resolve_core(
        &self,
        req: &actix_web::HttpRequest,
    ) -> Result<AtomicCore, atomic_core::AtomicCoreError> {
        crate::db_extractor::resolve_core(&self.manager, req).await
    }
}

/// Events broadcast to WebSocket clients
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    // Embedding pipeline events
    EmbeddingStarted {
        atom_id: String,
    },
    EmbeddingComplete {
        atom_id: String,
    },
    EmbeddingFailed {
        atom_id: String,
        error: String,
    },
    TaggingComplete {
        atom_id: String,
        tags_extracted: Vec<String>,
        new_tags_created: Vec<String>,
    },
    TaggingFailed {
        atom_id: String,
        error: String,
    },
    TaggingSkipped {
        atom_id: String,
    },
    BatchProgress {
        batch_id: String,
        phase: String,
        completed: usize,
        total: usize,
    },
    PipelineQueueStarted {
        run_id: String,
        total_jobs: usize,
        embedding_total: usize,
    },
    PipelineQueueProgress {
        run_id: String,
        stage: String,
        completed: usize,
        total: usize,
    },
    PipelineQueueCompleted {
        run_id: String,
        total_jobs: usize,
        failed_jobs: usize,
    },
    EventsLagged {
        skipped: u64,
    },

    // Atom lifecycle events
    AtomCreated {
        atom: atomic_core::AtomWithTags,
    },
    AtomUpdated {
        atom: atomic_core::AtomWithTags,
    },

    /// The per-DB `dashboard.featured_report_id` pointer changed.
    /// Broadcast on every write through the dashboard route so the
    /// BriefingWidget and any open detail-view star can refetch.
    /// `report_id` is `None` when the pointer was cleared.
    DashboardFeaturedChanged {
        report_id: Option<String>,
    },

    // Import progress events
    ImportProgress {
        current: i32,
        total: i32,
        current_file: String,
        status: String,
    },

    // Ingestion pipeline events
    IngestionFetchStarted {
        url: String,
        request_id: String,
    },
    IngestionFetchComplete {
        url: String,
        request_id: String,
        content_length: usize,
    },
    IngestionFetchFailed {
        url: String,
        request_id: String,
        error: String,
    },
    IngestionSkipped {
        url: String,
        request_id: String,
        reason: String,
    },
    IngestionComplete {
        request_id: String,
        atom_id: String,
        url: String,
        title: String,
    },
    IngestionFailed {
        request_id: String,
        url: String,
        error: String,
    },
    FeedPollComplete {
        feed_id: String,
        new_items: i32,
        skipped: i32,
        errors: i32,
    },
    FeedPollFailed {
        feed_id: String,
        error: String,
    },

    // Chat streaming events
    ChatStreamDelta {
        conversation_id: String,
        content: String,
    },
    ChatToolStart {
        conversation_id: String,
        tool_call_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    ChatToolComplete {
        conversation_id: String,
        tool_call_id: String,
        results_count: i32,
    },
    ChatComplete {
        conversation_id: String,
        message: atomic_core::ChatMessageWithContext,
    },
    ChatCanvasAction {
        conversation_id: String,
        action: String,
        params: serde_json::Value,
    },
    ChatError {
        conversation_id: String,
        error: String,
    },
}

impl From<atomic_core::EmbeddingEvent> for ServerEvent {
    fn from(event: atomic_core::EmbeddingEvent) -> Self {
        match event {
            atomic_core::EmbeddingEvent::Started { atom_id } => {
                ServerEvent::EmbeddingStarted { atom_id }
            }
            atomic_core::EmbeddingEvent::EmbeddingComplete { atom_id } => {
                ServerEvent::EmbeddingComplete { atom_id }
            }
            atomic_core::EmbeddingEvent::EmbeddingFailed { atom_id, error } => {
                ServerEvent::EmbeddingFailed { atom_id, error }
            }
            atomic_core::EmbeddingEvent::TaggingComplete {
                atom_id,
                tags_extracted,
                new_tags_created,
            } => ServerEvent::TaggingComplete {
                atom_id,
                tags_extracted,
                new_tags_created,
            },
            atomic_core::EmbeddingEvent::TaggingFailed { atom_id, ref error } => {
                tracing::warn!(atom_id, error = %error, "Tagging failed");
                ServerEvent::TaggingFailed {
                    atom_id,
                    error: error.clone(),
                }
            }
            atomic_core::EmbeddingEvent::TaggingSkipped { atom_id } => {
                ServerEvent::TaggingSkipped { atom_id }
            }
            atomic_core::EmbeddingEvent::BatchProgress {
                batch_id,
                phase,
                completed,
                total,
            } => ServerEvent::BatchProgress {
                batch_id,
                phase,
                completed,
                total,
            },
            atomic_core::EmbeddingEvent::PipelineQueueStarted {
                run_id,
                total_jobs,
                embedding_total,
            } => ServerEvent::PipelineQueueStarted {
                run_id,
                total_jobs,
                embedding_total,
            },
            atomic_core::EmbeddingEvent::PipelineQueueProgress {
                run_id,
                stage,
                completed,
                total,
            } => ServerEvent::PipelineQueueProgress {
                run_id,
                stage,
                completed,
                total,
            },
            atomic_core::EmbeddingEvent::PipelineQueueCompleted {
                run_id,
                total_jobs,
                failed_jobs,
            } => ServerEvent::PipelineQueueCompleted {
                run_id,
                total_jobs,
                failed_jobs,
            },
        }
    }
}

impl From<atomic_core::IngestionEvent> for ServerEvent {
    fn from(event: atomic_core::IngestionEvent) -> Self {
        match event {
            atomic_core::IngestionEvent::FetchStarted { url, request_id } => {
                ServerEvent::IngestionFetchStarted { url, request_id }
            }
            atomic_core::IngestionEvent::FetchComplete {
                url,
                request_id,
                content_length,
            } => ServerEvent::IngestionFetchComplete {
                url,
                request_id,
                content_length,
            },
            atomic_core::IngestionEvent::FetchFailed {
                url,
                request_id,
                error,
            } => ServerEvent::IngestionFetchFailed {
                url,
                request_id,
                error,
            },
            atomic_core::IngestionEvent::Skipped {
                url,
                request_id,
                reason,
            } => ServerEvent::IngestionSkipped {
                url,
                request_id,
                reason,
            },
            atomic_core::IngestionEvent::IngestionComplete {
                request_id,
                atom_id,
                url,
                title,
            } => ServerEvent::IngestionComplete {
                request_id,
                atom_id,
                url,
                title,
            },
            atomic_core::IngestionEvent::IngestionFailed {
                request_id,
                url,
                error,
            } => ServerEvent::IngestionFailed {
                request_id,
                url,
                error,
            },
            atomic_core::IngestionEvent::FeedPollComplete {
                feed_id,
                new_items,
                skipped,
                errors,
            } => ServerEvent::FeedPollComplete {
                feed_id,
                new_items,
                skipped,
                errors,
            },
            atomic_core::IngestionEvent::FeedPollFailed { feed_id, error } => {
                ServerEvent::FeedPollFailed { feed_id, error }
            }
        }
    }
}

impl From<atomic_core::ChatEvent> for ServerEvent {
    fn from(event: atomic_core::ChatEvent) -> Self {
        match event {
            atomic_core::ChatEvent::StreamDelta {
                conversation_id,
                content,
            } => ServerEvent::ChatStreamDelta {
                conversation_id,
                content,
            },
            atomic_core::ChatEvent::ToolStart {
                conversation_id,
                tool_call_id,
                tool_name,
                tool_input,
            } => ServerEvent::ChatToolStart {
                conversation_id,
                tool_call_id,
                tool_name,
                tool_input,
            },
            atomic_core::ChatEvent::ToolComplete {
                conversation_id,
                tool_call_id,
                results_count,
            } => ServerEvent::ChatToolComplete {
                conversation_id,
                tool_call_id,
                results_count,
            },
            atomic_core::ChatEvent::Complete {
                conversation_id,
                message,
            } => ServerEvent::ChatComplete {
                conversation_id,
                message,
            },
            atomic_core::ChatEvent::CanvasAction {
                conversation_id,
                action,
                params,
            } => ServerEvent::ChatCanvasAction {
                conversation_id,
                action,
                params,
            },
            atomic_core::ChatEvent::AtomCreated {
                conversation_id: _,
                atom,
            } => ServerEvent::AtomCreated { atom },
            atomic_core::ChatEvent::AtomUpdated {
                conversation_id: _,
                atom,
            } => ServerEvent::AtomUpdated { atom },
            atomic_core::ChatEvent::AtomPipelineEvent {
                conversation_id: _,
                event,
            } => ServerEvent::from(event),
            atomic_core::ChatEvent::Error {
                conversation_id,
                error,
            } => ServerEvent::ChatError {
                conversation_id,
                error,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_started_conversion() {
        let event = atomic_core::EmbeddingEvent::Started {
            atom_id: "a1".into(),
        };
        let server_event = ServerEvent::from(event);
        match server_event {
            ServerEvent::EmbeddingStarted { atom_id } => assert_eq!(atom_id, "a1"),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_embedding_complete_conversion() {
        let event = atomic_core::EmbeddingEvent::EmbeddingComplete {
            atom_id: "a2".into(),
        };
        match ServerEvent::from(event) {
            ServerEvent::EmbeddingComplete { atom_id } => assert_eq!(atom_id, "a2"),
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_embedding_failed_conversion() {
        let event = atomic_core::EmbeddingEvent::EmbeddingFailed {
            atom_id: "a3".into(),
            error: "timeout".into(),
        };
        match ServerEvent::from(event) {
            ServerEvent::EmbeddingFailed { atom_id, error } => {
                assert_eq!(atom_id, "a3");
                assert_eq!(error, "timeout");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_tagging_complete_conversion() {
        let event = atomic_core::EmbeddingEvent::TaggingComplete {
            atom_id: "a4".into(),
            tags_extracted: vec!["t1".into()],
            new_tags_created: vec!["t2".into()],
        };
        match ServerEvent::from(event) {
            ServerEvent::TaggingComplete {
                atom_id,
                tags_extracted,
                new_tags_created,
            } => {
                assert_eq!(atom_id, "a4");
                assert_eq!(tags_extracted, vec!["t1"]);
                assert_eq!(new_tags_created, vec!["t2"]);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_chat_stream_delta_conversion() {
        let event = atomic_core::ChatEvent::StreamDelta {
            conversation_id: "c1".into(),
            content: "hello".into(),
        };
        match ServerEvent::from(event) {
            ServerEvent::ChatStreamDelta {
                conversation_id,
                content,
            } => {
                assert_eq!(conversation_id, "c1");
                assert_eq!(content, "hello");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_chat_tool_start_conversion() {
        let event = atomic_core::ChatEvent::ToolStart {
            conversation_id: "c2".into(),
            tool_call_id: "tc1".into(),
            tool_name: "search".into(),
            tool_input: serde_json::json!({"query": "test"}),
        };
        match ServerEvent::from(event) {
            ServerEvent::ChatToolStart {
                conversation_id,
                tool_name,
                tool_input,
                ..
            } => {
                assert_eq!(conversation_id, "c2");
                assert_eq!(tool_name, "search");
                assert_eq!(tool_input["query"], "test");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_chat_error_conversion() {
        let event = atomic_core::ChatEvent::Error {
            conversation_id: "c3".into(),
            error: "api failed".into(),
        };
        match ServerEvent::from(event) {
            ServerEvent::ChatError {
                conversation_id,
                error,
            } => {
                assert_eq!(conversation_id, "c3");
                assert_eq!(error, "api failed");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_server_event_serializes_with_type_tag() {
        let event = ServerEvent::EmbeddingComplete {
            atom_id: "a1".into(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "EmbeddingComplete");
        assert_eq!(json["atom_id"], "a1");
    }

    #[test]
    fn test_event_broadcast_delivery() {
        let (tx, mut rx) = broadcast::channel::<ServerEvent>(16);
        let event = ServerEvent::EmbeddingStarted {
            atom_id: "a1".into(),
        };
        tx.send(event).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            ServerEvent::EmbeddingStarted { atom_id } => assert_eq!(atom_id, "a1"),
            _ => panic!("Wrong variant"),
        }
    }
}
