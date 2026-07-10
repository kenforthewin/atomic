//! SQLite → Postgres migration routes.
//!
//! Two entry points, one per side of a migration:
//! - `POST /api/migrations/sqlite` (Postgres server): raw SQLite file upload
//!   that becomes a new logical database via a background import job.
//! - `POST /api/migrations/push` (SQLite server / desktop sidecar): snapshot
//!   a local database and push it to a remote server's upload endpoint,
//!   mirroring the remote job's progress locally.
//!
//! Both are polled via `GET /api/migrations/{id}` and cancelled via DELETE.

use crate::migration_jobs::{max_upload_bytes, PushRequest};
use crate::state::AppState;
use actix_web::{web, HttpResponse};
use atomic_core::AtomicCoreError;
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use utoipa::{IntoParams, ToSchema};

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

#[derive(Deserialize, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
pub struct UploadQuery {
    /// Display name for the new database.
    pub name: String,
    /// Land migrated feeds paused (default false — the CLI and direct API
    /// callers decide; the desktop push flow defaults to true on its side).
    pub pause_feeds: Option<bool>,
}

#[utoipa::path(
    post,
    path = "/api/migrations/sqlite",
    params(UploadQuery),
    request_body(content = Vec<u8>, content_type = "application/octet-stream", description = "Raw SQLite database file"),
    responses(
        (status = 202, description = "Import job started"),
        (status = 400, description = "Server is not running on Postgres storage, or the upload is not a SQLite file"),
        (status = 413, description = "Upload exceeds the size limit")
    ),
    tag = "migrations"
)]
pub async fn upload_sqlite_migration(
    state: web::Data<AppState>,
    query: web::Query<UploadQuery>,
    mut payload: web::Payload,
) -> HttpResponse {
    if !state.manager.is_postgres() {
        return crate::error::error_response(AtomicCoreError::Configuration(
            "This server runs on SQLite storage; migration import requires a Postgres-backed \
             server"
                .to_string(),
        ));
    }

    let (job_id, upload_path) = match state.migration_jobs.new_upload_path() {
        Ok(reserved) => reserved,
        Err(e) => return crate::error::error_response(e),
    };

    // Stream the body to disk with a hard size cap.
    let max_bytes = max_upload_bytes();
    let mut file = match tokio::fs::File::create(&upload_path).await {
        Ok(file) => file,
        Err(e) => return crate::error::error_response(AtomicCoreError::Io(e)),
    };
    let mut written = 0u64;
    while let Some(chunk) = payload.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                let _ = tokio::fs::remove_file(&upload_path).await;
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": format!("Upload interrupted: {e}")
                }));
            }
        };
        written += chunk.len() as u64;
        if written > max_bytes {
            let _ = tokio::fs::remove_file(&upload_path).await;
            return HttpResponse::PayloadTooLarge().json(serde_json::json!({
                "error": format!("Upload exceeds the {max_bytes}-byte limit")
            }));
        }
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&upload_path).await;
            return crate::error::error_response(AtomicCoreError::Io(e));
        }
    }
    if let Err(e) = file.flush().await {
        return crate::error::error_response(AtomicCoreError::Io(e));
    }
    drop(file);

    if !has_sqlite_magic(&upload_path) {
        let _ = tokio::fs::remove_file(&upload_path).await;
        return crate::error::error_response(AtomicCoreError::Validation(
            "Upload is not a SQLite database file".to_string(),
        ));
    }

    let query = query.into_inner();
    match state.migration_jobs.start_import(
        state.manager.clone(),
        job_id,
        upload_path,
        query.name,
        query.pause_feeds.unwrap_or(false),
    ) {
        Ok(job) => HttpResponse::Accepted().json(job),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(
    post,
    path = "/api/migrations/push",
    request_body = PushRequest,
    responses(
        (status = 202, description = "Push job started"),
        (status = 400, description = "Server is not running on SQLite storage"),
        (status = 404, description = "Local database not found")
    ),
    tag = "migrations"
)]
pub async fn push_migration(
    state: web::Data<AppState>,
    body: web::Json<PushRequest>,
) -> HttpResponse {
    if state.manager.is_postgres() {
        return crate::error::error_response(AtomicCoreError::Configuration(
            "Migration push runs on the local SQLite instance, not the Postgres server".to_string(),
        ));
    }

    let request = body.into_inner();
    let local_db_name = match state.manager.list_databases().await {
        Ok((databases, _)) => match databases
            .into_iter()
            .find(|db| db.id == request.database_id)
        {
            Some(db) => db.name,
            None => {
                return crate::error::error_response(AtomicCoreError::NotFound(format!(
                    "Database '{}'",
                    request.database_id
                )))
            }
        },
        Err(e) => return crate::error::error_response(e),
    };
    let core = match state.manager.get_core(&request.database_id).await {
        Ok(core) => core,
        Err(e) => return crate::error::error_response(e),
    };

    match state
        .migration_jobs
        .start_push(core, request, local_db_name)
    {
        Ok(job) => HttpResponse::Accepted().json(job),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(get, path = "/api/migrations/{id}", params(("id" = String, Path, description = "Migration job ID")), responses((status = 200, description = "Migration job status")), tag = "migrations")]
pub async fn get_migration_job(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    match state.migration_jobs.status(&path.into_inner()) {
        Ok(job) => HttpResponse::Ok().json(job),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(delete, path = "/api/migrations/{id}", params(("id" = String, Path, description = "Migration job ID")), responses((status = 200, description = "Migration job cancelled or deleted")), tag = "migrations")]
pub async fn cancel_or_delete_migration_job(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    match state.migration_jobs.cancel_or_delete(&path.into_inner()) {
        Ok(job) => HttpResponse::Ok().json(job),
        Err(e) => crate::error::error_response(e),
    }
}

fn has_sqlite_magic(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 16];
    file.read_exact(&mut magic).is_ok() && &magic == SQLITE_MAGIC
}
