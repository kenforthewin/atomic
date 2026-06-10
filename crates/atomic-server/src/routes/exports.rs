//! Background export routes.

use crate::db_extractor::request_manager;
use crate::state::AppState;
use actix_files::NamedFile;
use actix_web::http::header::{
    ContentDisposition, DispositionParam, DispositionType, HeaderValue, REFERRER_POLICY,
};
use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

#[utoipa::path(post, path = "/api/databases/{id}/exports/markdown", params(("id" = String, Path, description = "Database ID")), responses((status = 202, description = "Export job started")), tag = "databases")]
pub async fn start_markdown_export(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    // The export job captures the manager that governs *this* request, so a
    // composing layer's RequestDatabaseManager override carries through to
    // the background work, not just the synchronous response.
    let manager = request_manager(&req, &state);
    match state
        .export_jobs
        .start_markdown_export(manager, path.into_inner())
        .await
    {
        Ok(job) => HttpResponse::Accepted().json(job),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(get, path = "/api/exports/{id}", params(("id" = String, Path, description = "Export job ID")), responses((status = 200, description = "Export job status")), tag = "databases")]
pub async fn get_export_job(state: web::Data<AppState>, path: web::Path<String>) -> HttpResponse {
    match state.export_jobs.status(&path.into_inner(), true) {
        Ok(job) => HttpResponse::Ok().json(job),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(delete, path = "/api/exports/{id}", params(("id" = String, Path, description = "Export job ID")), responses((status = 200, description = "Export job cancelled or deleted")), tag = "databases")]
pub async fn cancel_or_delete_export_job(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    match state.export_jobs.cancel_or_delete(&path.into_inner()) {
        Ok(job) => HttpResponse::Ok().json(job),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
pub struct DownloadQuery {
    /// One-time download token returned by the export job status.
    pub token: String,
}

#[utoipa::path(
    get,
    path = "/api/exports/{id}/download",
    params(
        ("id" = String, Path, description = "Export job ID"),
        DownloadQuery
    ),
    responses(
        (status = 200, description = "Markdown export archive"),
        (status = 400, description = "Invalid or expired download token"),
        (status = 404, description = "Export artifact not found")
    ),
    tag = "databases",
    security(())
)]
pub async fn download_export(
    state: web::Data<AppState>,
    req: HttpRequest,
    path: web::Path<String>,
    query: web::Query<DownloadQuery>,
) -> HttpResponse {
    let artifact = match state
        .export_jobs
        .validate_download(&path.into_inner(), &query.token)
    {
        Ok(artifact) => artifact,
        Err(e) => return crate::error::error_response(e),
    };

    let file = match NamedFile::open_async(&artifact.path).await {
        Ok(file) => file.set_content_disposition(ContentDisposition {
            disposition: DispositionType::Attachment,
            parameters: vec![DispositionParam::Filename(artifact.filename)],
        }),
        Err(e) => {
            return crate::error::error_response(atomic_core::AtomicCoreError::Io(e));
        }
    };

    let mut response = file.into_response(&req);
    response
        .headers_mut()
        .insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}
