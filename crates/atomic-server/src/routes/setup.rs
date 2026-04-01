//! Instance setup endpoint — allows claiming an unconfigured instance

use crate::state::AppState;
use actix_web::{web, HttpResponse};
use serde::Deserialize;

/// GET /api/setup/status — Check if the instance needs initial setup or unlock
pub async fn setup_status(state: web::Data<AppState>) -> HttpResponse {
    if !state.manager.is_initialized() {
        // Check if databases exist on disk but are locked (encrypted, no passphrase yet)
        let data_dir = state.manager.data_dir();
        let registry_exists = data_dir.join("registry.db").exists();

        return HttpResponse::Ok().json(serde_json::json!({
            "needs_setup": !registry_exists,
            "needs_unlock": registry_exists,
        }));
    }

    let core = match state.manager.active_core() {
        Ok(c) => c,
        Err(e) => return crate::error::error_response(e),
    };
    match web::block(move || core.list_api_tokens()).await {
        Ok(Ok(tokens)) => {
            let active = tokens.iter().filter(|t| !t.is_revoked).count();
            HttpResponse::Ok().json(serde_json::json!({
                "needs_setup": active == 0,
                "needs_unlock": false,
            }))
        }
        Ok(Err(e)) => crate::error::error_response(e),
        Err(e) => HttpResponse::InternalServerError().json(serde_json::json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
pub struct ClaimBody {
    pub name: Option<String>,
    /// Optional passphrase for SQLCipher database encryption.
    /// If set, all databases will be encrypted at rest.
    /// Cannot be changed after initial setup.
    pub passphrase: Option<String>,
}

/// POST /api/setup/claim — Create the first API token (only works when no tokens exist).
/// If the manager is not yet initialized (deferred mode), this also creates the databases
/// with the optional passphrase for encryption.
pub async fn claim_instance(
    state: web::Data<AppState>,
    body: web::Json<ClaimBody>,
) -> HttpResponse {
    let body = body.into_inner();
    let name = body.name.unwrap_or_else(|| "default".to_string());

    // Initialize databases if in deferred mode
    if !state.manager.is_initialized() {
        if let Err(e) = state.manager.initialize(body.passphrase) {
            return crate::error::error_response(e);
        }
        run_post_init_tasks(&state);
    }

    let core = match state.manager.active_core() {
        Ok(c) => c,
        Err(e) => return crate::error::error_response(e),
    };

    match web::block(move || {
        // Check that no active tokens exist
        let tokens = core.list_api_tokens()?;
        let active = tokens.iter().filter(|t| !t.is_revoked).count();
        if active > 0 {
            return Ok(None);
        }
        let (info, raw) = core.create_api_token(&name)?;
        Ok(Some((info, raw)))
    })
    .await
    {
        Ok(Ok(Some((info, raw_token)))) => HttpResponse::Created().json(serde_json::json!({
            "id": info.id,
            "name": info.name,
            "token": raw_token,
            "prefix": info.token_prefix,
            "created_at": info.created_at,
        })),
        Ok(Ok(None)) => HttpResponse::Conflict().json(serde_json::json!({
            "error": "Instance already claimed"
        })),
        Ok(Err(e)) => crate::error::error_response(e),
        Err(e) => HttpResponse::InternalServerError().json(serde_json::json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
pub struct UnlockBody {
    pub passphrase: String,
}

/// POST /api/setup/unlock — Unlock an encrypted database with a passphrase.
/// Used on restart when the server detects encrypted databases but has no passphrase.
pub async fn unlock_instance(
    state: web::Data<AppState>,
    body: web::Json<UnlockBody>,
) -> HttpResponse {
    if state.manager.is_initialized() {
        return HttpResponse::Ok().json(serde_json::json!({
            "status": "already_unlocked"
        }));
    }

    if let Err(e) = state.manager.initialize(Some(body.into_inner().passphrase)) {
        return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": format!("Failed to unlock: {e}"),
            "hint": "Check your passphrase and try again"
        }));
    }

    // Run the same startup recovery tasks that run on boot for non-encrypted instances
    run_post_init_tasks(&state);

    HttpResponse::Ok().json(serde_json::json!({
        "status": "unlocked"
    }))
}

/// Runs startup recovery tasks after the manager has been initialized.
/// Called both from `claim_instance` (fresh setup) and `unlock_instance` (encrypted restart).
fn run_post_init_tasks(state: &web::Data<AppState>) {
    // Migrate legacy token if present
    if let Ok(core) = state.manager.active_core() {
        match core.migrate_legacy_token() {
            Ok(true) => tracing::info!("migrated legacy auth token to new token system"),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "failed to migrate legacy token"),
        }
    }

    // Reset stuck atoms and process pending work for all databases
    let (databases, _) = match state.manager.list_databases() {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(error = %e, "failed to list databases for post-init recovery");
            return;
        }
    };

    for db_info in &databases {
        let db_core = match state.manager.get_core(&db_info.id) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(db = %db_info.name, error = %e, "failed to load database");
                continue;
            }
        };
        let on_event = crate::event_bridge::embedding_event_callback(state.event_tx.clone());

        match db_core.reset_stuck_processing() {
            Ok(count) if count > 0 => tracing::info!(db = %db_info.name, count = count, "reset atoms stuck in processing state"),
            Ok(_) => {}
            Err(e) => tracing::warn!(db = %db_info.name, error = %e, "failed to reset stuck processing"),
        }

        match db_core.process_pending_embeddings(on_event.clone()) {
            Ok(count) if count > 0 => tracing::info!(db = %db_info.name, count = count, "processing pending embeddings in background"),
            Ok(_) => {}
            Err(e) => tracing::warn!(db = %db_info.name, error = %e, "failed to start pending embeddings"),
        }

        match db_core.process_pending_tagging(on_event) {
            Ok(count) if count > 0 => tracing::info!(db = %db_info.name, count = count, "processing pending tagging operations in background"),
            Ok(_) => {}
            Err(e) => tracing::warn!(db = %db_info.name, error = %e, "failed to start pending tagging"),
        }
    }
}
