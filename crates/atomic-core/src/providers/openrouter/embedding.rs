//! OpenRouter embedding implementation

use crate::providers::error::ProviderError;
use crate::providers::openrouter::OpenRouterProvider;
use crate::providers::traits::EmbeddingConfig;
use serde::{Deserialize, Serialize};

/// OpenRouter Embeddings API request
#[derive(Serialize)]
struct EmbeddingRequest {
    model: String,
    input: Vec<String>,
    /// Matryoshka output width. Omitted when `None` so providers that don't
    /// accept the parameter aren't sent it; when present we also enforce it
    /// client-side (see [`embed_batch`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    provider: ProviderPreferences,
}

/// OpenRouter provider-routing preferences.
///
/// Embeddings are latency-sensitive (a semantic search blocks on embedding the
/// query) and upstreams for the same model differ wildly: measured 2026-07-12,
/// qwen3-embedding-8b took ~0.5s on two upstreams and 13–80s on a third, and
/// OpenRouter's default price-sorted routing kept picking the slow one. Sorting
/// by latency routes around pathological upstreams without pinning any by name.
#[derive(Serialize)]
struct ProviderPreferences {
    sort: &'static str,
}

impl ProviderPreferences {
    fn lowest_latency() -> Self {
        Self { sort: "latency" }
    }
}

/// OpenRouter Embeddings API response
#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

/// OpenRouter may return HTTP 200 with an error body when the upstream provider
/// fails after OpenRouter has started proxying the request.
#[derive(Deserialize)]
struct OpenRouterErrorResponse {
    error: OpenRouterErrorDetail,
}

#[derive(Deserialize)]
struct OpenRouterErrorDetail {
    #[serde(default)]
    code: Option<serde_json::Value>,
    #[serde(default)]
    message: Option<String>,
}

/// Generate embeddings for multiple texts via OpenRouter API
pub async fn embed_batch(
    provider: &OpenRouterProvider,
    texts: &[String],
    config: &EmbeddingConfig,
) -> Result<Vec<Vec<f32>>, ProviderError> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let request = EmbeddingRequest {
        model: config.model.clone(),
        input: texts.to_vec(),
        dimensions: config.dimensions,
        provider: ProviderPreferences::lowest_latency(),
    };

    let response = provider
        .client()
        .post(format!("{}/embeddings", provider.base_url()))
        .header("Authorization", format!("Bearer {}", provider.api_key()))
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://atomic.app")
        .header("X-Title", "Atomic")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let body = response.text().await.unwrap_or_default();

        if status == 429 {
            tracing::warn!(status, retry_after, model = %config.model, body_preview = %crate::providers::error::truncate_utf8(&body, 200), "OpenRouter embedding rate limited");
            return Err(ProviderError::RateLimited {
                retry_after_secs: retry_after,
            });
        }

        tracing::error!(status, model = %config.model, body_preview = %crate::providers::error::truncate_utf8(&body, 500), "OpenRouter embedding API error");
        return Err(ProviderError::Api {
            status,
            message: body,
        });
    }

    let body = response.text().await?;

    // OpenRouter can return HTTP 200 with an error body when the upstream
    // provider fails after proxying has started. Check for this before
    // trying to parse as a successful embedding response.
    if let Ok(err_resp) = serde_json::from_str::<OpenRouterErrorResponse>(&body) {
        let message = err_resp
            .error
            .message
            .unwrap_or_else(|| "Unknown upstream error".to_string());
        let code = err_resp
            .error
            .code
            .map(|c| c.to_string())
            .unwrap_or_default();
        tracing::error!(
            model = %config.model,
            error_code = %code,
            error_message = %message,
            "OpenRouter returned 200 with error body (upstream provider failure)"
        );
        // Always treat as 502 (upstream failure) so the adaptive retry
        // will split the batch — the upstream error code may be 400
        // (e.g. payload too large) but reducing batch size can fix it.
        return Err(ProviderError::Api {
            status: 502,
            message: format!("[upstream {}] {}", code, message),
        });
    }

    let embedding_response: EmbeddingResponse = serde_json::from_str(&body)
        .map_err(|e| {
            tracing::error!(error = %e, model = %config.model, body_preview = %crate::providers::error::truncate_utf8(&body, 500), "OpenRouter embedding parse error");
            ProviderError::ParseError(format!("Failed to parse embedding response: {e}"))
        })?;

    let mut vectors: Vec<Vec<f32>> = embedding_response
        .data
        .into_iter()
        .map(|d| d.embedding)
        .collect();

    // Enforce the requested Matryoshka width client-side. Our embedding models
    // are MRL-trained, so truncating a longer vector to the target prefix and
    // re-normalizing to unit length yields the same vector the provider would
    // have returned for that dimension. This makes the stored width correct
    // even when a provider silently ignores the `dimensions` parameter (as
    // some OpenRouter upstreams do), so it can never mismatch the vector
    // column the schema was created at.
    if let Some(target) = config.dimensions {
        for v in vectors.iter_mut() {
            if v.len() > target {
                v.truncate(target);
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in v.iter_mut() {
                        *x /= norm;
                    }
                }
            }
        }
    }

    Ok(vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_asks_for_lowest_latency_routing() {
        let request = EmbeddingRequest {
            model: "qwen/qwen3-embedding-8b".to_string(),
            input: vec!["hello".to_string()],
            dimensions: Some(1536),
            provider: ProviderPreferences::lowest_latency(),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["provider"]["sort"], "latency");
        assert_eq!(json["dimensions"], 1536);
    }
}
