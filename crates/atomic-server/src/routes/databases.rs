//! Database management routes
//!
//! These handlers operate on the [`DatabaseManager`] itself rather than on
//! one resolved database, so they resolve it via
//! [`request_manager`](crate::db_extractor::request_manager) — honoring a
//! composing layer's [`RequestDatabaseManager`](crate::db_extractor::RequestDatabaseManager)
//! override exactly like the `Db` extractor does for single-database routes.

use crate::db_extractor::request_manager;
use crate::error::ApiErrorResponse;
use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[utoipa::path(get, path = "/api/databases", responses((status = 200, description = "List of databases with active ID")), tag = "databases")]
pub async fn list_databases(req: HttpRequest, state: web::Data<AppState>) -> HttpResponse {
    let manager = request_manager(&req, &state);
    match manager.list_databases().await {
        Ok((databases, active_id)) => HttpResponse::Ok().json(serde_json::json!({
            "databases": databases,
            "active_id": active_id,
        })),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateDatabaseBody {
    /// Name for the new database
    pub name: String,
}

#[utoipa::path(post, path = "/api/databases", request_body = CreateDatabaseBody, responses((status = 201, description = "Database created", body = atomic_core::DatabaseInfo)), tag = "databases")]
pub async fn create_database(
    req: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreateDatabaseBody>,
) -> HttpResponse {
    let name = body.into_inner().name;
    let manager = request_manager(&req, &state);
    match manager.create_database(&name).await {
        Ok(info) => HttpResponse::Created().json(info),
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct RenameDatabaseBody {
    /// New name for the database
    pub name: String,
}

#[utoipa::path(put, path = "/api/databases/{id}", params(("id" = String, Path, description = "Database ID")), request_body = RenameDatabaseBody, responses((status = 200, description = "Database renamed"), (status = 404, description = "Database not found", body = ApiErrorResponse)), tag = "databases")]
pub async fn rename_database(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<RenameDatabaseBody>,
) -> HttpResponse {
    let id = path.into_inner();
    let name = body.into_inner().name;
    let manager = request_manager(&req, &state);
    match manager.rename_database(&id, &name).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"renamed": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(delete, path = "/api/databases/{id}", params(("id" = String, Path, description = "Database ID")), responses((status = 200, description = "Database deleted"), (status = 400, description = "Cannot delete default database", body = ApiErrorResponse)), tag = "databases")]
pub async fn delete_database(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    let manager = request_manager(&req, &state);
    match manager.delete_database(&id).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"deleted": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(put, path = "/api/databases/{id}/activate", params(("id" = String, Path, description = "Database ID")), responses((status = 200, description = "Database activated")), tag = "databases")]
pub async fn activate_database(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    let manager = request_manager(&req, &state);
    match manager.set_active(&id).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"activated": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(put, path = "/api/databases/{id}/default", params(("id" = String, Path, description = "Database ID")), responses((status = 200, description = "Default database changed")), tag = "databases")]
pub async fn set_default_database(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    let manager = request_manager(&req, &state);
    match manager.set_default_database(&id).await {
        Ok(()) => HttpResponse::Ok().json(serde_json::json!({"set_default": true})),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(get, path = "/api/databases/{id}/stats", params(("id" = String, Path, description = "Database ID")), responses((status = 200, description = "Database statistics")), tag = "databases")]
pub async fn database_stats(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let id = path.into_inner();
    let manager = request_manager(&req, &state);
    match manager.get_core(&id).await {
        Ok(core) => {
            let atom_count = core.count_atoms().await.unwrap_or(0);
            HttpResponse::Ok().json(serde_json::json!({
                "atom_count": atom_count,
            }))
        }
        Err(e) => crate::error::error_response(e),
    }
}
