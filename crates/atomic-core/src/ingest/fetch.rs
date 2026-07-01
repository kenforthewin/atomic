//! HTTP fetching with concurrency limits via FETCH_SEMAPHORE.

use crate::executor::FETCH_SEMAPHORE;
use std::sync::LazyLock;

/// Shared reqwest client for ingestion — separate from AI provider clients.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("Mozilla/5.0 (compatible; Atomic/1.0; +https://atomicapp.ai)")
        .build()
        .expect("Failed to build HTTP client")
});

/// Fetch a URL, returning the raw body bytes.
/// Acquires a FETCH_SEMAPHORE permit before making the request.
/// Accepts any content type (suitable for RSS/Atom XML feeds).
pub async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let _permit = FETCH_SEMAPHORE
        .acquire()
        .await
        .map_err(|_| "Fetch semaphore closed".to_string())?;

    let response = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {} for {}", status, url));
    }

    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("Failed to read response body: {}", e))
}

/// Fetch a URL, returning the HTML body as a string.
/// Acquires a FETCH_SEMAPHORE permit before making the request.
/// Rejects non-HTML content types early.
pub async fn fetch_html(url: &str) -> Result<String, String> {
    let _permit = FETCH_SEMAPHORE
        .acquire()
        .await
        .map_err(|_| "Fetch semaphore closed".to_string())?;

    let response = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {} for {}", status, url));
    }

    // Reject non-HTML content types
    if let Some(ct) = response.headers().get(reqwest::header::CONTENT_TYPE) {
        let ct_str = ct.to_str().unwrap_or("");
        if !ct_str.contains("text/html") && !ct_str.contains("application/xhtml") {
            return Err(format!("Non-HTML content type: {}", ct_str));
        }
    }

    response
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))
}
