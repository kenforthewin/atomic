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
    match apply_manual_fix_impl(&db, &check, &item_id, body.into_inner()).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => crate::error::error_response(e),
    }
}

async fn apply_manual_fix_impl(
    db: &Db,
    check: &str,
    item_id: &str,
    req: ManualFixRequest,
) -> Result<serde_json::Value, atomic_core::error::AtomicCoreError> {
    use atomic_core::error::AtomicCoreError;
    let core = &db.0;

    match (check, req.action.as_str()) {
        // === Existing: content-overlap LLM merge ===
        ("duplicate_detection" | "content_overlap", "merge_with_llm") => {
            let parts: Vec<&str> = item_id.splitn(2, "__").collect();
            let (atom_a, atom_b) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                let legacy: Vec<&str> = item_id.splitn(2, '_').collect();
                if legacy.len() != 2 {
                    return Err(AtomicCoreError::Validation(
                        "item_id must be 'atom_a__atom_b' for pair actions".into(),
                    ));
                }
                (legacy[0], legacy[1])
            };
            match atomic_core::health::llm_fixes::merge_duplicate_pair(
                core, atom_a, atom_b, req.dry_run,
            )
            .await
            {
                Ok(Some(action)) => Ok(serde_json::to_value(action).unwrap_or_default()),
                Ok(None) => Ok(serde_json::json!({"status": "no_op"})),
                Err(e) => Err(e),
            }
        }

        // === Content overlap: keep_a / keep_b (archive the loser) ===
        ("content_overlap" | "duplicate_detection", action @ ("keep_a" | "keep_b")) => {
            let parts: Vec<&str> = item_id.splitn(2, "__").collect();
            if parts.len() != 2 {
                return Err(AtomicCoreError::Validation(
                    "item_id must be 'atom_a__atom_b'".into(),
                ));
            }
            let (a, b) = (parts[0], parts[1]);
            let loser = if action == "keep_a" { b } else { a };
            core.delete_atom(loser).await?;
            let key = pair_key(a, b);
            let _ = core
                .dismiss_health_item("content_overlap", &key, "resolved_other", None)
                .await;
            Ok(serde_json::json!({"status": "ok"}))
        }

        // === Dismiss actions (all reviewable checks) ===
        (check_name, action @ ("dismiss" | "mark_intentional" | "ignore_pair" | "defer")) => {
            let reason = match action {
                "mark_intentional" => "intentional_no_source",
                "ignore_pair" => "ignored_pair",
                "defer" => "deferred",
                _ => "resolved_other",
            };
            let expires_at = if action == "defer" {
                let exp = chrono::Utc::now() + chrono::Duration::days(7);
                Some(exp.to_rfc3339())
            } else {
                None
            };
            core
                .dismiss_health_item(check_name, item_id, reason, expires_at.as_deref())
                .await?;
            Ok(serde_json::json!({"status": "dismissed"}))
        }

        // === Content quality: add source URL ===
        ("content_quality", "add_source") => {
            let url = match req.url.as_deref() {
                Some(u) if !u.trim().is_empty() => u.trim().to_string(),
                _ => {
                    return Err(AtomicCoreError::Validation(
                        "url is required for add_source".into(),
                    ))
                }
            };
            match core.get_atom(item_id).await? {
                Some(atom) => {
                    let tag_ids: Vec<String> = atom.tags.iter().map(|t| t.id.clone()).collect();
                    let upd = atomic_core::UpdateAtomRequest {
                        content: atom.atom.content.clone(),
                        source_url: Some(url),
                        published_at: atom.atom.published_at.clone(),
                        tag_ids: Some(tag_ids),
                    };
                    core.update_atom(item_id, upd, |_| {}).await?;
                    Ok(serde_json::json!({"status": "ok"}))
                }
                None => Err(AtomicCoreError::NotFound("atom not found".into())),
            }
        }

        // === Tag health: move_under (reparent rootless tag) ===
        ("tag_health", "move_under") => {
            let parent_id = match req.parent_id.as_deref() {
                Some(p) if !p.trim().is_empty() => p.trim().to_string(),
                _ => {
                    return Err(AtomicCoreError::Validation(
                        "parent_id is required for move_under".into(),
                    ))
                }
            };
            match core.get_tag_by_id(item_id).await? {
                Some((name, _)) => {
                    core.update_tag(item_id, &name, Some(&parent_id)).await?;
                    Ok(serde_json::json!({"status": "ok"}))
                }
                None => Err(AtomicCoreError::NotFound("tag not found".into())),
            }
        }

        // === Tag health: merge (winner becomes into_tag_id, loser is item_id) ===
        ("tag_health", "merge") => {
            let winner_id = match req.into_tag_id.as_deref() {
                Some(p) if !p.trim().is_empty() => p.trim().to_string(),
                _ => {
                    return Err(AtomicCoreError::Validation(
                        "into_tag_id is required for merge".into(),
                    ))
                }
            };
            let winner_name = match core.get_tag_by_id(&winner_id).await? {
                Some((name, _)) => name,
                None => return Err(AtomicCoreError::NotFound("target tag not found".into())),
            };
            let loser_name = match core.get_tag_by_id(item_id).await? {
                Some((name, _)) => name,
                None => return Err(AtomicCoreError::NotFound("source tag not found".into())),
            };
            let merges = vec![compaction::TagMerge {
                winner_name,
                loser_name,
                reason: "manual_review_merge".to_string(),
            }];
            core.apply_tag_merges(&merges).await?;
            Ok(serde_json::json!({"status": "ok"}))
        }

        // === Boilerplate: re-embed ===
        ("boilerplate_pollution", "reembed") => {
            core.retry_embedding(item_id, |_| {}).await?;
            Ok(serde_json::json!({"status": "ok"}))
        }

        // === Content overlap: merge_with_edited_content ===
        ("content_overlap" | "duplicate_detection", "merge_with_edited_content") => {
            let parts: Vec<&str> = item_id.splitn(2, "__").collect();
            if parts.len() != 2 {
                return Err(AtomicCoreError::Validation(
                    "item_id must be 'atom_a__atom_b'".into(),
                ));
            }
            let winner = match req.winner_atom_id.as_deref() {
                Some(w) if !w.is_empty() => w.to_string(),
                _ => return Err(AtomicCoreError::Validation("winner_atom_id required".into())),
            };
            let loser = match req.loser_atom_id.as_deref() {
                Some(l) if !l.is_empty() => l.to_string(),
                _ => return Err(AtomicCoreError::Validation("loser_atom_id required".into())),
            };
            let content = match req.content.as_deref() {
                Some(c) if !c.trim().is_empty() => c.to_string(),
                _ => return Err(AtomicCoreError::Validation("content required".into())),
            };
            let action = atomic_core::health::llm_fixes::apply_edited_merge(core, &winner, &loser, &content).await?;
            let key = atomic_core::health::pair_key(parts[0], parts[1]);
            let _ = core.dismiss_health_item("content_overlap", &key, "resolved_other", None).await;
            Ok(serde_json::to_value(action).unwrap_or_default())
        }

        _ => Err(AtomicCoreError::Validation(format!(
            "unsupported check '{}' or action '{}'",
            check, req.action
        ))),
    }
}

// ==================== POST /api/health/fix/batch ====================

#[derive(Debug, Deserialize)]
pub struct BatchFixItem {
    pub check: String,
    pub item_id: String,
    pub action: String,
    #[serde(default)] pub url: Option<String>,
    #[serde(default)] pub parent_id: Option<String>,
    #[serde(default)] pub into_tag_id: Option<String>,
    #[serde(default)] pub content: Option<String>,
    #[serde(default)] pub winner_atom_id: Option<String>,
    #[serde(default)] pub loser_atom_id: Option<String>,
    #[serde(default)] pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub struct BatchFixRequest {
    pub items: Vec<BatchFixItem>,
}

pub async fn apply_manual_fix_batch(
    db: Db,
    body: web::Json<BatchFixRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    let mut results = Vec::with_capacity(req.items.len());
    for item in req.items {
        let single = ManualFixRequest {
            action: item.action.clone(),
            url: item.url,
            parent_id: item.parent_id,
            into_tag_id: item.into_tag_id,
            content: item.content,
            winner_atom_id: item.winner_atom_id,
            loser_atom_id: item.loser_atom_id,
            dry_run: item.dry_run,
        };
        match apply_manual_fix_impl(&db, &item.check, &item.item_id, single).await {
            Ok(_) => results.push(serde_json::json!({
                "check": item.check,
                "item_id": item.item_id,
                "ok": true
            })),
            Err(e) => results.push(serde_json::json!({
                "check": item.check,
                "item_id": item.item_id,
                "ok": false,
                "error": e.to_string()
            })),
        }
    }
    HttpResponse::Ok().json(serde_json::json!({"results": results}))
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

// ==================== POST /api/health/contradiction-summary/{atom_a}/{atom_b} ====================

pub async fn contradiction_summary_handler(
    db: Db,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (a, b) = path.into_inner();
    match atomic_core::health::llm_fixes::contradiction_summary(&db.0, &a, &b).await {
        Ok(summary) => HttpResponse::Ok().json(serde_json::json!({"summary": summary})),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== POST /api/health/strip-boilerplate/{atom_id} ====================

#[derive(Debug, Deserialize, Default)]
pub struct StripBoilerplateQuery {
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn strip_boilerplate_handler(
    db: Db,
    path: web::Path<String>,
    query: web::Query<StripBoilerplateQuery>,
) -> HttpResponse {
    let atom_id = path.into_inner();
    match atomic_core::health::llm_fixes::strip_boilerplate_atom(&db.0, &atom_id, query.dry_run).await {
        Ok((content, action)) => HttpResponse::Ok().json(serde_json::json!({
            "content": content,
            "action": action,
            "dry_run": query.dry_run
        })),
        Err(e) => crate::error::error_response(e),
    }
}