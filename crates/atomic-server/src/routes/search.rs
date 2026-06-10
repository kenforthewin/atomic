//! Search routes

use crate::db_extractor::Db;
use crate::error::{ok_or_error, ApiErrorResponse};
use actix_web::{web, HttpResponse};
use atomic_core::{SearchMode, SearchOptions, SemanticSearchResult, SimilarAtomResult};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SearchRequest {
    /// Search query text
    pub query: String,
    /// Search mode: "keyword", "semantic", or "hybrid"
    pub mode: String,
    /// Max results (default: 20)
    pub limit: Option<i32>,
    /// Minimum similarity threshold
    pub threshold: Option<f32>,
    /// Restrict results to atoms tagged with any of these tag IDs
    #[serde(default)]
    pub scope_tag_ids: Vec<String>,
    /// Restrict to atoms created/updated within the last N days
    pub since_days: Option<i32>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct GlobalSearchRequest {
    /// Search query text
    pub query: String,
    /// Per-section result cap (default: 5)
    pub section_limit: Option<i32>,
}

#[utoipa::path(
    post,
    path = "/api/search",
    request_body = SearchRequest,
    responses(
        (status = 200, description = "Search results", body = Vec<SemanticSearchResult>),
        (status = 400, description = "Invalid search mode", body = ApiErrorResponse),
    ),
    tag = "search",
)]
pub async fn search(db: Db, body: web::Json<SearchRequest>) -> HttpResponse {
    let req = body.into_inner();
    let mode = match req.mode.as_str() {
        "keyword" => SearchMode::Keyword,
        "semantic" => SearchMode::Semantic,
        "hybrid" => SearchMode::Hybrid,
        _ => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "Invalid search mode. Use 'keyword', 'semantic', or 'hybrid'."
            }));
        }
    };

    let mut options = SearchOptions::new(req.query, mode, req.limit.unwrap_or(20));
    if let Some(threshold) = req.threshold {
        options = options.with_threshold(threshold);
    }
    if !req.scope_tag_ids.is_empty() {
        options = options.with_scope(req.scope_tag_ids);
    }
    if let Some(days) = req.since_days {
        options = options.with_since_days(Some(days));
    }

    let result = db.0.search(options).await;
    ok_or_error(result)
}

#[utoipa::path(
    post,
    path = "/api/search/global",
    request_body = GlobalSearchRequest,
    responses(
        (status = 200, description = "Grouped global search results", body = atomic_core::GlobalSearchResponse),
    ),
    tag = "search",
)]
pub async fn global_search(db: Db, body: web::Json<GlobalSearchRequest>) -> HttpResponse {
    let req = body.into_inner();
    ok_or_error(
        db.0.search_global_keyword(&req.query, req.section_limit.unwrap_or(5))
            .await,
    )
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FindSimilarQuery {
    /// Max results (default: 10)
    pub limit: Option<i32>,
    /// Minimum similarity threshold (default: 0.7)
    pub threshold: Option<f32>,
}

#[utoipa::path(
    get,
    path = "/api/atoms/{id}/similar",
    params(
        ("id" = String, Path, description = "Atom ID"),
        FindSimilarQuery,
    ),
    responses(
        (status = 200, description = "Similar atoms", body = Vec<SimilarAtomResult>),
    ),
    tag = "search",
)]
pub async fn find_similar(
    db: Db,
    path: web::Path<String>,
    query: web::Query<FindSimilarQuery>,
) -> HttpResponse {
    let atom_id = path.into_inner();
    let limit = query.limit.unwrap_or(10);
    let threshold = query.threshold.unwrap_or(0.7);
    ok_or_error(db.0.find_similar(&atom_id, limit, threshold).await)
}
