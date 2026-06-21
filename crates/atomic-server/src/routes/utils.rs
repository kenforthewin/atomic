//! Utility routes

use crate::db_extractor::Db;
use actix_web::HttpResponse;

#[utoipa::path(get, path = "/api/utils/sqlite-vec", responses((status = 200, description = "sqlite-vec version")), tag = "utils")]
pub async fn check_sqlite_vec(db: Db) -> HttpResponse {
    match db.0.check_sqlite_vec().await {
        Ok(version) => HttpResponse::Ok().json(serde_json::json!({"version": version})),
        Err(e) => HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": format!("sqlite-vec not loaded: {}", e)})),
    }
}

#[utoipa::path(post, path = "/api/utils/compact-tags", responses((status = 200, description = "Tag compaction results")), tag = "utils")]
pub async fn compact_tags(db: Db) -> HttpResponse {
    // All orchestration (provider/model resolution via the settings_for_ai
    // overlay, capabilities, merge application) lives in the core facade so the
    // explicit-provider-config path is honored — a raw get_settings() read here
    // would bypass it and let a cloud tenant's settings drive the provider.
    match db.0.compact_tags().await {
        Ok(result) => HttpResponse::Ok().json(serde_json::json!({
            "tags_merged": result.tags_merged,
            "atoms_retagged": result.atoms_retagged
        })),
        Err(e) => HttpResponse::InternalServerError()
            .json(serde_json::json!({"error": e.to_string()})),
    }
}
