//! Bridge between atomic-core callback events and the server broadcast channel
//!
//! Provides helper functions that create callback closures which forward
//! EmbeddingEvent and ChatEvent instances into the tokio broadcast channel
//! as ServerEvent variants.

use crate::state::ServerEvent;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Create an EmbeddingEvent callback that broadcasts to WebSocket clients
pub fn embedding_event_callback(
    tx: broadcast::Sender<ServerEvent>,
) -> impl Fn(atomic_core::EmbeddingEvent) + Send + Sync + Clone + 'static {
    move |event: atomic_core::EmbeddingEvent| {
        let _ = tx.send(ServerEvent::from(event));
    }
}

/// Create an IngestionEvent callback that broadcasts to WebSocket clients
pub fn ingestion_event_callback(
    tx: broadcast::Sender<ServerEvent>,
) -> impl Fn(atomic_core::IngestionEvent) + Send + Sync + Clone + 'static {
    move |event: atomic_core::IngestionEvent| {
        let _ = tx.send(ServerEvent::from(event));
    }
}

/// Create a ChatEvent callback that broadcasts to WebSocket clients
pub fn chat_event_callback(
    tx: broadcast::Sender<ServerEvent>,
) -> impl Fn(atomic_core::ChatEvent) + Send + Sync + 'static {
    move |event: atomic_core::ChatEvent| {
        let _ = tx.send(ServerEvent::from(event));
    }
}

/// Create a TaskEvent callback for the scheduler. With the daily briefing
/// retired in phase 3, every remaining task event is debug-logged and
/// otherwise dropped — finding atoms produced by the reports loop ride
/// the standard `atom_created` / embedding-event broadcast.
pub fn task_event_callback(
    tx: broadcast::Sender<ServerEvent>,
) -> Arc<dyn Fn(atomic_core::scheduler::TaskEvent) + Send + Sync> {
    // `tx` is kept on the signature so existing call sites and the future
    // wiring back of task-scoped events do not have to ripple-change.
    let _ = &tx;
    Arc::new(move |event: atomic_core::scheduler::TaskEvent| {
        use atomic_core::scheduler::TaskEvent;
        match event {
            TaskEvent::Started { task_id, db_id } => {
                tracing::debug!(task_id, db_id, "[scheduler] task started");
            }
            TaskEvent::Completed { task_id, db_id, .. } => {
                tracing::debug!(task_id, db_id, "[scheduler] task completed");
            }
            TaskEvent::Failed {
                task_id,
                db_id,
                error,
            } => {
                tracing::debug!(task_id, db_id, error = %error, "[scheduler] task failed");
            }
        }
    })
}
