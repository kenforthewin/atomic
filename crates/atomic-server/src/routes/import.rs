//! Import routes

use crate::db_extractor::Db;
use crate::event_bridge::embedding_event_callback;
use crate::event_channel::EventChannel;
use crate::state::ServerEvent;
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, Serialize, ToSchema)]
pub struct ImportObsidianRequest {
    /// Path to Obsidian vault directory
    pub vault_path: String,
    /// Max notes to import (all if not set)
    pub max_notes: Option<i32>,
}

#[utoipa::path(post, path = "/api/import/obsidian", request_body = ImportObsidianRequest, responses((status = 200, description = "Import result")), tag = "import")]
pub async fn import_obsidian_vault(
    events: EventChannel,
    db: Db,
    body: web::Json<ImportObsidianRequest>,
) -> HttpResponse {
    let on_event = embedding_event_callback(events.0.clone());
    let tx = events.0.clone();
    let on_progress = move |progress: atomic_core::ImportProgress| {
        let _ = tx.send(ServerEvent::ImportProgress {
            current: progress.current,
            total: progress.total,
            current_file: progress.current_file,
            status: progress.status,
        });
    };

    match db
        .0
        .import_obsidian_vault(&body.vault_path, body.max_notes, on_event, on_progress)
        .await
    {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(e) => crate::error::error_response(e),
    }
}
