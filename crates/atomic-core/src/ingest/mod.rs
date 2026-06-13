//! Content ingestion pipeline — URL fetching, article extraction, and RSS feed management.
//!
//! Two-layer architecture:
//! 1. `resolve_url()` — fetch + extract, no DB interaction
//! 2. `AtomicCore::ingest_url()` — dedup, create atom, trigger embedding

pub mod extract;
pub mod fetch;
pub mod poller;
pub mod rss;

use serde::{Deserialize, Serialize};

// ==================== Events ====================

/// Events emitted during the ingestion pipeline.
/// Follows the same callback pattern as `EmbeddingEvent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IngestionEvent {
    FetchStarted {
        url: String,
        request_id: String,
    },
    FetchComplete {
        url: String,
        request_id: String,
        content_length: usize,
    },
    FetchFailed {
        url: String,
        request_id: String,
        error: String,
    },
    /// Page wasn't article-shaped — no atom created.
    Skipped {
        url: String,
        request_id: String,
        reason: String,
    },
    IngestionComplete {
        request_id: String,
        atom_id: String,
        url: String,
        title: String,
    },
    IngestionFailed {
        request_id: String,
        url: String,
        error: String,
    },
    FeedPollComplete {
        feed_id: String,
        new_items: i32,
        skipped: i32,
        errors: i32,
    },
    FeedPollFailed {
        feed_id: String,
        error: String,
    },
}

// ==================== Request / Result types ====================

/// Request to ingest a single URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionRequest {
    pub url: String,
    #[serde(default)]
    pub tag_ids: Vec<String>,
    pub title_hint: Option<String>,
    pub published_at: Option<String>,
}

/// Successful ingestion result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionResult {
    pub atom_id: String,
    pub url: String,
    pub title: String,
    pub content_length: usize,
}

/// Feed poll summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedPollResult {
    pub feed_id: String,
    pub new_items: i32,
    pub skipped: i32,
    pub errors: i32,
}

/// Resolved content from a URL — the output of fetch + extract, before DB writes.
pub struct ResolvedContent {
    pub title: String,
    pub markdown: String,
    pub byline: Option<String>,
    pub excerpt: Option<String>,
    pub site_name: Option<String>,
}

// ==================== Core resolve function ====================

/// Fetch a URL, check readability, and extract article content as markdown.
/// Does NOT touch the database. Emits events via the callback.
pub async fn resolve_url<F>(
    url: &str,
    request_id: &str,
    on_event: &F,
) -> Result<ResolvedContent, String>
where
    F: Fn(IngestionEvent),
{
    on_event(IngestionEvent::FetchStarted {
        url: url.to_string(),
        request_id: request_id.to_string(),
    });

    let html = match fetch::fetch_html(url).await {
        Ok(html) => {
            on_event(IngestionEvent::FetchComplete {
                url: url.to_string(),
                request_id: request_id.to_string(),
                content_length: html.len(),
            });
            html
        }
        Err(e) => {
            on_event(IngestionEvent::FetchFailed {
                url: url.to_string(),
                request_id: request_id.to_string(),
                error: e.clone(),
            });
            return Err(e);
        }
    };

    match extract::extract_article(&html, url) {
        Ok(article) => Ok(ResolvedContent {
            title: article.title,
            markdown: article.content,
            byline: article.byline,
            excerpt: article.excerpt,
            site_name: article.site_name,
        }),
        Err(reason) => {
            on_event(IngestionEvent::Skipped {
                url: url.to_string(),
                request_id: request_id.to_string(),
                reason: reason.clone(),
            });
            Err(reason)
        }
    }
}
