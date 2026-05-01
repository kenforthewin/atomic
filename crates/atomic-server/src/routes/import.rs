//! Import routes

use crate::db_extractor::Db;
use crate::event_bridge::embedding_event_callback;
use crate::state::{AppState, ServerEvent};
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
    state: web::Data<AppState>,
    db: Db,
    body: web::Json<ImportObsidianRequest>,
) -> HttpResponse {
    let on_event = embedding_event_callback(state.event_tx.clone());
    let tx = state.event_tx.clone();
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
        Ok(result) => {
            // Fire-and-forget health maintenance after bulk import
            let core = db.0.clone();
            tokio::spawn(async move {
                if let Ok(report) = core.compute_health().await {
                    if report.overall_score < 95 {
                        let req = atomic_core::health::FixRequest {
                            checks: None,
                            mode: "auto".to_string(),
                            include_medium: false,
                        };
                        let _ = core.run_health_fix(&req).await;
                    }
                }
            });
            HttpResponse::Ok().json(result)
        }
        Err(e) => crate::error::error_response(e),
    }
}
