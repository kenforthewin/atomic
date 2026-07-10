//! Instance setup endpoint — allows claiming an unconfigured instance

use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use utoipa::ToSchema;

const SETUP_CLAIMED_AT_KEY: &str = "setup.claimed_at";

#[derive(Serialize, ToSchema)]
pub struct SetupStatusResponse {
    pub needs_setup: bool,
    pub already_claimed: bool,
    pub requires_setup_token: bool,
    pub setup_token_configured: bool,
}

#[utoipa::path(
    get,
    path = "/api/setup/status",
    responses(
        (status = 200, description = "Whether the instance needs initial setup", body = SetupStatusResponse)
    ),
    tag = "setup",
    security(())
)]
pub async fn setup_status(state: web::Data<AppState>) -> HttpResponse {
    let core = match state.manager.active_core().await {
        Ok(c) => c,
        Err(e) => return crate::error::error_response(e),
    };
    match core.list_api_tokens().await {
        Ok(tokens) => {
            let active = tokens.iter().filter(|t| !t.is_revoked).count();
            let settings = match core.get_settings().await {
                Ok(s) => s,
                Err(e) => return crate::error::error_response(e),
            };
            let already_claimed = settings.contains_key(SETUP_CLAIMED_AT_KEY) || !tokens.is_empty();
            let needs_setup = !already_claimed && active == 0;
            HttpResponse::Ok().json(SetupStatusResponse {
                needs_setup,
                already_claimed,
                requires_setup_token: needs_setup && !state.dangerously_skip_setup_token,
                setup_token_configured: state.setup_token.is_some(),
            })
        }
        Err(e) => crate::error::error_response(e),
    }
}

#[derive(Deserialize, ToSchema)]
pub struct ClaimBody {
    pub name: Option<String>,
    pub setup_token: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ClaimResponse {
    pub id: String,
    pub name: String,
    pub token: String,
    pub prefix: String,
    pub created_at: String,
}

#[utoipa::path(
    post,
    path = "/api/setup/claim",
    request_body = ClaimBody,
    responses(
        (status = 201, description = "Instance claimed and first token created", body = ClaimResponse),
        (status = 409, description = "Instance already has an active token")
    ),
    tag = "setup",
    security(())
)]
pub async fn claim_instance(
    state: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<ClaimBody>,
) -> HttpResponse {
    if let Some(ip) = client_ip(&req) {
        if !state.setup_claim_limiter.check(ip) {
            return HttpResponse::TooManyRequests().json(serde_json::json!({
                "error": "Too many setup claim attempts. Try again shortly."
            }));
        }
    }

    let body = body.into_inner();
    if !state.dangerously_skip_setup_token {
        let Some(setup_token) = state.setup_token.as_ref() else {
            return HttpResponse::Forbidden().json(serde_json::json!({
                "error": "Setup requires ATOMIC_SETUP_TOKEN to be configured on the server"
            }));
        };
        let submitted = body.setup_token.as_deref().unwrap_or_default();
        if !setup_token.verify(submitted) {
            return HttpResponse::Forbidden().json(serde_json::json!({
                "error": "Invalid setup token"
            }));
        }
    }

    let name = body
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());

    let _guard = state.setup_claim_lock.lock().await;
    let core = match state.manager.active_core().await {
        Ok(c) => c,
        Err(e) => return crate::error::error_response(e),
    };

    // Check that this instance has never been claimed before.
    let tokens = match core.list_api_tokens().await {
        Ok(t) => t,
        Err(e) => return crate::error::error_response(e),
    };
    let settings = match core.get_settings().await {
        Ok(s) => s,
        Err(e) => return crate::error::error_response(e),
    };
    if settings.contains_key(SETUP_CLAIMED_AT_KEY) || !tokens.is_empty() {
        return HttpResponse::Conflict().json(serde_json::json!({
            "error": "Instance already claimed"
        }));
    }
    match core.create_api_token(&name).await {
        Ok((info, raw_token)) => {
            if let Err(e) = core
                .set_setting(SETUP_CLAIMED_AT_KEY, &Utc::now().to_rfc3339())
                .await
            {
                return crate::error::error_response(e);
            }
            HttpResponse::Created().json(ClaimResponse {
                id: info.id,
                name: info.name,
                token: raw_token,
                prefix: info.token_prefix,
                created_at: info.created_at,
            })
        }
        Err(e) => crate::error::error_response(e),
    }
}

fn client_ip(req: &HttpRequest) -> Option<IpAddr> {
    req.peer_addr().map(|addr| addr.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        export_jobs::ExportJobManager,
    migration_jobs::MigrationJobManager,
        log_buffer::LogBuffer,
        state::{SetupClaimLimiter, SetupToken},
    };
    use actix_web::{test as actix_test, web, App};
    use std::sync::Arc;
    use tokio::sync::{broadcast, Mutex};

    fn test_state(
        setup_token: Option<&str>,
        dangerously_skip_setup_token: bool,
    ) -> web::Data<AppState> {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(atomic_core::DatabaseManager::new(temp.path()).unwrap());
        let (event_tx, _) = broadcast::channel(16);
        let state = web::Data::new(AppState {
            manager,
            event_tx,
            public_url: None,
            log_buffer: LogBuffer::new(16),
            export_jobs: ExportJobManager::for_tests(temp.path().join("exports")),
            migration_jobs: MigrationJobManager::for_tests(temp.path().join("migrations")),
            setup_token: setup_token.map(|token| SetupToken::from_raw(token.to_string()).unwrap()),
            dangerously_skip_setup_token,
            setup_claim_lock: Mutex::new(()),
            setup_claim_limiter: SetupClaimLimiter::new(),
        });
        std::mem::forget(temp);
        state
    }

    #[actix_web::test]
    async fn local_claim_without_setup_token_fails_by_default() {
        let state = test_state(None, false);
        let app = actix_test::init_service(
            App::new()
                .app_data(state)
                .route("/api/setup/claim", web::post().to(claim_instance)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/api/setup/claim")
            .peer_addr("127.0.0.1:1234".parse().unwrap())
            .set_json(serde_json::json!({ "name": "admin" }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 403);
    }

    #[actix_web::test]
    async fn dangerous_skip_setup_token_allows_remote_claim_without_token() {
        let state = test_state(None, true);
        let app = actix_test::init_service(
            App::new()
                .app_data(state)
                .route("/api/setup/claim", web::post().to(claim_instance)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/api/setup/claim")
            .peer_addr("203.0.113.10:1234".parse().unwrap())
            .set_json(serde_json::json!({ "name": "admin" }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);
    }

    #[actix_web::test]
    async fn remote_claim_without_configured_setup_token_fails() {
        let state = test_state(None, false);
        let app = actix_test::init_service(
            App::new()
                .app_data(state)
                .route("/api/setup/claim", web::post().to(claim_instance)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/api/setup/claim")
            .peer_addr("203.0.113.10:1234".parse().unwrap())
            .set_json(serde_json::json!({ "name": "admin" }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 403);
    }

    #[actix_web::test]
    async fn remote_claim_with_valid_setup_token_succeeds() {
        let state = test_state(Some("secret-setup-token"), false);
        let app = actix_test::init_service(
            App::new()
                .app_data(state)
                .route("/api/setup/claim", web::post().to(claim_instance)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/api/setup/claim")
            .peer_addr("203.0.113.10:1234".parse().unwrap())
            .set_json(serde_json::json!({
                "name": "admin",
                "setup_token": "secret-setup-token"
            }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);
    }

    #[actix_web::test]
    async fn claim_does_not_reopen_after_token_history_exists() {
        let state = test_state(Some("secret-setup-token"), false);
        let core = state.manager.active_core().await.unwrap();
        core.create_api_token("existing").await.unwrap();

        let app = actix_test::init_service(
            App::new()
                .app_data(state)
                .route("/api/setup/claim", web::post().to(claim_instance)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/api/setup/claim")
            .peer_addr("203.0.113.10:1234".parse().unwrap())
            .set_json(serde_json::json!({
                "name": "admin",
                "setup_token": "secret-setup-token"
            }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 409);
    }
}
