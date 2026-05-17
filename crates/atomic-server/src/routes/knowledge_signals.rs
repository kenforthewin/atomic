//! Knowledge-quality signal routes.

use crate::db_extractor::Db;
use crate::error::ok_or_error;
use actix_web::{web, HttpResponse};
use serde::Deserialize;
use utoipa::IntoParams;

#[derive(Deserialize, IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
pub struct KnowledgeSignalsQuery {
    /// Optional provider id, e.g. `wiki_candidate`
    pub provider_id: Option<String>,
    /// Include dismissed signals in the response.
    pub include_dismissed: Option<bool>,
    /// Include currently snoozed signals in the response.
    pub include_snoozed: Option<bool>,
    /// Max signals to return.
    pub limit: Option<i32>,
}

#[utoipa::path(
    get,
    path = "/api/knowledge-signals",
    params(KnowledgeSignalsQuery),
    responses((status = 200, description = "Knowledge-quality signals", body = Vec<atomic_core::KnowledgeSignal>)),
    tag = "knowledge-signals"
)]
pub async fn list_knowledge_signals(
    db: Db,
    query: web::Query<KnowledgeSignalsQuery>,
) -> HttpResponse {
    let q = query.into_inner();
    let filter = atomic_core::KnowledgeSignalFilter {
        provider_id: q.provider_id,
        include_dismissed: q.include_dismissed.unwrap_or(false),
        include_snoozed: q.include_snoozed.unwrap_or(false),
        limit: q.limit,
    };
    ok_or_error(db.0.list_knowledge_signals(filter).await)
}

#[utoipa::path(
    put,
    path = "/api/knowledge-signals/providers/{provider_id}",
    params(("provider_id" = String, Path, description = "Signal provider id")),
    request_body = atomic_core::KnowledgeSignalProviderConfig,
    responses((status = 200, description = "Provider config updated", body = atomic_core::KnowledgeSignalProviderConfig)),
    tag = "knowledge-signals"
)]
pub async fn set_knowledge_signal_provider_config(
    db: Db,
    path: web::Path<String>,
    body: web::Json<atomic_core::KnowledgeSignalProviderConfig>,
) -> HttpResponse {
    ok_or_error(
        db.0.set_knowledge_signal_provider_config(&path.into_inner(), body.into_inner())
            .await,
    )
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SnoozeSignalBody {
    pub until: String,
}

#[utoipa::path(
    post,
    path = "/api/knowledge-signals/{signal_key}/dismiss",
    params(("signal_key" = String, Path, description = "Stable signal key")),
    responses((status = 200, description = "Signal dismissed")),
    tag = "knowledge-signals"
)]
pub async fn dismiss_knowledge_signal(db: Db, path: web::Path<String>) -> HttpResponse {
    ok_or_error(db.0.dismiss_knowledge_signal(&path.into_inner()).await)
}

#[utoipa::path(
    post,
    path = "/api/knowledge-signals/{signal_key}/snooze",
    params(("signal_key" = String, Path, description = "Stable signal key")),
    request_body = SnoozeSignalBody,
    responses((status = 200, description = "Signal snoozed")),
    tag = "knowledge-signals"
)]
pub async fn snooze_knowledge_signal(
    db: Db,
    path: web::Path<String>,
    body: web::Json<SnoozeSignalBody>,
) -> HttpResponse {
    ok_or_error(
        db.0.snooze_knowledge_signal(&path.into_inner(), &body.until)
            .await,
    )
}

#[utoipa::path(
    post,
    path = "/api/knowledge-signals/{signal_key}/restore",
    params(("signal_key" = String, Path, description = "Stable signal key")),
    responses((status = 200, description = "Signal restored")),
    tag = "knowledge-signals"
)]
pub async fn restore_knowledge_signal(db: Db, path: web::Path<String>) -> HttpResponse {
    ok_or_error(db.0.restore_knowledge_signal(&path.into_inner()).await)
}
