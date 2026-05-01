//! Knowledge health check routes.
//!
//! GET  /api/health/knowledge          — compute & return full health report
//! POST /api/health/fix                — auto-fix by tier
//! POST /api/health/fix/{check}/{item} — fix one item requiring review
//! POST /api/health/undo/{fix_id}      — undo a previously applied fix
//! GET  /api/health/history            — last N stored reports (trending)
//! GET  /api/health/fixes/recent       — recent fix log entries

use crate::db_extractor::Db;
use actix_web::{web, HttpResponse};
use atomic_core::compaction;
use atomic_core::health::{
    self, audit, pair_key, FixRequest, FixResponse, HealthCheckResult, HealthReport,
};
use atomic_core::health::audit::{HealthFixLog, StoredHealthReport};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Request body for the per-item fix endpoint.
#[derive(Deserialize, Serialize, ToSchema)]
pub struct ManualFixRequest {
    pub action: String,
    // Optional per-action fields
    pub url: Option<String>,
    pub parent_id: Option<String>,
    pub into_tag_id: Option<String>,
    pub content: Option<String>,
    pub winner_atom_id: Option<String>,
    pub loser_atom_id: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// Query params for history endpoint.
#[derive(Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<i32>,
}

// ==================== GET /api/health/knowledge ====================

#[utoipa::path(
    get,
    path = "/api/health/knowledge",
    tag = "health",
    responses(
        (status = 200, description = "Current health report", body = HealthReport),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearer_auth" = [])),
)]
pub async fn get_health_knowledge(db: Db) -> HttpResponse {
    match health::compute_health(&db.0).await {
        Ok(report) => HttpResponse::Ok().json(report),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== POST /api/health/fix ====================

#[utoipa::path(
    post,
    path = "/api/health/fix",
    tag = "health",
    request_body = FixRequest,
    responses(
        (status = 200, description = "Fix response", body = FixResponse),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearer_auth" = [])),
)]
pub async fn run_health_fix(db: Db, body: web::Json<FixRequest>) -> HttpResponse {
    match health::run_fix(&db.0, &body).await {
        Ok(response) => HttpResponse::Ok().json(response),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== POST /api/health/fix/{check}/{item_id} ====================

#[utoipa::path(
    post,
    path = "/api/health/fix/{check}/{item_id}",
    tag = "health",
    params(
        ("check" = String, Path, description = "Check name"),
        ("item_id" = String, Path, description = "Item identifier"),
    ),
    request_body = ManualFixRequest,
    responses(
        (status = 200, description = "Action taken or no-op"),
        (status = 400, description = "Bad request"),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearer_auth" = [])),
)]
pub async fn apply_manual_fix(
    db: Db,
    path: web::Path<(String, String)>,
    body: web::Json<ManualFixRequest>,
) -> HttpResponse {
    let (check, item_id) = path.into_inner();

    match (check.as_str(), body.action.as_str()) {
        ("duplicate_detection", "merge_with_llm") => {
            // item_id is expected to be "atomA_atomB" (hyphen-separated)
            let parts: Vec<&str> = item_id.splitn(2, '_').collect();
            if parts.len() != 2 {
                return HttpResponse::BadRequest().json(serde_json::json!({
                    "error": "item_id must be 'atomA_id_atomB_id' for merge"
                }));
            }
            let atom_a = parts[0];
            let atom_b = parts[1];
            let dry_run = false;
            match atomic_core::health::llm_fixes::merge_duplicate_pair(
                &db.0, atom_a, atom_b, dry_run,
            )
            .await
            {
                Ok(Some(action)) => HttpResponse::Ok().json(action),
                Ok(None) => HttpResponse::Ok().json(serde_json::json!({"status": "no_op"})),
                Err(e) => crate::error::error_response(e),
            }
        }
        _ => HttpResponse::BadRequest().json(serde_json::json!({
            "error": format!("unsupported check '{}' or action '{}'", check, body.action)
        })),
    }
}

// ==================== POST /api/health/undo/{fix_id} ====================

#[utoipa::path(
    post,
    path = "/api/health/undo/{fix_id}",
    tag = "health",
    params(
        ("fix_id" = String, Path, description = "Fix ID from the audit log"),
    ),
    responses(
        (status = 200, description = "Undo successful"),
        (status = 404, description = "Fix not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearer_auth" = [])),
)]
pub async fn undo_health_fix(db: Db, path: web::Path<String>) -> HttpResponse {
    let fix_id = path.into_inner();
    match audit::undo(&db.0, &fix_id).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"status": "ok", "fix_id": fix_id})),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== GET /api/health/history ====================

#[utoipa::path(
    get,
    path = "/api/health/history",
    tag = "health",
    params(
        ("limit" = Option<i32>, Query, description = "Maximum number of reports to return"),
    ),
    responses(
        (status = 200, description = "Stored health reports", body = Vec<StoredHealthReport>),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearer_auth" = [])),
)]
pub async fn get_health_history(db: Db, query: web::Query<HistoryQuery>) -> HttpResponse {
    let limit = query.limit.unwrap_or(30).min(90);
    match db.0.get_health_reports(limit).await {
        Ok(reports) => HttpResponse::Ok().json(reports),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== GET /api/health/fixes/recent ====================

#[utoipa::path(
    get,
    path = "/api/health/fixes/recent",
    tag = "health",
    params(
        ("limit" = Option<i32>, Query, description = "Maximum number of fix log entries"),
    ),
    responses(
        (status = 200, description = "Recent fix log entries", body = Vec<HealthFixLog>),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearer_auth" = [])),
)]
pub async fn get_recent_fixes(db: Db, query: web::Query<HistoryQuery>) -> HttpResponse {
    let limit = query.limit.unwrap_or(20).min(100);
    match audit::get_recent_fixes(&db.0, limit).await {
        Ok(fixes) => HttpResponse::Ok().json(fixes),
        Err(e) => crate::error::error_response(e),
    }
}


// ==================== POST /api/health/check/{check_name} ====================

#[utoipa::path(
    post,
    path = "/api/health/check/{check_name}",
    tag = "health",
    params(
        ("check_name" = String, Path, description = "Health check name to run in isolation"),
    ),
    responses(
        (status = 200, description = "Check result", body = HealthCheckResult),
        (status = 400, description = "Unknown check name"),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearer_auth" = [])),
)]
pub async fn compute_single_check(
    db: Db,
    path: web::Path<String>,
) -> HttpResponse {
    let check_name = path.into_inner();
    match health::compute_single_check(&db.0, &check_name).await {
        Ok((_name, result)) => HttpResponse::Ok().json(result),
        Err(e) => crate::error::error_response(e),
    }
}