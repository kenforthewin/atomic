//! Reports REST endpoints. CRUD on report definitions, history view,
//! manual "run now" trigger.
//!
//! Run-now is async: it spawns the runner on the tokio runtime and
//! returns 202 with the report id immediately. The finding atom is
//! observable via `GET /api/reports/:id/findings` (and via the standard
//! atom endpoints once the embedding pipeline finishes).

use crate::db_extractor::Db;
use crate::error::{error_response, ok_or_error, ApiErrorResponse};
use crate::state::{AppState, ServerEvent};
use actix_web::{web, HttpResponse};
use atomic_core::models::{CreateReportRequest, UpdateReportRequest};
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

#[utoipa::path(
    get,
    path = "/api/reports",
    responses(
        (status = 200, description = "All reports for this database",
         body = Vec<atomic_core::models::Report>)
    ),
    tag = "reports"
)]
pub async fn list_reports(db: Db) -> HttpResponse {
    ok_or_error(db.0.list_reports().await)
}

#[utoipa::path(
    get,
    path = "/api/reports/{id}",
    responses(
        (status = 200, description = "The requested report",
         body = atomic_core::models::Report),
        (status = 404, description = "No report with this id",
         body = ApiErrorResponse)
    ),
    tag = "reports"
)]
pub async fn get_report(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    match db.0.get_report(&id).await {
        Ok(Some(r)) => HttpResponse::Ok().json(r),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({"error": "not found"})),
        Err(e) => error_response(e),
    }
}

#[utoipa::path(
    post,
    path = "/api/reports",
    request_body = CreateReportRequest,
    responses(
        (status = 200, description = "Report created",
         body = atomic_core::models::Report),
        (status = 400, description = "Invalid schedule or timezone",
         body = ApiErrorResponse)
    ),
    tag = "reports"
)]
pub async fn create_report(db: Db, body: web::Json<CreateReportRequest>) -> HttpResponse {
    ok_or_error(db.0.create_report(body.into_inner()).await)
}

#[utoipa::path(
    put,
    path = "/api/reports/{id}",
    request_body = UpdateReportRequest,
    responses(
        (status = 200, description = "Report updated",
         body = atomic_core::models::Report),
        (status = 400, description = "Invalid schedule or timezone",
         body = ApiErrorResponse),
        (status = 404, description = "No report with this id",
         body = ApiErrorResponse)
    ),
    tag = "reports"
)]
pub async fn update_report(
    db: Db,
    path: web::Path<String>,
    body: web::Json<UpdateReportRequest>,
) -> HttpResponse {
    ok_or_error(
        db.0.update_report(&path.into_inner(), body.into_inner())
            .await,
    )
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetEnabledBody {
    pub enabled: bool,
}

#[utoipa::path(
    patch,
    path = "/api/reports/{id}/enabled",
    request_body = SetEnabledBody,
    responses(
        (status = 204, description = "Enabled flag updated"),
        (status = 404, description = "No report with this id",
         body = ApiErrorResponse)
    ),
    tag = "reports"
)]
pub async fn set_report_enabled(
    db: Db,
    path: web::Path<String>,
    body: web::Json<SetEnabledBody>,
) -> HttpResponse {
    match db
        .0
        .set_report_enabled(&path.into_inner(), body.enabled)
        .await
    {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(e) => error_response(e),
    }
}

#[utoipa::path(
    delete,
    path = "/api/reports/{id}",
    responses(
        (status = 204, description = "Report deleted"),
        (status = 404, description = "No report with this id",
         body = ApiErrorResponse)
    ),
    tag = "reports"
)]
pub async fn delete_report(db: Db, path: web::Path<String>) -> HttpResponse {
    match db.0.delete_report(&path.into_inner()).await {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(e) => error_response(e),
    }
}

#[derive(Debug, Deserialize, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
pub struct ListFindingsQuery {
    /// Max findings to return (default 50, max 200).
    pub limit: Option<i32>,
}

#[utoipa::path(
    get,
    path = "/api/reports/{id}/findings",
    params(ListFindingsQuery),
    responses(
        (status = 200, description = "Most-recent-first findings joined with atom snippet",
         body = Vec<(atomic_core::models::ReportFinding, atomic_core::models::AtomWithTags)>)
    ),
    tag = "reports"
)]
pub async fn list_findings_for_report(
    db: Db,
    path: web::Path<String>,
    query: web::Query<ListFindingsQuery>,
) -> HttpResponse {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    ok_or_error(
        db.0.list_findings_for_report(&path.into_inner(), limit)
            .await,
    )
}

#[utoipa::path(
    get,
    path = "/api/findings/{atom_id}/citations",
    responses(
        (status = 200, description = "Citation rows for a finding atom, ordered by position",
         body = Vec<atomic_core::models::ReportFindingCitation>)
    ),
    tag = "reports"
)]
pub async fn list_finding_citations(db: Db, path: web::Path<String>) -> HttpResponse {
    ok_or_error(db.0.list_citations_for_finding(&path.into_inner()).await)
}

/// Async manual-run response. Includes the report id and a hint at where
/// to poll for results; the actual finding atom shows up via the
/// findings endpoint once the agent loop completes.
#[derive(Debug, serde::Serialize, ToSchema)]
pub struct RunNowResponse {
    pub report_id: String,
    pub status: &'static str,
    pub findings_url: String,
}

#[utoipa::path(
    post,
    path = "/api/reports/{id}/run",
    responses(
        (status = 202, description = "Run dispatched; poll /findings for completion",
         body = RunNowResponse),
        (status = 404, description = "No report with this id",
         body = ApiErrorResponse)
    ),
    tag = "reports"
)]
pub async fn run_report_now(
    db: Db,
    path: web::Path<String>,
    state: web::Data<AppState>,
) -> HttpResponse {
    let id = path.into_inner();
    // Validate the report exists before reporting 202 — otherwise the
    // 404 would be deferred to the background task and the caller would
    // see a successful dispatch for a nonexistent id.
    match db.0.get_report(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return HttpResponse::NotFound().json(serde_json::json!({"error": "report not found"}))
        }
        Err(e) => return error_response(e),
    }

    let core = db.0.clone();
    let id_owned = id.clone();
    let event_tx = state.event_tx.clone();
    tokio::spawn(async move {
        match core.run_report_now(&id_owned).await {
            Ok(outcome) => {
                tracing::info!(
                    report_id = %id_owned,
                    outcome = ?outcome,
                    "[reports/run-now] complete"
                );
                // Broadcast `atom-created` on success so the dashboard
                // widget refreshes live. Matches what the scheduled-run
                // loop emits in `atomic-server::main`.
                if let atomic_core::reports::RunOutcome::Succeeded { finding_atom_id } = outcome {
                    if let Ok(Some(atom)) = core.get_atom(&finding_atom_id).await {
                        let _ = event_tx.send(ServerEvent::AtomCreated { atom });
                    }
                }
            }
            Err(e) => tracing::error!(
                report_id = %id_owned,
                error = %e,
                "[reports/run-now] failed"
            ),
        }
    });

    HttpResponse::Accepted().json(RunNowResponse {
        report_id: id.clone(),
        status: "dispatched",
        findings_url: format!("/api/reports/{id}/findings"),
    })
}
