//! The CORS policy shared by every deployment of the HTTP surface.
//!
//! Lived in the standalone binary originally; extracted so embedders that
//! compose their own `App` (the Tauri sidecar launcher, atomic-cloud) apply
//! the identical browser-origin policy instead of re-deriving it. The policy
//! is deployment-agnostic: it knows nothing about who is embedding it.
//!
//! Three origin classes are allowed:
//!
//! 1. **Local app shells** — the Tauri/Capacitor webviews and localhost dev
//!    servers that host the product frontend (`tauri://localhost`,
//!    `http://localhost:*`, …).
//! 2. **Browser-extension pages** (`chrome-extension://`, `moz-extension://`,
//!    `safari-web-extension://`). The web clipper ships without
//!    `host_permissions` (Chrome Web Store review prefers `activeTab`-only),
//!    so its fetches are ordinary CORS-governed requests — without this
//!    allowance the options page cannot even test a connection. Extensions
//!    authenticate with a Bearer token; note `supports_credentials` is NOT
//!    enabled, so cookie-credentialed cross-origin reads stay blocked and
//!    the session-cookie plane keeps its same-origin posture.
//! 3. **The deployment's public origin**, when one is configured.

use actix_cors::Cors;
use actix_web::http::header;

/// Build the CORS middleware. `public_url` is the deployment's public origin
/// (`--public-url`), allowed in addition to local-app and extension origins.
pub fn build_cors(public_url: Option<&str>) -> Cors {
    let public_origin = public_url.and_then(origin_from_url);
    Cors::default()
        .allowed_origin_fn(move |origin, _req_head| {
            let Ok(origin) = origin.to_str() else {
                return false;
            };
            is_local_origin(origin)
                || is_extension_origin(origin)
                || public_origin.as_deref() == Some(origin)
        })
        .allowed_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"])
        .allow_any_header()
        .expose_headers(vec![header::HeaderName::from_static("mcp-session-id")])
        .max_age(3600)
}

fn origin_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{scheme}://{host}{port}"))
}

fn is_local_origin(origin: &str) -> bool {
    if matches!(
        origin,
        "tauri://localhost" | "capacitor://localhost" | "ionic://localhost"
    ) {
        return true;
    }

    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    host == "localhost"
        || host == "tauri.localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host.ends_with(".localhost")
}

/// A browser-extension page origin. The scheme alone is the class — an
/// extension's ID is not a secret and not a security boundary; the Bearer
/// token is what authenticates the request.
fn is_extension_origin(origin: &str) -> bool {
    origin.starts_with("chrome-extension://")
        || origin.starts_with("moz-extension://")
        || origin.starts_with("safari-web-extension://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::http::Method;
    use actix_web::test as actix_test;
    use actix_web::{web, App};

    use crate::app::health;

    fn preflight(origin: &str) -> actix_web::test::TestRequest {
        actix_test::TestRequest::default()
            .method(Method::OPTIONS)
            .uri("/health")
            .insert_header((header::ORIGIN, origin))
            .insert_header((header::ACCESS_CONTROL_REQUEST_METHOD, "GET"))
            .insert_header((
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,content-type",
            ))
    }

    #[actix_web::test]
    async fn cors_allows_mcp_session_headers_from_local_origins() {
        let app = actix_test::init_service(
            App::new()
                .wrap(build_cors(None))
                .route("/health", web::get().to(health)),
        )
        .await;

        let req = preflight("http://localhost:5173")
            .insert_header((
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,content-type,mcp-session-id,mcp-protocol-version",
            ))
            .to_request();

        let response = actix_test::call_service(&app, req).await;

        assert!(response.status().is_success());
        let allowed_headers = response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|value| value.to_str().ok())
            .expect("preflight response should include allowed headers");

        assert!(allowed_headers.contains("authorization"));
        assert!(allowed_headers.contains("content-type"));
        assert!(allowed_headers.contains("mcp-session-id"));
        assert!(allowed_headers.contains("mcp-protocol-version"));
    }

    #[actix_web::test]
    async fn cors_exposes_mcp_session_id_to_browser_clients() {
        let app = actix_test::init_service(
            App::new()
                .wrap(build_cors(None))
                .route("/health", web::get().to(health)),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/health")
            .insert_header((header::ORIGIN, "http://localhost:5173"))
            .to_request();

        let response = actix_test::call_service(&app, req).await;

        assert!(response.status().is_success());
        let exposed_headers = response
            .headers()
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
            .and_then(|value| value.to_str().ok())
            .expect("CORS response should expose MCP session header");

        assert!(exposed_headers.contains("mcp-session-id"));
    }

    #[actix_web::test]
    async fn cors_allows_extension_origins_without_credentials() {
        let app = actix_test::init_service(
            App::new()
                .wrap(build_cors(None))
                .route("/health", web::get().to(health)),
        )
        .await;

        for origin in [
            "chrome-extension://bknijbafnefbaklndpglcmlhaglikccf",
            "moz-extension://5a3f1e30-6a1c-4a2b-9d3e-000000000000",
            "safari-web-extension://ABC123",
        ] {
            let response = actix_test::call_service(&app, preflight(origin).to_request()).await;
            assert!(response.status().is_success(), "{origin} preflight");
            let allow_origin = response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok())
                .expect("extension preflight should be allowed");
            assert_eq!(allow_origin, origin);
            // Bearer-token clients only: never allow cookie credentials, so
            // the session-cookie plane stays same-origin.
            assert!(
                response
                    .headers()
                    .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                    .is_none(),
                "credentials must not be allowed for {origin}"
            );
        }
    }

    #[actix_web::test]
    async fn cors_rejects_foreign_web_origins() {
        let app = actix_test::init_service(
            App::new()
                .wrap(build_cors(None))
                .route("/health", web::get().to(health)),
        )
        .await;

        let response =
            actix_test::call_service(&app, preflight("https://evil.example").to_request()).await;
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "foreign web origin must not be allowed"
        );
    }

    #[actix_web::test]
    async fn cors_allows_configured_public_origin() {
        let app = actix_test::init_service(
            App::new()
                .wrap(build_cors(Some("https://notes.example.com/")))
                .route("/health", web::get().to(health)),
        )
        .await;

        let response = actix_test::call_service(
            &app,
            preflight("https://notes.example.com").to_request(),
        )
        .await;
        assert!(response.status().is_success());
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://notes.example.com")
        );
    }
}
