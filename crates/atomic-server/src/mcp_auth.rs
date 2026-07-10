//! MCP-specific Bearer auth middleware
//!
//! Similar to `auth::BearerAuth` but returns `WWW-Authenticate` header with
//! `resource_metadata` pointing to the OAuth discovery endpoint, as required
//! by the MCP spec for OAuth-capable servers.
//!
//! Uses `EitherBody` so that 401 responses flow back through the CORS
//! middleware as `Ok(response)` rather than `Err(error)` — this ensures
//! cross-origin MCP clients (like the MCP Inspector) receive CORS headers
//! on auth failures too.

use crate::state::AppState;
use actix_web::body::EitherBody;
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::web;
use actix_web::Error;
use actix_web::HttpResponse;
use futures::future::{ok, LocalBoxFuture, Ready};
use std::rc::Rc;
use std::task::{Context, Poll};

/// Middleware for the `/mcp` scope that verifies Bearer tokens and returns
/// MCP-compliant `WWW-Authenticate` headers on 401.
pub struct McpAuth {
    pub state: web::Data<AppState>,
}

impl<S, B> Transform<S, ServiceRequest> for McpAuth
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Transform = McpAuthMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(McpAuthMiddleware {
            service: Rc::new(service),
            state: self.state.clone(),
        })
    }
}

pub struct McpAuthMiddleware<S> {
    service: Rc<S>,
    state: web::Data<AppState>,
}

impl<S, B> Service<ServiceRequest> for McpAuthMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<EitherBody<B>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let state = self.state.clone();

        let raw_token = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(String::from);

        let www_authenticate = {
            let public_url = state
                .public_url
                .as_deref()
                .unwrap_or("")
                .trim_end_matches('/');
            format!(
                "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
                public_url
            )
        };

        let raw_token = match raw_token {
            Some(t) => t,
            None => {
                return Box::pin(async move {
                    Ok(req
                        .into_response(
                            HttpResponse::Unauthorized()
                                .insert_header(("WWW-Authenticate", www_authenticate))
                                .json(serde_json::json!({"error": "unauthorized"})),
                        )
                        .map_into_right_body())
                });
            }
        };

        let svc = Rc::clone(&self.service);
        Box::pin(async move {
            // Verify token via active core (routes through registry or storage backend)
            let core = match state.manager.active_core().await {
                Ok(c) => c,
                _ => {
                    return Ok(req
                        .into_response(
                            HttpResponse::Unauthorized()
                                .insert_header(("WWW-Authenticate", www_authenticate))
                                .json(serde_json::json!({"error": "server_error"})),
                        )
                        .map_into_right_body());
                }
            };

            let token_info = match core.verify_api_token(&raw_token).await {
                Ok(Some(info)) => info,
                _ => {
                    return Ok(req
                        .into_response(
                            HttpResponse::Unauthorized()
                                .insert_header(("WWW-Authenticate", www_authenticate))
                                .json(serde_json::json!({"error": "invalid_token"})),
                        )
                        .map_into_right_body());
                }
            };

            // Fire-and-forget last_used_at update
            let token_id = token_info.id.clone();
            let core_clone = core.clone();
            tokio::spawn(async move {
                let _ = core_clone.update_token_last_used(&token_id).await;
            });

            svc.call(req).await.map(|res| res.map_into_left_body())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, ServerEvent};
    use actix_web::test as actix_test;
    use actix_web::App;
    use tokio::sync::broadcast;

    async fn protected_endpoint() -> HttpResponse {
        HttpResponse::Ok().json(serde_json::json!({"ok": true}))
    }

    async fn test_state(public_url: Option<&str>) -> (web::Data<AppState>, String) {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = std::sync::Arc::new(atomic_core::DatabaseManager::new(temp.path()).unwrap());
        let (_, raw_token) = manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("test-token")
            .await
            .unwrap();
        let (event_tx, _) = broadcast::channel::<ServerEvent>(16);
        let state = web::Data::new(AppState {
            manager,
            event_tx,
            public_url: public_url.map(String::from),
            log_buffer: crate::log_buffer::LogBuffer::new(16),
            export_jobs: crate::export_jobs::ExportJobManager::for_tests(
                temp.path().join("exports"),
            ),
            migration_jobs: crate::migration_jobs::MigrationJobManager::for_tests(
                temp.path().join("migrations"),
            ),
            setup_token: None,
            dangerously_skip_setup_token: false,
            setup_claim_lock: tokio::sync::Mutex::new(()),
            setup_claim_limiter: crate::state::SetupClaimLimiter::new(),
        });
        std::mem::forget(temp);
        (state, raw_token)
    }

    #[actix_web::test]
    async fn test_valid_token_passes() {
        let (state, raw_token) = test_state(Some("https://atomic.example.com")).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/mcp/ping")
            .insert_header(("Authorization", format!("Bearer {}", raw_token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn test_missing_token_returns_401_with_www_authenticate() {
        let (state, _) = test_state(Some("https://atomic.example.com")).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get().uri("/mcp/ping").to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);

        let www_auth = resp
            .headers()
            .get("WWW-Authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www_auth.contains("resource_metadata="));
        assert!(www_auth.contains("/.well-known/oauth-protected-resource"));
        assert!(www_auth.contains("https://atomic.example.com"));
    }

    #[actix_web::test]
    async fn test_invalid_token_returns_401_with_www_authenticate() {
        let (state, _) = test_state(Some("https://atomic.example.com")).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/mcp/ping")
            .insert_header(("Authorization", "Bearer bad_token"))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);

        let www_auth = resp
            .headers()
            .get("WWW-Authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www_auth.contains("resource_metadata="));
    }

    #[actix_web::test]
    async fn test_revoked_token_returns_401() {
        let (state, raw_token) = test_state(Some("https://atomic.example.com")).await;

        let core = state.manager.active_core().await.unwrap();
        core.create_api_token("replacement").await.unwrap();
        let tokens = core.list_api_tokens().await.unwrap();
        let token_to_revoke = tokens.iter().find(|t| t.name == "test-token").unwrap();
        core.revoke_api_token(&token_to_revoke.id).await.unwrap();

        let app = actix_test::init_service(
            App::new().service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/mcp/ping")
            .insert_header(("Authorization", format!("Bearer {}", raw_token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }

    #[actix_web::test]
    async fn test_no_public_url_still_returns_www_authenticate() {
        let (state, _) = test_state(None).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get().uri("/mcp/ping").to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);

        let www_auth = resp
            .headers()
            .get("WWW-Authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www_auth.contains("resource_metadata="));
    }

    #[actix_web::test]
    async fn test_401_is_ok_response_not_error() {
        // Verify that auth failures return Ok(401) not Err, so CORS middleware
        // can add headers to the response
        let (state, _) = test_state(Some("https://atomic.example.com")).await;
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/mcp")
                    .wrap(McpAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get().uri("/mcp/ping").to_request();
        // call_service (not try_call_service) — panics if it gets Err
        // This proves we return Ok(401), not Err
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }
}
