//! URL ingestion routes

use crate::db_extractor::Db;
use crate::event_bridge::{embedding_event_callback, ingestion_event_callback};
use crate::event_channel::EventChannel;
use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, Serialize, ToSchema)]
pub struct IngestUrlRequest {
    /// URL to ingest
    pub url: String,
    /// Tag IDs to assign to ingested atom
    #[serde(default)]
    pub tag_ids: Vec<String>,
    /// Hint for the article title
    pub title_hint: Option<String>,
    /// Publication date override
    pub published_at: Option<String>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct IngestUrlsRequest {
    /// List of URLs to ingest
    pub urls: Vec<IngestUrlRequest>,
}

#[utoipa::path(post, path = "/api/ingest/url", request_body = IngestUrlRequest, responses((status = 200, description = "Ingested atom")), tag = "ingestion")]
pub async fn ingest_url(
    events: EventChannel,
    db: Db,
    body: web::Json<IngestUrlRequest>,
) -> HttpResponse {
    let request = atomic_core::IngestionRequest {
        url: body.url.clone(),
        tag_ids: body.tag_ids.clone(),
        title_hint: body.title_hint.clone(),
        published_at: body.published_at.clone(),
    };

    let on_ingest = ingestion_event_callback(events.0.clone());
    let on_embed = embedding_event_callback(events.0.clone());

    match db.0.ingest_url(request, on_ingest, on_embed).await {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(e) => crate::error::error_response(e),
    }
}

#[utoipa::path(post, path = "/api/ingest/urls", request_body = IngestUrlsRequest, responses((status = 200, description = "Batch ingestion results")), tag = "ingestion")]
pub async fn ingest_urls(
    events: EventChannel,
    db: Db,
    body: web::Json<IngestUrlsRequest>,
) -> HttpResponse {
    let requests: Vec<atomic_core::IngestionRequest> = body
        .urls
        .iter()
        .map(|r| atomic_core::IngestionRequest {
            url: r.url.clone(),
            tag_ids: r.tag_ids.clone(),
            title_hint: r.title_hint.clone(),
            published_at: r.published_at.clone(),
        })
        .collect();

    let on_ingest = ingestion_event_callback(events.0.clone());
    let on_embed = embedding_event_callback(events.0.clone());

    let results = db.0.ingest_urls(requests, on_ingest, on_embed).await;

    let (successes, failures): (Vec<_>, Vec<_>) = results
        .into_iter()
        .enumerate()
        .partition(|(_, r)| r.is_ok());

    let ingested: Vec<_> = successes.into_iter().map(|(_, r)| r.unwrap()).collect();
    let errors: Vec<_> = failures
        .into_iter()
        .map(|(i, r)| {
            serde_json::json!({
                "index": i,
                "error": r.unwrap_err().to_string()
            })
        })
        .collect();

    HttpResponse::Ok().json(serde_json::json!({
        "ingested": ingested,
        "errors": errors,
    }))
}
