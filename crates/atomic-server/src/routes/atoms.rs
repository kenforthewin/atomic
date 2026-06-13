//! Atom and Tag CRUD routes

use crate::db_extractor::Db;
use crate::error::{ok_or_error, ApiErrorResponse};
use crate::event_bridge::embedding_event_callback;
use crate::event_channel::EventChannel;
use crate::state::ServerEvent;
use actix_web::{web, HttpResponse};
use atomic_core::{
    AtomLink, AtomWithTags, BulkCreateResult, PaginatedAtoms, PaginatedTagChildren, SourceInfo,
    Tag, TagWithCount,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

// ==================== Atoms ====================

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetAtomsQuery {
    /// Filter by tag ID
    pub tag_id: Option<String>,
    /// Max results to return (default: 50)
    pub limit: Option<i32>,
    /// Offset for pagination
    pub offset: Option<i32>,
    /// Cursor for keyset pagination (updated_at value)
    pub cursor: Option<String>,
    /// Cursor tiebreaker (atom id)
    pub cursor_id: Option<String>,
    /// Source filter: "all", "manual", or "external"
    pub source: Option<String>,
    /// Filter by specific source domain (e.g. "nytimes.com")
    pub source_value: Option<String>,
    /// Sort field: "updated", "created", "published", or "title"
    pub sort_by: Option<String>,
    /// Sort direction: "desc" or "asc"
    pub sort_order: Option<String>,
    /// Optional `kind` filter for external consumers (MCP exports, future
    /// sync clients) that want to exclude report-generated finding atoms.
    /// CSV of `captured` / `report`. Missing/empty = no filter (returns all
    /// kinds, preserving the UI's read shape — the React app does not pass
    /// this parameter). Invalid value → 400.
    pub kinds: Option<String>,
}

/// Parse the `?kinds=` CSV into a `KindFilter`. `None` and empty strings
/// resolve to `KindFilter::All` (backwards compatible). Any unknown token
/// returns a 400-ready error. Whitespace around individual tokens is
/// trimmed so `?kinds=captured, report` works.
fn parse_kinds(raw: Option<&str>) -> Result<atomic_core::models::KindFilter, HttpResponse> {
    use atomic_core::models::{AtomKind, KindFilter};
    use std::str::FromStr;
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(KindFilter::All);
    };
    let mut kinds = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match AtomKind::from_str(token) {
            Ok(k) if !kinds.contains(&k) => kinds.push(k),
            Ok(_) => {}
            Err(_) => {
                return Err(HttpResponse::BadRequest().json(ApiErrorResponse {
                    error: format!(
                        "invalid kinds value '{token}': expected 'captured' or 'report'"
                    ),
                }));
            }
        }
    }
    if kinds.is_empty() {
        return Ok(KindFilter::All);
    }
    Ok(KindFilter::Only(kinds))
}

#[utoipa::path(
    get,
    path = "/api/atoms",
    params(GetAtomsQuery),
    responses(
        (status = 200, description = "Paginated list of atoms", body = PaginatedAtoms),
        (status = 500, description = "Internal error", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn get_atoms(db: Db, query: web::Query<GetAtomsQuery>) -> HttpResponse {
    let source_filter = match query.source.as_deref() {
        Some("manual") => atomic_core::SourceFilter::Manual,
        Some("external") => atomic_core::SourceFilter::External,
        _ => atomic_core::SourceFilter::All,
    };
    let sort_by = match query.sort_by.as_deref() {
        Some("created") => atomic_core::SortField::Created,
        Some("published") => atomic_core::SortField::Published,
        Some("title") => atomic_core::SortField::Title,
        _ => atomic_core::SortField::Updated,
    };
    let sort_order = match query.sort_order.as_deref() {
        Some("asc") => atomic_core::SortOrder::Asc,
        _ => atomic_core::SortOrder::Desc,
    };
    let params = atomic_core::ListAtomsParams {
        tag_id: query.tag_id.clone(),
        limit: query.limit.unwrap_or(50),
        offset: query.offset.unwrap_or(0),
        cursor: query.cursor.clone(),
        cursor_id: query.cursor_id.clone(),
        source_filter,
        source_value: query.source_value.clone(),
        sort_by,
        sort_order,
    };
    // The default (missing `kinds`) stays `KindFilter::All` so the React
    // app — which does not pass this parameter — keeps showing findings
    // alongside captured atoms. External consumers that don't want
    // findings in their export must opt in with `?kinds=captured`. This
    // is deliberately backwards compatible; restricting the default would
    // hide finding atoms from the UI that already renders them.
    let kinds = match parse_kinds(query.kinds.as_deref()) {
        Ok(k) => k,
        Err(resp) => return resp,
    };
    ok_or_error(db.0.list_atoms(&params, &kinds).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::models::{AtomKind, KindFilter};

    #[test]
    fn kinds_query_filter_parses_captured() {
        let f = parse_kinds(Some("captured")).expect("parses");
        assert!(matches!(f, KindFilter::Only(ref ks) if ks == &vec![AtomKind::Captured]));
    }

    #[test]
    fn kinds_query_filter_parses_csv_both() {
        let f = parse_kinds(Some("captured,report")).expect("parses");
        match f {
            KindFilter::Only(ks) => {
                assert_eq!(ks, vec![AtomKind::Captured, AtomKind::Report]);
            }
            _ => panic!("expected Only"),
        }
    }

    #[test]
    fn kinds_query_filter_dedupes_duplicates() {
        // Defensive: `?kinds=captured,captured` collapses, not duplicates.
        let f = parse_kinds(Some("captured,captured")).expect("parses");
        assert!(matches!(f, KindFilter::Only(ref ks) if ks == &vec![AtomKind::Captured]));
    }

    #[test]
    fn kinds_query_invalid_returns_400() {
        let err = parse_kinds(Some("banana")).expect_err("rejected");
        assert_eq!(err.status(), actix_web::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn kinds_query_missing_returns_all_kinds() {
        assert!(matches!(
            parse_kinds(None).expect("none ok"),
            KindFilter::All
        ));
        // Empty / whitespace-only matches None semantics.
        assert!(matches!(
            parse_kinds(Some("")).expect("empty ok"),
            KindFilter::All
        ));
        assert!(matches!(
            parse_kinds(Some("   ")).expect("ws ok"),
            KindFilter::All
        ));
    }

    #[test]
    fn kinds_query_tolerates_whitespace_around_tokens() {
        let f = parse_kinds(Some(" captured , report ")).expect("parses");
        match f {
            KindFilter::Only(ks) => {
                assert_eq!(ks, vec![AtomKind::Captured, AtomKind::Report]);
            }
            _ => panic!("expected Only"),
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/atoms/sources",
    responses(
        (status = 200, description = "List of sources with counts", body = Vec<SourceInfo>),
    ),
    tag = "atoms",
)]
pub async fn get_source_list(db: Db) -> HttpResponse {
    ok_or_error(db.0.get_source_list().await)
}

#[utoipa::path(
    get,
    path = "/api/atoms/{id}",
    params(
        ("id" = String, Path, description = "Atom ID"),
    ),
    responses(
        (status = 200, description = "Atom with tags", body = AtomWithTags),
        (status = 404, description = "Atom not found", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn get_atom(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    match db.0.get_atom(&id).await {
        Ok(Some(atom)) => HttpResponse::Ok().json(atom),
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({"error": "Atom not found"})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(
    get,
    path = "/api/atoms/{id}/links",
    params(
        ("id" = String, Path, description = "Source atom ID"),
    ),
    responses(
        (status = 200, description = "Materialized atom links emitted by this atom", body = Vec<AtomLink>),
        (status = 500, description = "Internal error", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn get_atom_links(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    ok_or_error(db.0.get_atom_links(&id).await)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LinkSuggestionsQuery {
    /// Title query. Empty returns recent atoms.
    pub q: Option<String>,
    /// Max results to return (default: 10, max: 50)
    pub limit: Option<i32>,
}

#[utoipa::path(
    get,
    path = "/api/atoms/link-suggestions",
    params(LinkSuggestionsQuery),
    responses(
        (status = 200, description = "Recent atoms or title matches for editor link completion", body = Vec<atomic_core::AtomLinkSuggestion>),
        (status = 500, description = "Internal error", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn get_atom_link_suggestions(
    db: Db,
    query: web::Query<LinkSuggestionsQuery>,
) -> HttpResponse {
    ok_or_error(
        db.0.suggest_atom_links(
            query.q.as_deref().unwrap_or_default(),
            query.limit.unwrap_or(10),
        )
        .await,
    )
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetAtomBySourceUrlQuery {
    /// The source URL to look up
    pub url: String,
}

#[utoipa::path(
    get,
    path = "/api/atoms/by-source-url",
    params(GetAtomBySourceUrlQuery),
    responses(
        (status = 200, description = "Atom found", body = AtomWithTags),
        (status = 404, description = "No atom with this source URL", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn get_atom_by_source_url(
    db: Db,
    query: web::Query<GetAtomBySourceUrlQuery>,
) -> HttpResponse {
    let url = query.into_inner().url;
    match db.0.get_atom_by_source_url(&url).await {
        Ok(Some(atom)) => HttpResponse::Ok().json(atom),
        Ok(None) => HttpResponse::NotFound()
            .json(serde_json::json!({"error": "No atom found with this source URL"})),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateAtomRequest {
    /// Markdown content of the atom
    pub content: String,
    /// Optional source URL
    pub source_url: Option<String>,
    /// Optional publication date (ISO 8601)
    pub published_at: Option<String>,
    /// Tag IDs to assign
    #[serde(default)]
    pub tag_ids: Vec<String>,
    /// When true, skip creation if an atom with the same source_url already exists
    #[serde(default)]
    pub skip_if_source_exists: bool,
}

#[utoipa::path(
    post,
    path = "/api/atoms",
    request_body = CreateAtomRequest,
    responses(
        (status = 201, description = "Created atom", body = AtomWithTags),
        (status = 400, description = "Validation error", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn create_atom(
    events: EventChannel,
    db: Db,
    body: web::Json<CreateAtomRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    let on_event = embedding_event_callback(events.0.clone());
    let event_tx = events.0;
    match db
        .0
        .create_atom(
            atomic_core::CreateAtomRequest {
                content: req.content,
                source_url: req.source_url,
                published_at: req.published_at,
                tag_ids: req.tag_ids,
                skip_if_source_exists: req.skip_if_source_exists,
            },
            on_event,
        )
        .await
    {
        Ok(Some(atom)) => {
            let _ = event_tx.send(ServerEvent::AtomCreated { atom: atom.clone() });
            HttpResponse::Created().json(atom)
        }
        Ok(None) => HttpResponse::Ok().json(serde_json::json!({"skipped": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(
    post,
    path = "/api/atoms/bulk",
    request_body = Vec<CreateAtomRequest>,
    responses(
        (status = 201, description = "Bulk create result", body = BulkCreateResult),
        (status = 400, description = "Validation error", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn bulk_create_atoms(
    events: EventChannel,
    db: Db,
    body: web::Json<Vec<CreateAtomRequest>>,
) -> HttpResponse {
    let requests: Vec<atomic_core::CreateAtomRequest> = body
        .into_inner()
        .into_iter()
        .map(|r| atomic_core::CreateAtomRequest {
            content: r.content,
            source_url: r.source_url,
            published_at: r.published_at,
            tag_ids: r.tag_ids,
            skip_if_source_exists: r.skip_if_source_exists,
        })
        .collect();
    let on_event = embedding_event_callback(events.0.clone());
    let event_tx = events.0;
    match db.0.create_atoms_bulk(requests, on_event).await {
        Ok(result) => {
            for atom in &result.atoms {
                let _ = event_tx.send(ServerEvent::AtomCreated { atom: atom.clone() });
            }
            HttpResponse::Created().json(result)
        }
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct UpdateAtomRequest {
    /// Updated markdown content
    pub content: String,
    /// Updated source URL
    pub source_url: Option<String>,
    /// Updated publication date
    pub published_at: Option<String>,
    /// Updated tag IDs (if provided, replaces all tags)
    pub tag_ids: Option<Vec<String>>,
}

#[utoipa::path(
    put,
    path = "/api/atoms/{id}",
    params(
        ("id" = String, Path, description = "Atom ID"),
    ),
    request_body = UpdateAtomRequest,
    responses(
        (status = 200, description = "Updated atom", body = AtomWithTags),
        (status = 404, description = "Atom not found", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn update_atom(
    events: EventChannel,
    db: Db,
    path: web::Path<String>,
    body: web::Json<UpdateAtomRequest>,
) -> HttpResponse {
    let id = path.into_inner();
    let req = body.into_inner();
    let on_event = embedding_event_callback(events.0.clone());
    let event_tx = events.0;
    match db
        .0
        .update_atom(
            &id,
            atomic_core::UpdateAtomRequest {
                content: req.content,
                source_url: req.source_url,
                published_at: req.published_at,
                tag_ids: req.tag_ids,
            },
            on_event,
        )
        .await
    {
        Ok(atom) => {
            let _ = event_tx.send(ServerEvent::AtomUpdated { atom: atom.clone() });
            HttpResponse::Ok().json(atom)
        }
        Err(e) => crate::error::error_response(e),
    }
}

/// Update atom content/metadata without triggering embedding or tagging pipeline.
/// Used by auto-save during inline editing.
#[utoipa::path(
    put,
    path = "/api/atoms/{id}/content",
    params(
        ("id" = String, Path, description = "Atom ID"),
    ),
    request_body = UpdateAtomRequest,
    responses(
        (status = 200, description = "Updated atom (no pipeline triggered)", body = AtomWithTags),
        (status = 404, description = "Atom not found", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn update_atom_content_only(
    db: Db,
    path: web::Path<String>,
    body: web::Json<UpdateAtomRequest>,
) -> HttpResponse {
    let id = path.into_inner();
    let req = body.into_inner();
    ok_or_error(
        db.0.update_atom_content_only(
            &id,
            atomic_core::UpdateAtomRequest {
                content: req.content,
                source_url: req.source_url,
                published_at: req.published_at,
                tag_ids: req.tag_ids,
            },
        )
        .await,
    )
}

#[utoipa::path(
    post,
    path = "/api/atoms/{id}/process",
    params(
        ("id" = String, Path, description = "Atom ID"),
    ),
    responses(
        (status = 200, description = "Queued atom pipeline processing"),
        (status = 404, description = "Atom not found", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn process_atom_pipeline(
    events: EventChannel,
    db: Db,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    tracing::info!(atom_id = %id, "Received explicit atom pipeline request");
    let on_event = embedding_event_callback(events.0.clone());
    ok_or_error(db.0.process_atom_pipeline(&id, on_event).await)
}

#[utoipa::path(
    delete,
    path = "/api/atoms/{id}",
    params(
        ("id" = String, Path, description = "Atom ID"),
    ),
    responses(
        (status = 200, description = "Atom deleted"),
        (status = 404, description = "Atom not found", body = ApiErrorResponse),
    ),
    tag = "atoms",
)]
pub async fn delete_atom(db: Db, path: web::Path<String>) -> HttpResponse {
    let id = path.into_inner();
    ok_or_error(db.0.delete_atom(&id).await)
}

// ==================== Tags ====================

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetTagsQuery {
    /// Minimum atom count to include (default: 2)
    pub min_count: Option<i32>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetTagChildrenQuery {
    /// Minimum atom count to include (default: 0)
    pub min_count: Option<i32>,
    /// Max results (default: 100)
    pub limit: Option<i32>,
    /// Offset for pagination
    pub offset: Option<i32>,
}

#[utoipa::path(
    get,
    path = "/api/tags",
    params(GetTagsQuery),
    responses(
        (status = 200, description = "Hierarchical tag tree", body = Vec<TagWithCount>),
    ),
    tag = "tags",
)]
pub async fn get_tags(db: Db, query: web::Query<GetTagsQuery>) -> HttpResponse {
    let min_count = query.min_count.unwrap_or(2);
    ok_or_error(db.0.get_all_tags_filtered(min_count).await)
}

#[utoipa::path(
    get,
    path = "/api/tags/{id}/children",
    params(
        ("id" = String, Path, description = "Parent tag ID"),
        GetTagChildrenQuery,
    ),
    responses(
        (status = 200, description = "Paginated tag children", body = PaginatedTagChildren),
    ),
    tag = "tags",
)]
pub async fn get_tag_children(
    db: Db,
    path: web::Path<String>,
    query: web::Query<GetTagChildrenQuery>,
) -> HttpResponse {
    let parent_id = path.into_inner();
    let min_count = query.min_count.unwrap_or(0);
    let limit = query.limit.unwrap_or(100);
    let offset = query.offset.unwrap_or(0);
    ok_or_error(
        db.0.get_tag_children(&parent_id, min_count, limit, offset)
            .await,
    )
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateTagRequest {
    /// Tag name
    pub name: String,
    /// Parent tag ID for hierarchy
    pub parent_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/tags",
    request_body = CreateTagRequest,
    responses(
        (status = 201, description = "Created tag", body = Tag),
        (status = 400, description = "Validation error", body = ApiErrorResponse),
    ),
    tag = "tags",
)]
pub async fn create_tag(db: Db, body: web::Json<CreateTagRequest>) -> HttpResponse {
    let req = body.into_inner();
    match db.0.create_tag(&req.name, req.parent_id.as_deref()).await {
        Ok(tag) => HttpResponse::Created().json(tag),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct UpdateTagRequest {
    /// Updated tag name
    pub name: String,
    /// Updated parent tag ID
    pub parent_id: Option<String>,
}

#[utoipa::path(
    put,
    path = "/api/tags/{id}",
    params(
        ("id" = String, Path, description = "Tag ID"),
    ),
    request_body = UpdateTagRequest,
    responses(
        (status = 200, description = "Updated tag", body = Tag),
        (status = 404, description = "Tag not found", body = ApiErrorResponse),
    ),
    tag = "tags",
)]
pub async fn update_tag(
    db: Db,
    path: web::Path<String>,
    body: web::Json<UpdateTagRequest>,
) -> HttpResponse {
    let id = path.into_inner();
    let req = body.into_inner();
    ok_or_error(
        db.0.update_tag(&id, &req.name, req.parent_id.as_deref())
            .await,
    )
}

#[utoipa::path(
    delete,
    path = "/api/tags/{id}",
    params(
        ("id" = String, Path, description = "Tag ID"),
        ("recursive" = Option<bool>, Query, description = "Delete child tags recursively"),
    ),
    responses(
        (status = 200, description = "Tag deleted"),
        (status = 404, description = "Tag not found", body = ApiErrorResponse),
    ),
    tag = "tags",
)]
pub async fn delete_tag(
    db: Db,
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> HttpResponse {
    let id = path.into_inner();
    let recursive = query.get("recursive").map(|v| v == "true").unwrap_or(false);
    ok_or_error(db.0.delete_tag(&id, recursive).await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SetAutotagTargetRequest {
    /// Whether the tag should be a candidate for AI auto-tagging.
    pub value: bool,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SetAutotagDescriptionRequest {
    /// Optional guidance injected next to this top-level auto-tag target.
    pub description: String,
}

#[utoipa::path(
    put,
    path = "/api/tags/{id}/autotag-target",
    params(
        ("id" = String, Path, description = "Tag ID"),
    ),
    request_body = SetAutotagTargetRequest,
    responses(
        (status = 204, description = "Flag updated"),
        (status = 404, description = "Tag not found", body = ApiErrorResponse),
    ),
    tag = "tags",
)]
pub async fn set_tag_autotag_target(
    db: Db,
    path: web::Path<String>,
    body: web::Json<SetAutotagTargetRequest>,
) -> HttpResponse {
    let id = path.into_inner();
    let value = body.into_inner().value;
    match db.0.set_tag_autotag_target(&id, value).await {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(
    put,
    path = "/api/tags/{id}/autotag-description",
    params(
        ("id" = String, Path, description = "Tag ID"),
    ),
    request_body = SetAutotagDescriptionRequest,
    responses(
        (status = 204, description = "Description updated"),
        (status = 404, description = "Top-level tag not found", body = ApiErrorResponse),
    ),
    tag = "tags",
)]
pub async fn set_tag_autotag_description(
    db: Db,
    path: web::Path<String>,
    body: web::Json<SetAutotagDescriptionRequest>,
) -> HttpResponse {
    let id = path.into_inner();
    let description = body.into_inner().description;
    match db.0.set_tag_autotag_description(&id, &description).await {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct ConfigureAutotagTargetsRequest {
    /// Names of seeded default categories to keep flagged.
    /// Any seeded default not in this list is unflagged.
    pub keep_defaults: Vec<String>,
    /// Names of new top-level tags to create with the flag set.
    /// Existing top-level tags with matching names are flagged in place.
    pub add_custom: Vec<String>,
}

#[utoipa::path(
    post,
    path = "/api/tags/configure-autotag-targets",
    request_body = ConfigureAutotagTargetsRequest,
    responses(
        (status = 200, description = "Newly created/flagged custom tags", body = Vec<Tag>),
    ),
    tag = "tags",
)]
pub async fn configure_autotag_targets(
    db: Db,
    body: web::Json<ConfigureAutotagTargetsRequest>,
) -> HttpResponse {
    let req = body.into_inner();
    ok_or_error(
        db.0.configure_autotag_targets(&req.keep_defaults, &req.add_custom)
            .await,
    )
}
