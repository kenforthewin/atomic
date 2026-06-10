//! Wiki article routes

use crate::db_extractor::Db;
use crate::error::{ok_or_error, ApiErrorResponse};
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[utoipa::path(get, path = "/api/wiki", responses((status = 200, description = "All wiki articles", body = Vec<atomic_core::WikiArticleSummary>)), tag = "wiki")]
pub async fn get_all_wiki_articles(db: Db) -> HttpResponse {
    ok_or_error(db.0.get_all_wiki_articles().await)
}

#[utoipa::path(get, path = "/api/wiki/{tag_id}", params(("tag_id" = String, Path, description = "Tag ID")), responses((status = 200, description = "Wiki article with citations (or `null` if no article exists for this tag)", body = atomic_core::WikiArticleWithCitations)), tag = "wiki")]
pub async fn get_wiki(db: Db, path: web::Path<String>) -> HttpResponse {
    let tag_id = path.into_inner();
    // Returns `200 null` (not 404) when no article exists. The frontend
    // store relies on this — an error response would set an error state
    // instead of treating "no wiki" as a normal empty result.
    ok_or_error(db.0.get_wiki(&tag_id).await)
}

#[utoipa::path(get, path = "/api/wiki/{tag_id}/status", params(("tag_id" = String, Path, description = "Tag ID")), responses((status = 200, description = "Wiki article status", body = atomic_core::WikiArticleStatus)), tag = "wiki")]
pub async fn get_wiki_status(db: Db, path: web::Path<String>) -> HttpResponse {
    let tag_id = path.into_inner();
    ok_or_error(db.0.get_wiki_status(&tag_id).await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct GenerateWikiBody {
    /// Tag name for the wiki article
    pub tag_name: String,
}

#[utoipa::path(post, path = "/api/wiki/{tag_id}/generate", params(("tag_id" = String, Path, description = "Tag ID")), request_body = GenerateWikiBody, responses((status = 200, description = "Generated wiki article", body = atomic_core::WikiArticleWithCitations), (status = 400, description = "Error", body = ApiErrorResponse)), tag = "wiki")]
pub async fn generate_wiki(
    db: Db,
    path: web::Path<String>,
    body: web::Json<GenerateWikiBody>,
) -> HttpResponse {
    let tag_id = path.into_inner();
    match db.0.generate_wiki(&tag_id, &body.tag_name).await {
        Ok(article) => HttpResponse::Ok().json(article),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/wiki/{tag_id}/update", params(("tag_id" = String, Path, description = "Tag ID")), request_body = GenerateWikiBody, responses((status = 200, description = "Updated wiki article", body = atomic_core::WikiArticleWithCitations), (status = 400, description = "Error", body = ApiErrorResponse)), tag = "wiki")]
pub async fn update_wiki(
    db: Db,
    path: web::Path<String>,
    body: web::Json<GenerateWikiBody>,
) -> HttpResponse {
    let tag_id = path.into_inner();
    match db.0.update_wiki(&tag_id, &body.tag_name).await {
        Ok(article) => HttpResponse::Ok().json(article),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(delete, path = "/api/wiki/{tag_id}", params(("tag_id" = String, Path, description = "Tag ID")), responses((status = 200, description = "Wiki deleted")), tag = "wiki")]
pub async fn delete_wiki(db: Db, path: web::Path<String>) -> HttpResponse {
    let tag_id = path.into_inner();
    ok_or_error(db.0.delete_wiki(&tag_id).await)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RelatedTagsQuery {
    /// Max results (default: 10)
    pub limit: Option<usize>,
}

#[utoipa::path(get, path = "/api/wiki/{tag_id}/related", params(("tag_id" = String, Path, description = "Tag ID"), RelatedTagsQuery), responses((status = 200, description = "Related tags", body = Vec<atomic_core::RelatedTag>)), tag = "wiki")]
pub async fn get_related_tags(
    db: Db,
    path: web::Path<String>,
    query: web::Query<RelatedTagsQuery>,
) -> HttpResponse {
    let tag_id = path.into_inner();
    let limit = query.limit.unwrap_or(10);
    ok_or_error(db.0.get_related_tags(&tag_id, limit).await)
}

#[utoipa::path(get, path = "/api/wiki/{tag_id}/links", params(("tag_id" = String, Path, description = "Tag ID")), responses((status = 200, description = "Wiki cross-reference links", body = Vec<atomic_core::WikiLink>)), tag = "wiki")]
pub async fn get_wiki_links(db: Db, path: web::Path<String>) -> HttpResponse {
    let tag_id = path.into_inner();
    ok_or_error(db.0.get_wiki_links(&tag_id).await)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SuggestionsQuery {
    /// Max suggestions (default: 10)
    pub limit: Option<i32>,
}

#[utoipa::path(get, path = "/api/wiki/suggestions", params(SuggestionsQuery), responses((status = 200, description = "Suggested wiki articles", body = Vec<atomic_core::SuggestedArticle>)), tag = "wiki")]
pub async fn get_wiki_suggestions(db: Db, query: web::Query<SuggestionsQuery>) -> HttpResponse {
    let limit = query.limit.unwrap_or(10);
    ok_or_error(db.0.get_suggested_wiki_articles(limit).await)
}

#[utoipa::path(get, path = "/api/wiki/{tag_id}/versions", params(("tag_id" = String, Path, description = "Tag ID")), responses((status = 200, description = "Version history", body = Vec<atomic_core::WikiVersionSummary>)), tag = "wiki")]
pub async fn list_wiki_versions(db: Db, path: web::Path<String>) -> HttpResponse {
    let tag_id = path.into_inner();
    ok_or_error(db.0.list_wiki_versions(&tag_id).await)
}

#[utoipa::path(get, path = "/api/wiki/versions/{version_id}", params(("version_id" = String, Path, description = "Version ID")), responses((status = 200, description = "Wiki article version", body = atomic_core::WikiArticleVersion)), tag = "wiki")]
pub async fn get_wiki_version(db: Db, path: web::Path<String>) -> HttpResponse {
    let version_id = path.into_inner();
    ok_or_error(db.0.get_wiki_version(&version_id).await)
}

#[utoipa::path(post, path = "/api/wiki/recompute-tag-embeddings", responses((status = 200, description = "Recomputed tag embeddings")), tag = "wiki")]
pub async fn recompute_all_tag_embeddings(db: Db) -> HttpResponse {
    match db.0.recompute_all_tag_embeddings().await {
        Ok(count) => HttpResponse::Ok().json(serde_json::json!({"count": count})),
        Err(e) => crate::error::error_response(e),
    }
}

// ==================== Wiki Proposals (human-in-the-loop update review) ====================

#[utoipa::path(
    post,
    path = "/api/wiki/{tag_id}/propose",
    params(("tag_id" = String, Path, description = "Tag ID")),
    request_body = GenerateWikiBody,
    responses(
        (status = 200, description = "Proposal created or no update needed", body = atomic_core::WikiProposal),
        (status = 400, description = "Error", body = ApiErrorResponse)
    ),
    tag = "wiki"
)]
pub async fn propose_wiki(
    db: Db,
    path: web::Path<String>,
    body: web::Json<GenerateWikiBody>,
) -> HttpResponse {
    let tag_id = path.into_inner();
    match db.0.propose_wiki_update(&tag_id, &body.tag_name).await {
        Ok(Some(proposal)) => HttpResponse::Ok().json(proposal),
        Ok(None) => HttpResponse::Ok().json(serde_json::json!({
            "status": "no_update_needed"
        })),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(
    get,
    path = "/api/wiki/{tag_id}/proposal",
    params(("tag_id" = String, Path, description = "Tag ID")),
    responses(
        (status = 200, description = "Pending wiki proposal", body = atomic_core::WikiProposal),
        (status = 404, description = "No pending proposal", body = ApiErrorResponse)
    ),
    tag = "wiki"
)]
pub async fn get_wiki_proposal(db: Db, path: web::Path<String>) -> HttpResponse {
    let tag_id = path.into_inner();
    match db.0.get_wiki_proposal(&tag_id).await {
        Ok(Some(proposal)) => HttpResponse::Ok().json(proposal),
        Ok(None) => HttpResponse::NotFound()
            .json(serde_json::json!({"error": "No pending proposal for this tag"})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(
    post,
    path = "/api/wiki/{tag_id}/proposal/accept",
    params(("tag_id" = String, Path, description = "Tag ID")),
    responses(
        (status = 200, description = "Proposal accepted and promoted to live article", body = atomic_core::WikiArticleWithCitations),
        (status = 400, description = "Error", body = ApiErrorResponse)
    ),
    tag = "wiki"
)]
pub async fn accept_wiki_proposal(db: Db, path: web::Path<String>) -> HttpResponse {
    let tag_id = path.into_inner();
    match db.0.accept_wiki_proposal(&tag_id).await {
        Ok(article) => HttpResponse::Ok().json(article),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(
    post,
    path = "/api/wiki/{tag_id}/proposal/dismiss",
    params(("tag_id" = String, Path, description = "Tag ID")),
    responses(
        (status = 204, description = "Proposal dismissed"),
        (status = 400, description = "Error", body = ApiErrorResponse)
    ),
    tag = "wiki"
)]
pub async fn dismiss_wiki_proposal(db: Db, path: web::Path<String>) -> HttpResponse {
    let tag_id = path.into_inner();
    match db.0.dismiss_wiki_proposal(&tag_id).await {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(e) => crate::error::error_response(e),
    }
}
