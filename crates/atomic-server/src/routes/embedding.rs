//! Embedding management routes

use crate::db_extractor::{request_manager, Db};
use crate::error::ApiErrorResponse;
use crate::event_bridge::embedding_event_callback;
use crate::event_channel::EventChannel;
use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use atomic_core::models::PipelineStatus;
use atomic_core::registry::DatabaseInfo;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct DatabasePipelineStatus {
    pub database: DatabaseInfo,
    pub status: PipelineStatus,
}

#[derive(Serialize, ToSchema)]
pub struct AllPipelineStatuses {
    pub databases: Vec<DatabasePipelineStatus>,
}

#[utoipa::path(post, path = "/api/embeddings/process-pending", responses((status = 200, description = "Number of atoms queued for embedding")), tag = "embeddings")]
pub async fn process_pending_embeddings(events: EventChannel, db: Db) -> HttpResponse {
    let on_event = embedding_event_callback(events.0.clone());
    match db.0.process_pending_embeddings(on_event).await {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({"count": count})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/embeddings/process-tagging", responses((status = 200, description = "Number of atoms queued for tagging")), tag = "embeddings")]
pub async fn process_pending_tagging(events: EventChannel, db: Db) -> HttpResponse {
    let on_event = embedding_event_callback(events.0.clone());
    match db.0.process_pending_tagging(on_event).await {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({"count": count})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/embeddings/retry/{atom_id}", params(("atom_id" = String, Path, description = "Atom ID")), responses((status = 200, description = "Embedding retried"), (status = 404, description = "Atom not found", body = ApiErrorResponse)), tag = "embeddings")]
pub async fn retry_embedding(
    events: EventChannel,
    db: Db,
    path: web::Path<String>,
) -> HttpResponse {
    let atom_id = path.into_inner();
    let on_event = embedding_event_callback(events.0.clone());
    match db.0.retry_embedding(&atom_id, on_event).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"status": "ok"})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/tagging/retry/{atom_id}", params(("atom_id" = String, Path, description = "Atom ID")), responses((status = 200, description = "Tagging retried"), (status = 404, description = "Atom not found", body = ApiErrorResponse)), tag = "embeddings")]
pub async fn retry_tagging(events: EventChannel, db: Db, path: web::Path<String>) -> HttpResponse {
    let atom_id = path.into_inner();
    let on_event = embedding_event_callback(events.0.clone());
    match db.0.retry_tagging(&atom_id, on_event).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"status": "ok"})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/embeddings/retry-failed", responses((status = 200, description = "Number of failed embeddings queued")), tag = "embeddings")]
pub async fn retry_failed_embeddings(events: EventChannel, db: Db) -> HttpResponse {
    let on_event = embedding_event_callback(events.0.clone());
    match db.0.retry_failed_embeddings(on_event).await {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({"count": count})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/tagging/retry-failed", responses((status = 200, description = "Number of failed tagging jobs queued")), tag = "embeddings")]
pub async fn retry_failed_tagging(events: EventChannel, db: Db) -> HttpResponse {
    let on_event = embedding_event_callback(events.0.clone());
    match db.0.retry_failed_tagging(on_event).await {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({"count": count})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/embeddings/reembed-all", responses((status = 200, description = "Number of atoms queued for re-embedding")), tag = "embeddings")]
pub async fn reembed_all_atoms(events: EventChannel, db: Db) -> HttpResponse {
    let on_event = embedding_event_callback(events.0.clone());
    match db.0.reembed_all_atoms(on_event).await {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({"count": count})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/tagging/retag-all", responses((status = 200, description = "Number of atoms queued for re-tagging")), tag = "embeddings")]
pub async fn retag_all_atoms(events: EventChannel, db: Db) -> HttpResponse {
    let on_event = embedding_event_callback(events.0.clone());
    match db.0.retag_all_atoms(on_event).await {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({"count": count})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/embeddings/reset-stuck", responses((status = 200, description = "Number of stuck atoms reset")), tag = "embeddings")]
pub async fn reset_stuck_processing(db: Db) -> HttpResponse {
    match db.0.reset_stuck_processing().await {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({"count": count})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(get, path = "/api/embeddings/status", responses((status = 200, description = "Pipeline status summary", body = atomic_core::PipelineStatus)), tag = "embeddings")]
pub async fn get_pipeline_status(db: Db) -> HttpResponse {
    match db.0.get_pipeline_status().await {
        Ok(status) => HttpResponse::Ok().json(status),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(get, path = "/api/embeddings/status/all", responses((status = 200, description = "Pipeline status summary for all databases", body = AllPipelineStatuses)), tag = "embeddings")]
pub async fn get_all_pipeline_statuses(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> HttpResponse {
    // Cross-database fan-out: iterate the manager governing this request
    // (honoring a composing layer's RequestDatabaseManager override), not
    // AppState's unconditionally.
    let manager = request_manager(&req, &state);
    let (databases, _) = match manager.list_databases().await {
        Ok(result) => result,
        Err(e) => return crate::error::error_response(e),
    };

    let mut results = Vec::with_capacity(databases.len());
    for database in databases {
        let core = match manager.get_core(&database.id).await {
            Ok(core) => core,
            Err(e) => return crate::error::error_response(e),
        };
        let status = match core.get_pipeline_status().await {
            Ok(status) => status,
            Err(e) => return crate::error::error_response(e),
        };
        results.push(DatabasePipelineStatus { database, status });
    }

    HttpResponse::Ok().json(AllPipelineStatuses { databases: results })
}

#[utoipa::path(get, path = "/api/atoms/{id}/embedding-status", params(("id" = String, Path, description = "Atom ID")), responses((status = 200, description = "Embedding status"), (status = 404, description = "Atom not found", body = ApiErrorResponse)), tag = "embeddings")]
pub async fn get_embedding_status(db: Db, path: web::Path<String>) -> HttpResponse {
    let atom_id = path.into_inner();
    match db.0.get_embedding_status(&atom_id).await {
        Ok(status) => HttpResponse::Ok().json(serde_json::json!({"status": status})),
        Err(e) => crate::error::error_response(e),
    }
}
