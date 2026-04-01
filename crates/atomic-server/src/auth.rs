//! Bearer token authentication middleware — verifies tokens against the database

use crate::state::AppState;
use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::error::ErrorUnauthorized;
use actix_web::web;
use actix_web::Error;
use futures::future::{ok, LocalBoxFuture, Ready};
use std::task::{Context, Poll};

/// Middleware that requires a valid Bearer token (looked up in the api_tokens table)
pub struct BearerAuth {
    pub state: web::Data<AppState>,
}

impl<S, B> Transform<S, ServiceRequest> for BearerAuth
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = BearerAuthMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(BearerAuthMiddleware {
            service,
            state: self.state.clone(),
        })
    }
}

pub struct BearerAuthMiddleware<S> {
    service: S,
    state: web::Data<AppState>,
}

impl<S, B> Service<ServiceRequest> for BearerAuthMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let state = self.state.clone();

        // If the server is not yet initialized (encrypted DB waiting for unlock,
        // or fresh install awaiting setup), return 503 instead of a misleading 401.
        if !state.manager.is_initialized() {
            return Box::pin(async {
                Err(actix_web::error::ErrorServiceUnavailable(
                    "Server not initialized — complete setup or unlock the database first",
                ))
            });
        }

        // Extract the Authorization header
        let raw_token = req
            .headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(String::from);

        let raw_token = match raw_token {
            Some(t) => t,
            None => {
                return Box::pin(async {
                    Err(ErrorUnauthorized("Invalid or missing Bearer token"))
                });
            }
        };

        // Verify token — uses registry if available, falls through to storage backend otherwise
        let core = match state.manager.active_core() {
            Ok(c) => c,
            Err(_) => {
                return Box::pin(async {
                    Err(ErrorUnauthorized("Invalid or missing Bearer token"))
                });
            }
        };
        let token_info = match core.verify_api_token(&raw_token) {
            Ok(Some(info)) => info,
            Ok(None) => {
                return Box::pin(async {
                    Err(ErrorUnauthorized("Invalid or missing Bearer token"))
                });
            }
            Err(_) => {
                return Box::pin(async {
                    Err(ErrorUnauthorized("Invalid or missing Bearer token"))
                });
            }
        };

        // Fire-and-forget last_used_at update
        let token_id = token_info.id.clone();
        let core_clone = core.clone();
        tokio::task::spawn_blocking(move || {
            let _ = core_clone.update_token_last_used(&token_id);
        });

        let fut = self.service.call(req);
        Box::pin(async move { fut.await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test as actix_test;
    use actix_web::{web, App, HttpResponse};
    use tokio::sync::broadcast;

    async fn protected_endpoint() -> HttpResponse {
        HttpResponse::Ok().json(serde_json::json!({"ok": true}))
    }

    fn test_app_state() -> (web::Data<AppState>, String) {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = std::sync::Arc::new(
            atomic_core::DatabaseManager::new(temp.path()).unwrap()
        );
        let (info, raw_token) = manager.registry().unwrap().create_api_token("test-token").unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let state = web::Data::new(AppState { manager, event_tx, public_url: None, log_buffer: crate::log_buffer::LogBuffer::new(10) });
        // Leak the tempdir so the DB stays alive during the test
        std::mem::forget(temp);
        let _ = info;
        (state, raw_token)
    }

    #[actix_web::test]
    async fn test_valid_bearer_token() {
        let (state, raw_token) = test_app_state();
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/ping")
            .insert_header(("Authorization", format!("Bearer {}", raw_token)))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn test_missing_auth_header() {
        let (state, _) = test_app_state();
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/ping")
            .to_request();
        let resp = actix_test::try_call_service(&app, req).await;
        assert!(resp.is_err());
    }

    #[actix_web::test]
    async fn test_wrong_bearer_token() {
        let (state, _) = test_app_state();
        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/ping")
            .insert_header(("Authorization", "Bearer wrong-token"))
            .to_request();
        let resp = actix_test::try_call_service(&app, req).await;
        assert!(resp.is_err());
    }

    #[actix_web::test]
    async fn test_revoked_token_rejected() {
        let (state, raw_token) = test_app_state();

        // Get the token ID and revoke it
        let tokens = state.manager.registry().unwrap().list_api_tokens().unwrap();
        let token_id = &tokens[0].id;
        state.manager.registry().unwrap().revoke_api_token(token_id).unwrap();

        let app = actix_test::init_service(
            App::new().service(
                web::scope("/api")
                    .wrap(BearerAuth {
                        state: state.clone(),
                    })
                    .route("/ping", web::get().to(protected_endpoint)),
            ),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/api/ping")
            .insert_header(("Authorization", format!("Bearer {}", raw_token)))
            .to_request();
        let resp = actix_test::try_call_service(&app, req).await;
        assert!(resp.is_err());
    }
}
