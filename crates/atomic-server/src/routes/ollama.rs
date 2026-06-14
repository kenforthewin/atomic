//! Ollama and provider routes

use crate::db_extractor::Db;
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Deserialize, Serialize, ToSchema)]
pub struct TestOllamaBody {
    /// Ollama server host URL
    pub host: String,
}

#[utoipa::path(post, path = "/api/ollama/test", request_body = TestOllamaBody, responses((status = 200, description = "Connection test result")), tag = "providers")]
pub async fn test_ollama(body: web::Json<TestOllamaBody>) -> HttpResponse {
    match atomic_core::providers::models::test_ollama_connection(&body.host).await {
        Ok(true) => HttpResponse::Ok().json(serde_json::json!({"success": true})),
        Ok(false) => HttpResponse::Ok().json(serde_json::json!({"success": false})),
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({"error": e})),
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OllamaHostQuery {
    /// Ollama host URL (default: http://127.0.0.1:11434)
    pub host: Option<String>,
}

#[utoipa::path(get, path = "/api/ollama/models", params(OllamaHostQuery), responses((status = 200, description = "All Ollama models")), tag = "providers")]
pub async fn get_ollama_models(query: web::Query<OllamaHostQuery>) -> HttpResponse {
    let host = query.host.as_deref().unwrap_or("http://127.0.0.1:11434");
    match atomic_core::providers::models::fetch_ollama_models(host).await {
        Ok(models) => HttpResponse::Ok().json(models),
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({"error": e})),
    }
}

#[utoipa::path(get, path = "/api/ollama/embedding-models", params(OllamaHostQuery), responses((status = 200, description = "Ollama embedding models")), tag = "providers")]
pub async fn get_ollama_embedding_models(query: web::Query<OllamaHostQuery>) -> HttpResponse {
    let host = query.host.as_deref().unwrap_or("http://127.0.0.1:11434");
    match atomic_core::providers::models::get_ollama_embedding_models(host).await {
        Ok(models) => HttpResponse::Ok().json(models),
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({"error": e})),
    }
}

#[utoipa::path(get, path = "/api/ollama/llm-models", params(OllamaHostQuery), responses((status = 200, description = "Ollama LLM models")), tag = "providers")]
pub async fn get_ollama_llm_models(query: web::Query<OllamaHostQuery>) -> HttpResponse {
    let host = query.host.as_deref().unwrap_or("http://127.0.0.1:11434");
    match atomic_core::providers::models::get_ollama_llm_models(host).await {
        Ok(models) => HttpResponse::Ok().json(models),
        Err(e) => HttpResponse::BadGateway().json(serde_json::json!({"error": e})),
    }
}

#[utoipa::path(get, path = "/api/provider/verify", responses((status = 200, description = "Whether an AI provider is configured")), tag = "providers")]
pub async fn verify_provider_configured(db: Db) -> HttpResponse {
    // Delegate to the core check rather than re-deriving the answer from a raw
    // `get_settings()` read: `verify_provider_configured` resolves provider
    // config through `settings_for_ai`, which overlays any explicit provider
    // configuration injected at open time (the path a composing process uses
    // when it manages credentials outside the settings tables). Reading the
    // settings tables directly here would miss that overlay and report an
    // explicitly-configured provider as unconfigured.
    match db.0.verify_provider_configured().await {
        Ok(configured) => HttpResponse::Ok().json(serde_json::json!({ "configured": configured })),
        Err(e) => crate::error::error_response(e),
    }
}
