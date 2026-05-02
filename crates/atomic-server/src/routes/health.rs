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

        // === Tag health: merge_tags (similar-name pair — item_id = "a_id__b_id", winner = into_tag_id) ===
        ("tag_health", "merge_tags") => {
            let winner_id = match req.into_tag_id.as_deref() {
                Some(p) if !p.trim().is_empty() => p.trim().to_string(),
                _ => {
                    return Err(AtomicCoreError::Validation(
                        "into_tag_id is required for merge_tags".into(),
                    ))
                }
            };
            let parts: Vec<&str> = item_id.splitn(2, "__").collect();
            if parts.len() != 2 {
                return Err(AtomicCoreError::Validation(
                    "item_id must be 'a_id__b_id' for merge_tags".into(),
                ));
            }
            let (a_id, b_id) = (parts[0], parts[1]);
            let loser_id = if winner_id == a_id { b_id } else { a_id };
            let winner_name = match core.get_tag_by_id(&winner_id).await? {
                Some((name, _)) => name,
                None => return Err(AtomicCoreError::NotFound("winner tag not found".into())),
            };
            let loser_name = match core.get_tag_by_id(loser_id).await? {
                Some((name, _)) => name,
                None => return Err(AtomicCoreError::NotFound("loser tag not found".into())),
            };
            let merges = vec![compaction::TagMerge {
                winner_name,
                loser_name,
                reason: "similar_name_pair_merge".to_string(),
            }];
            core.apply_tag_merges(&merges).await?;
            // Also dismiss the pair so it doesn't resurface
            let _ = core
                .dismiss_health_item("tag_health", item_id, "merged", None)
                .await;
            Ok(serde_json::json!({"status": "ok"}))
        }
        // === Tag health: delete_tag (manual review — single-atom non-autotag or any tag) ===
        ("tag_health", "delete_tag") => {
            core.delete_tag(item_id, false).await?;
            audit::log_fix(
                core,
                "tag_health",
                "delete_tag",
                "low",
                None,
                Some(&[item_id.to_string()]),
                serde_json::json!([{"id": item_id}]),
                serde_json::json!({"deleted": 1}),
                None,
                None,
            )
            .await?;
            Ok(serde_json::json!({"status": "ok"}))
        }

        // === Tag health: merge_into_parent (reparent a tag) ===
        ("tag_health", "merge_into_parent") => {
            let new_parent_id = match req.into_tag_id.as_deref() {
                Some(p) if !p.trim().is_empty() => p.trim().to_string(),
                _ => {
                    return Err(AtomicCoreError::Validation(
                        "into_tag_id is required for merge_into_parent".into(),
                    ))
                }
            };
            match core.get_tag_by_id(item_id).await? {
                Some((name, _)) => {
                    core.update_tag(item_id, &name, Some(&new_parent_id)).await?;
                    Ok(serde_json::json!({"status": "ok"}))
                }
                None => Err(AtomicCoreError::NotFound("tag not found".into())),
            }
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

        // === Broken internal links: remove-link ===
        ("broken_internal_links", "remove_link") => {
            let link_raw = match req.content.as_deref() {
                Some(c) if !c.trim().is_empty() => c.to_string(),
                _ => return Err(AtomicCoreError::Validation("content (link_raw) is required for remove_link".into())),
            };
            let action = atomic_core::health::fixes::remove_broken_link(core, item_id, &link_raw).await?;
            Ok(serde_json::to_value(action).unwrap_or_default())
        }

        // === Broken internal links: relink ===
        ("broken_internal_links", "relink") => {
            let link_raw = match req.content.as_deref() {
                Some(c) if !c.trim().is_empty() => c.to_string(),
                _ => return Err(AtomicCoreError::Validation("content (link_raw) is required for relink".into())),
            };
            let target_atom_id = match req.into_tag_id.as_deref() {
                Some(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => return Err(AtomicCoreError::Validation("into_tag_id (target_atom_id) is required for relink".into())),
            };
            let action = atomic_core::health::fixes::relink_broken_link(core, item_id, &link_raw, &target_atom_id).await?;
            Ok(serde_json::to_value(action).unwrap_or_default())
        }

        // === Broken internal links: auto_resolve ===
        ("broken_internal_links", "auto_resolve") => {
            let link_raw = match req.content.as_deref() {
                Some(c) if !c.trim().is_empty() => c.to_string(),
                _ => return Err(AtomicCoreError::Validation("content (link_raw) is required for auto_resolve".into())),
            };
            let link_text = req.url.as_deref().unwrap_or("").to_string();
            let outcome = atomic_core::health::llm_fixes::auto_resolve_broken_link(
                core, item_id, &link_raw, &link_text,
            ).await?;
            Ok(serde_json::to_value(&outcome).unwrap_or_default())
        }

        // === Content overlap / duplicate: verify with LLM ===
        ("content_overlap" | "duplicate_detection", "verify_with_llm") => {
            let parts: Vec<&str> = item_id.splitn(2, "__").collect();
            let (atom_a, atom_b) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                return Err(AtomicCoreError::Validation(
                    "item_id must be 'atom_a__atom_b' for verify_with_llm".into(),
                ));
            };
            let (is_duplicate, reason) =
                atomic_core::health::llm_fixes::verify_overlap_pair(core, atom_a, atom_b).await?;
            Ok(serde_json::json!({"is_duplicate": is_duplicate, "reason": reason}))
        }

        // === Contradiction detection: verify with LLM ===
        ("contradiction_detection", "verify_with_llm") => {
            let parts: Vec<&str> = item_id.splitn(2, "__").collect();
            let (atom_a, atom_b) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                return Err(AtomicCoreError::Validation(
                    "item_id must be 'atom_a__atom_b' for verify_with_llm".into(),
                ));
            };
            let (is_real, reason) =
                atomic_core::health::llm_fixes::verify_contradiction_pair(core, atom_a, atom_b).await?;
            Ok(serde_json::json!({"is_contradiction": is_real, "reason": reason}))
        }

        // === Contradiction detection: merge with LLM ===
        ("contradiction_detection", "merge_with_llm") => {
            let parts: Vec<&str> = item_id.splitn(2, "__").collect();
            let (atom_a, atom_b) = if parts.len() == 2 {
                (parts[0], parts[1])
            } else {
                return Err(AtomicCoreError::Validation(
                    "item_id must be 'atom_a__atom_b' for merge_with_llm".into(),
                ));
            };
            match atomic_core::health::llm_fixes::merge_contradicting_pair(
                core, atom_a, atom_b, req.dry_run,
            )
            .await
            {
                Ok(Some(action)) => Ok(serde_json::to_value(action).unwrap_or_default()),
                Ok(None) => Ok(serde_json::json!({"status": "no_op"})),
                Err(e) => Err(e),
            }
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

// ==================== GET /api/health/broken-link-suggest ====================

#[derive(Deserialize)]
pub struct BrokenLinkSuggestQuery {
    pub q: String,
    #[serde(default)]
    pub limit: Option<i32>,
}

pub async fn broken_link_suggest_handler(
    db: Db,
    query: web::Query<BrokenLinkSuggestQuery>,
) -> HttpResponse {
    let limit = query.limit.unwrap_or(5).min(20).max(1);
    match db.0.suggest_atoms_for_broken_link(&query.q, limit).await {
        Ok(rows) => {
            let suggestions: Vec<serde_json::Value> = rows.into_iter().map(|(atom_id, title, source_url, score)| {
                serde_json::json!({
                    "atom_id": atom_id,
                    "title": title,
                    "source_url": source_url,
                    "score": score,
                })
            }).collect();
            HttpResponse::Ok().json(serde_json::json!({ "suggestions": suggestions }))
        }
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct AutoResolveAllQuery {
    #[serde(default)]
    pub max: Option<u32>,
}

pub async fn broken_links_auto_resolve_all(
    db: Db,
    body: web::Json<serde_json::Value>,
) -> HttpResponse {
    let max = body
        .get("max")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(25);
    match atomic_core::health::llm_fixes::auto_resolve_all_broken_links(&db.0, max).await {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== POST /api/health/verify/{check} ====================

#[derive(Debug, serde::Deserialize)]
pub struct VerifyBatchBody {
    pub item_ids: Vec<String>,
    pub max: Option<u32>,
}

pub async fn verify_batch_handler(
    db: Db,
    path: web::Path<String>,
    body: web::Json<VerifyBatchBody>,
) -> HttpResponse {
    let check = path.into_inner();
    let body = body.into_inner();
    let limit = body.max.unwrap_or(50) as usize;
    let ids: Vec<String> = body.item_ids.into_iter().take(limit).collect();
    let core = &db.0;

    let mut checked = 0u32;
    let mut kept = 0u32;
    let mut dismissed_ids: Vec<String> = Vec::new();

    for item_id in &ids {
        let parts: Vec<&str> = item_id.splitn(2, "__").collect();
        if parts.len() != 2 {
            continue;
        }
        let (atom_a, atom_b) = (parts[0], parts[1]);
        checked += 1;
        let result = match check.as_str() {
            "content_overlap" | "duplicate_detection" => {
                atomic_core::health::llm_fixes::verify_overlap_pair(core, atom_a, atom_b)
                    .await
                    .map(|(is_dup, _)| is_dup)
            }
            "contradiction_detection" => {
                atomic_core::health::llm_fixes::verify_contradiction_pair(core, atom_a, atom_b)
                    .await
                    .map(|(is_real, _)| is_real)
            }
            _ => break,
        };
        match result {
            Ok(true) => kept += 1,
            Ok(false) => dismissed_ids.push(item_id.clone()),
            Err(_) => {}
        }
    }

    HttpResponse::Ok().json(serde_json::json!({
        "checked": checked,
        "kept": kept,
        "dismissed_ids": dismissed_ids,
    }))
}

// ==================== Tag proposal handlers ====================

/// POST /api/health/tag-proposal — generate a new LLM proposal.
pub async fn create_tag_proposal(db: Db) -> HttpResponse {
    match atomic_core::health::llm_fixes::propose_tag_restructure(&db.0).await {
        Ok(proposal) => HttpResponse::Ok().json(proposal),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(serde::Deserialize)]
pub struct ApplyTagProposalRequest {
    #[serde(default)]
    pub accepted_indices: Vec<usize>,
}

/// POST /api/health/tag-proposal/{proposal_id}/apply
pub async fn apply_tag_proposal(
    db: Db,
    path: web::Path<String>,
    body: web::Json<ApplyTagProposalRequest>,
) -> HttpResponse {
    let proposal_id = path.into_inner();
    match atomic_core::health::llm_fixes::apply_tag_proposal(
        &db.0,
        &proposal_id,
        &body.accepted_indices,
    )
    .await
    {
        Ok(actions) => HttpResponse::Ok().json(actions),
        Err(e) => crate::error::error_response(e),
    }
}

/// GET /api/health/tag-proposal/latest
pub async fn get_latest_tag_proposal(db: Db) -> HttpResponse {
    match db.0.get_latest_tag_proposal().await {
        Ok(Some(proposal)) => HttpResponse::Ok().json(proposal),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({"error": "no pending proposal"})),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== Health Config ====================

/// GET /api/health/config
pub async fn get_health_config(db: Db) -> HttpResponse {
    match db.0.get_health_config().await {
        Ok(config) => HttpResponse::Ok().json(config),
        Err(e) => crate::error::error_response(e),
    }
}

/// PUT /api/health/config
pub async fn set_health_config(
    db: Db,
    body: web::Json<atomic_core::health::HealthConfig>,
) -> HttpResponse {
    match db.0.set_health_config(&body.into_inner()).await {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== Wiki exclusion ====================

#[derive(serde::Deserialize)]
pub struct SetWikiExcludedTagsBody { pub tag_ids: Vec<String> }

/// GET /api/wiki/excluded-tags
pub async fn get_wiki_excluded_tags(db: Db) -> HttpResponse {
    match db.0.get_wiki_excluded_tag_ids().await {
        Ok(ids) => HttpResponse::Ok().json(serde_json::json!({ "tag_ids": ids })),
        Err(e) => crate::error::error_response(e),
    }
}

/// PUT /api/wiki/excluded-tags
pub async fn set_wiki_excluded_tags(
    db: Db,
    body: web::Json<SetWikiExcludedTagsBody>,
) -> HttpResponse {
    match db.0.set_wiki_excluded_tag_ids(&body.into_inner().tag_ids).await {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== Custom health checks ====================

/// GET /api/health/custom-checks
pub async fn get_custom_health_checks(db: Db) -> HttpResponse {
    match db.0.get_custom_health_checks().await {
        Ok(checks) => HttpResponse::Ok().json(serde_json::json!({ "checks": checks })),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(serde::Deserialize)]
pub struct SetCustomHealthChecksBody {
    pub checks: Vec<atomic_core::health::custom::CustomCheck>,
}

/// PUT /api/health/custom-checks
pub async fn set_custom_health_checks(
    db: Db,
    body: web::Json<SetCustomHealthChecksBody>,
) -> HttpResponse {
    match db.0.set_custom_health_checks(&body.into_inner().checks).await {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(serde::Deserialize)]
pub struct PreviewCustomHealthCheckBody {
    pub rule: atomic_core::health::custom::CustomRule,
}

/// POST /api/health/custom-checks/preview
///
/// Dry-runs an unsaved rule against the current DB so the UI can show
/// "this would flag N atoms" while the user is tuning parameters.
pub async fn preview_custom_health_check(
    db: Db,
    body: web::Json<PreviewCustomHealthCheckBody>,
) -> HttpResponse {
    match db.0.preview_custom_health_check(&body.into_inner().rule).await {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(e) => crate::error::error_response(e),
    }
}