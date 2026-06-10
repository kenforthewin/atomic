//! Settings routes
//!
//! Settings come in two flavors (see `atomic_core::settings`): workspace-only
//! keys live exclusively in `registry.db`; overridable keys default in the
//! registry but each database can override them in its own settings table.
//! `GET /api/settings` returns the resolved values *with their source* so the
//! frontend can render override affordances. `DELETE /api/settings/{key}`
//! clears an override on the active DB. `GET /api/settings/{key}/overrides`
//! lists which databases currently override the key.
//!
//! There is intentionally no endpoint to write a workspace default
//! out-of-band: changing the registry value for an embedding-space key would
//! silently shift every inheriting DB's resolved setting without recreating
//! their vector indexes or re-embedding their atoms, leaving them in a
//! broken state. If a "change for all DBs" feature ever ships it will need
//! its own dedicated route that walks every inheriting DB and queues
//! reembeds.

use crate::db_extractor::{request_manager, Db};
use crate::error::{ok_or_error, ApiErrorResponse};
use crate::event_channel::EventChannel;
use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[utoipa::path(get, path = "/api/settings", responses((status = 200, description = "All resolved settings tagged with source")), tag = "settings")]
pub async fn get_settings(db: Db) -> HttpResponse {
    ok_or_error(db.0.get_settings_with_source().await)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SetSettingBody {
    /// Setting value
    pub value: String,
}

#[utoipa::path(put, path = "/api/settings/{key}", params(("key" = String, Path, description = "Setting key")), request_body = SetSettingBody, responses((status = 200, description = "Setting updated"), (status = 400, description = "Invalid setting", body = ApiErrorResponse)), tag = "settings")]
pub async fn set_setting(
    events: EventChannel,
    db: Db,
    path: web::Path<String>,
    body: web::Json<SetSettingBody>,
) -> HttpResponse {
    let key = path.into_inner();
    let value = body.into_inner().value;

    // Handle embedding-space settings via set_setting_with_reembed (avoids deadlock)
    if atomic_core::settings::is_embedding_space_key(&key) {
        let on_event = crate::event_bridge::embedding_event_callback(events.0.clone());
        // `set_setting_with_reembed` writes through the resolver's routing:
        // workspace-only → registry, overridable + N≤1 → registry, overridable
        // + N>1 → per-DB override for the active database. It then re-embeds
        // **the active database only**, which is correct in every routing
        // case here:
        //   * N=1: there are no other DBs to fan out to.
        //   * N>1: the write created/updated a per-DB override. Other DBs
        //     keep inheriting the workspace default — their resolved value
        //     didn't change, so re-embedding them would corrupt their vec
        //     indexes (especially for dimension changes). The previous
        //     fan-out across all databases assumed the registry-global write
        //     model and is gone deliberately.
        // A future "change for all DBs" operation that updates the workspace
        // default cascade-style would need its own dedicated route that
        // walks every DB without an override and re-embeds them.
        let result = db.0.set_setting_with_reembed(&key, &value, on_event).await;
        ok_or_error(result)
    } else {
        ok_or_error(db.0.set_setting(&key, &value).await)
    }
}

#[utoipa::path(delete, path = "/api/settings/{key}", params(("key" = String, Path, description = "Setting key")), responses((status = 200, description = "Override cleared"), (status = 400, description = "Key is workspace-only", body = ApiErrorResponse)), tag = "settings")]
pub async fn clear_setting_override(
    events: EventChannel,
    db: Db,
    path: web::Path<String>,
) -> HttpResponse {
    let key = path.into_inner();
    if atomic_core::settings::is_embedding_space_key(&key) {
        let on_event = crate::event_bridge::embedding_event_callback(events.0.clone());
        ok_or_error(db.0.clear_override_with_reembed(&key, on_event).await)
    } else {
        ok_or_error(db.0.clear_override(&key).await)
    }
}

#[derive(Serialize, ToSchema)]
pub struct OverrideEntry {
    pub db_id: String,
    pub db_name: String,
    pub value: String,
}

#[utoipa::path(get, path = "/api/settings/{key}/overrides", params(("key" = String, Path, description = "Setting key")), responses((status = 200, description = "List of databases overriding the key", body = Vec<OverrideEntry>)), tag = "settings")]
pub async fn list_setting_overrides(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let key = path.into_inner();

    // Workspace-only keys can't be overridden — short-circuit so the frontend
    // can render "no overrides" without spinning up cores for every DB.
    if atomic_core::settings::is_workspace_only(&key) {
        return HttpResponse::Ok().json(Vec::<OverrideEntry>::new());
    }

    // Cross-database fan-out: iterate the manager governing this request
    // (honoring a composing layer's RequestDatabaseManager override), not
    // AppState's unconditionally.
    let manager = request_manager(&req, &state);
    let (databases, _active) = match manager.list_databases().await {
        Ok(v) => v,
        Err(e) => return crate::error::error_response(e),
    };

    let mut overrides: Vec<OverrideEntry> = Vec::new();
    for info in databases {
        let core = match manager.get_core(&info.id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(db_id = %info.id, "Failed to load core for override lookup: {}", e);
                continue;
            }
        };
        match core.get_setting_override(&key).await {
            Ok(Some(value)) => overrides.push(OverrideEntry {
                db_id: info.id,
                db_name: info.name,
                value,
            }),
            Ok(None) => {}
            Err(e) => tracing::error!(
                db_id = %info.id,
                key = %key,
                "Failed to read override: {}",
                e
            ),
        }
    }

    HttpResponse::Ok().json(overrides)
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct TestOpenRouterBody {
    /// OpenRouter API key to test
    pub api_key: String,
}

#[utoipa::path(post, path = "/api/settings/test-openrouter", request_body = TestOpenRouterBody, responses((status = 200, description = "Connection successful"), (status = 400, description = "API error", body = ApiErrorResponse)), tag = "settings")]
pub async fn test_openrouter_connection(body: web::Json<TestOpenRouterBody>) -> HttpResponse {
    // Validate the key against the authenticated `/key` endpoint rather than a
    // real chat completion. This avoids spending credits and exercising a
    // specific model just to confirm the key is valid.
    // https://openrouter.ai/docs/api/api-reference/api-keys/get-current-key
    let client = reqwest::Client::new();
    let response = client
        .get("https://openrouter.ai/api/v1/key")
        .header("Authorization", format!("Bearer {}", body.api_key))
        .send()
        .await;

    match response {
        Ok(resp) if resp.status().is_success() => {
            HttpResponse::Ok().json(serde_json::json!({"success": true}))
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("API error ({}): {}", status, body)
            }))
        }
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({
            "error": format!("Network error: {}", e)
        })),
    }
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct TestOpenAICompatBody {
    /// Base URL of the OpenAI-compatible API
    pub base_url: String,
    /// Optional API key for authentication
    pub api_key: Option<String>,
}

#[utoipa::path(post, path = "/api/settings/test-openai-compat", request_body = TestOpenAICompatBody, responses((status = 200, description = "Connection successful"), (status = 400, description = "API error", body = ApiErrorResponse)), tag = "settings")]
pub async fn test_openai_compat_connection(body: web::Json<TestOpenAICompatBody>) -> HttpResponse {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Normalize URL the same way OpenAICompatProvider does
    let trimmed = body.base_url.trim_end_matches('/');
    let base_url = if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{}/v1", trimmed)
    };

    let mut req = client.get(format!("{}/models", base_url));

    if let Some(ref api_key) = body.api_key {
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", api_key));
        }
    }

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            HttpResponse::Ok().json(serde_json::json!({"success": true}))
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            HttpResponse::BadRequest().json(serde_json::json!({
                "error": format!("API error ({}): {}", status, body)
            }))
        }
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({
            "error": format!("Connection failed: {}", e)
        })),
    }
}

#[utoipa::path(get, path = "/api/settings/embedding-models", responses((status = 200, description = "Curated OpenRouter embedding models with dimensions")), tag = "settings")]
pub async fn get_openrouter_embedding_models() -> HttpResponse {
    let models = atomic_core::providers::openrouter::models::get_embedding_models();
    HttpResponse::Ok().json(models)
}

#[utoipa::path(get, path = "/api/settings/models", responses((status = 200, description = "Available LLM models")), tag = "settings")]
pub async fn get_available_llm_models(db: Db) -> HttpResponse {
    use atomic_core::providers::models::fetch_and_return_capabilities;

    let core = &db.0;
    let (cached, is_stale) = match core.get_cached_capabilities().await {
        Ok(Some(cache)) => {
            let stale = cache.is_stale();
            (Some(cache), stale)
        }
        Ok(None) => (None, true),
        Err(_) => (None, true),
    };

    if let Some(ref cache) = cached {
        if !is_stale {
            return HttpResponse::Ok().json(cache.get_models_with_structured_outputs());
        }
    }

    let client = reqwest::Client::new();
    match fetch_and_return_capabilities(&client).await {
        Ok(fresh_cache) => {
            let _ = core.save_capabilities_cache(&fresh_cache).await;
            HttpResponse::Ok().json(fresh_cache.get_models_with_structured_outputs())
        }
        Err(e) => {
            if let Some(cache) = cached {
                HttpResponse::Ok().json(cache.get_models_with_structured_outputs())
            } else {
                HttpResponse::BadGateway()
                    .json(serde_json::json!({"error": format!("Failed to fetch models: {}", e)}))
            }
        }
    }
}
