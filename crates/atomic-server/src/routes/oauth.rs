//! OAuth 2.0 endpoints for MCP remote auth (Claude.ai compatibility)
//!
//! Implements Dynamic Client Registration, Authorization Code with PKCE,
//! and Token Exchange. Discovery endpoints allow MCP clients to find these
//! automatically.

use crate::state::AppState;
use actix_web::{web, HttpResponse};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::{IntoParams, ToSchema};

// ==================== Discovery Endpoints ====================

#[derive(Debug, Serialize, ToSchema)]
pub struct OAuthProtectedResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub bearer_methods_supported: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/.well-known/oauth-protected-resource",
    responses(
        (status = 200, description = "OAuth protected resource metadata", body = OAuthProtectedResourceMetadata),
        (status = 404, description = "OAuth is not configured")
    ),
    tag = "oauth",
    security(())
)]
pub async fn resource_metadata(state: web::Data<AppState>) -> HttpResponse {
    let public_url = match &state.public_url {
        Some(url) => url.trim_end_matches('/'),
        None => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "OAuth not configured — start server with --public-url"
            }))
        }
    };

    HttpResponse::Ok().json(OAuthProtectedResourceMetadata {
        resource: public_url.to_string(),
        authorization_servers: vec![public_url.to_string()],
        bearer_methods_supported: vec!["header".to_string()],
    })
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OAuthAuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    pub response_types_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/.well-known/oauth-authorization-server",
    responses(
        (status = 200, description = "OAuth authorization server metadata", body = OAuthAuthorizationServerMetadata),
        (status = 404, description = "OAuth is not configured")
    ),
    tag = "oauth",
    security(())
)]
pub async fn metadata(state: web::Data<AppState>) -> HttpResponse {
    let public_url = match &state.public_url {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => {
            return HttpResponse::NotFound().json(serde_json::json!({
                "error": "OAuth not configured — start server with --public-url"
            }))
        }
    };

    HttpResponse::Ok().json(OAuthAuthorizationServerMetadata {
        issuer: public_url.clone(),
        authorization_endpoint: format!("{}/oauth/authorize", public_url),
        token_endpoint: format!("{}/oauth/token", public_url),
        registration_endpoint: format!("{}/oauth/register", public_url),
        response_types_supported: vec!["code".to_string()],
        grant_types_supported: vec!["authorization_code".to_string()],
        code_challenge_methods_supported: vec!["S256".to_string()],
        token_endpoint_auth_methods_supported: vec!["client_secret_post".to_string()],
    })
}

// ==================== Dynamic Client Registration ====================

#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)]
pub struct RegisterRequest {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub grant_types: Option<Vec<String>>,
    #[serde(default)]
    pub response_types: Option<Vec<String>>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterResponse {
    pub client_id: String,
    pub client_secret: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub token_endpoint_auth_method: String,
}

#[utoipa::path(
    post,
    path = "/oauth/register",
    request_body = RegisterRequest,
    responses(
        (status = 201, description = "OAuth client registered", body = RegisterResponse),
        (status = 400, description = "Invalid client metadata"),
        (status = 404, description = "OAuth is not configured")
    ),
    tag = "oauth",
    security(())
)]
pub async fn register(
    state: web::Data<AppState>,
    body: web::Json<RegisterRequest>,
) -> HttpResponse {
    if state.public_url.is_none() {
        return HttpResponse::NotFound().json(serde_json::json!({
            "error": "OAuth not configured"
        }));
    }

    let req = body.into_inner();

    if req.redirect_uris.is_empty() {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "invalid_client_metadata",
            "error_description": "redirect_uris must not be empty"
        }));
    }

    // Generate client_secret: 32 random bytes, base64url-encoded
    let mut secret_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret_bytes);
    let client_secret = URL_SAFE_NO_PAD.encode(secret_bytes);

    // Hash the secret for storage
    let secret_hash = hex::encode(Sha256::digest(client_secret.as_bytes()));

    let redirect_uris_json = serde_json::to_string(&req.redirect_uris).unwrap_or_default();

    let core = match state.manager.active_core().await {
        Ok(c) => c,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "server_error",
                "error_description": e.to_string()
            }))
        }
    };
    let client_id = match core
        .create_oauth_client(&req.client_name, &secret_hash, &redirect_uris_json)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "server_error",
                "error_description": e.to_string()
            }))
        }
    };

    HttpResponse::Created().json(RegisterResponse {
        client_id,
        client_secret,
        client_name: req.client_name,
        redirect_uris: req.redirect_uris,
        grant_types: vec!["authorization_code".to_string()],
        response_types: vec!["code".to_string()],
        token_endpoint_auth_method: "client_secret_post".to_string(),
    })
}

// ==================== Authorization Endpoint ====================

#[derive(Debug, Deserialize, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
pub struct AuthorizeQuery {
    pub client_id: String,
    pub redirect_uri: String,
    pub response_type: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: Option<String>,
}

#[utoipa::path(
    get,
    path = "/oauth/authorize",
    params(AuthorizeQuery),
    responses(
        (status = 200, description = "OAuth consent page HTML"),
        (status = 302, description = "Redirect with OAuth error"),
        (status = 404, description = "OAuth is not configured")
    ),
    tag = "oauth",
    security(())
)]
pub async fn authorize_page(
    state: web::Data<AppState>,
    query: web::Query<AuthorizeQuery>,
) -> HttpResponse {
    if state.public_url.is_none() {
        return HttpResponse::NotFound().body("OAuth not configured");
    }

    let q = query.into_inner();

    if q.response_type != "code" {
        return redirect_with_error(
            &q.redirect_uri,
            "unsupported_response_type",
            q.state.as_deref(),
        );
    }

    if q.code_challenge_method != "S256" {
        return redirect_with_error(&q.redirect_uri, "invalid_request", q.state.as_deref());
    }

    // Look up client
    let core = match state.manager.active_core().await {
        Ok(c) => c,
        Err(_) => return HttpResponse::InternalServerError().body("Database error"),
    };
    let client_name: String = match core.get_oauth_client_name(&q.client_id).await {
        Ok(Some(name)) => name,
        Ok(None) => {
            return redirect_with_error(&q.redirect_uri, "invalid_request", q.state.as_deref())
        }
        Err(_) => return HttpResponse::InternalServerError().body("Database error"),
    };

    // Serve the HTML consent page
    let html = consent_page_html(
        &client_name,
        &q.client_id,
        &q.redirect_uri,
        &q.code_challenge,
        &q.code_challenge_method,
        q.state.as_deref().unwrap_or(""),
    );

    HttpResponse::Ok().content_type("text/html").body(html)
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AuthorizeApproveForm {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: Option<String>,
    pub api_token: String,
    pub action: String, // "approve" or "deny"
}

#[utoipa::path(
    post,
    path = "/oauth/authorize",
    request_body(content = AuthorizeApproveForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 200, description = "Validation error page HTML"),
        (status = 302, description = "Redirect with authorization code or OAuth error"),
        (status = 400, description = "Invalid authorization request")
    ),
    tag = "oauth",
    security(())
)]
pub async fn authorize_approve(
    state: web::Data<AppState>,
    form: web::Form<AuthorizeApproveForm>,
) -> HttpResponse {
    let f = form.into_inner();

    if f.action != "approve" {
        return redirect_with_error(&f.redirect_uri, "access_denied", f.state.as_deref());
    }

    // Verify the user's API token
    let core = match state.manager.active_core().await {
        Ok(c) => c,
        Err(_) => return HttpResponse::InternalServerError().body("Database error"),
    };
    match core.verify_api_token(&f.api_token).await {
        Ok(Some(_)) => {}
        _ => {
            return HttpResponse::Ok()
                .content_type("text/html")
                .body(error_page_html("Invalid API token. Please try again."));
        }
    }

    // Verify client_id exists and redirect_uri matches
    let redirect_uris_json: String = match core.get_oauth_client_redirect_uris(&f.client_id).await {
        Ok(Some(uris)) => uris,
        Ok(None) => {
            return redirect_with_error(&f.redirect_uri, "invalid_request", f.state.as_deref())
        }
        Err(_) => return HttpResponse::InternalServerError().body("Database error"),
    };

    let registered_uris: Vec<String> =
        serde_json::from_str(&redirect_uris_json).unwrap_or_default();
    if !registered_uris.contains(&f.redirect_uri) {
        return HttpResponse::BadRequest().body("redirect_uri does not match registered URIs");
    }

    // Generate authorization code
    let mut code_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut code_bytes);
    let code = URL_SAFE_NO_PAD.encode(code_bytes);
    let code_hash = hex::encode(Sha256::digest(code.as_bytes()));

    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::minutes(5);

    if let Err(e) = core
        .store_oauth_code(
            &code_hash,
            &f.client_id,
            &f.code_challenge,
            &f.code_challenge_method,
            &f.redirect_uri,
            &now.to_rfc3339(),
            &expires_at.to_rfc3339(),
        )
        .await
    {
        return HttpResponse::InternalServerError().json(serde_json::json!({
            "error": "server_error",
            "error_description": e.to_string()
        }));
    }

    // Redirect with code
    let mut redirect = format!("{}?code={}", f.redirect_uri, code);
    if let Some(s) = &f.state {
        redirect.push_str(&format!("&state={}", s));
    }

    HttpResponse::Found()
        .insert_header(("Location", redirect))
        .finish()
}

// ==================== Token Exchange ====================

#[derive(Debug, Deserialize, ToSchema)]
#[allow(dead_code)]
pub struct TokenRequest {
    pub grant_type: String,
    pub code: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub code_verifier: Option<String>,
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
}

#[utoipa::path(
    post,
    path = "/oauth/token",
    request_body(content = TokenRequest, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 200, description = "Access token response", body = TokenResponse),
        (status = 400, description = "Invalid token request"),
        (status = 401, description = "Invalid OAuth client")
    ),
    tag = "oauth",
    security(())
)]
pub async fn token(state: web::Data<AppState>, form: web::Form<TokenRequest>) -> HttpResponse {
    let req = form.into_inner();

    if req.grant_type != "authorization_code" {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "unsupported_grant_type"
        }));
    }

    let code = match &req.code {
        Some(c) => c,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "invalid_request",
                "error_description": "missing code"
            }))
        }
    };

    let client_id = match &req.client_id {
        Some(c) => c,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "invalid_request",
                "error_description": "missing client_id"
            }))
        }
    };

    let client_secret = match &req.client_secret {
        Some(c) => c,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "invalid_request",
                "error_description": "missing client_secret"
            }))
        }
    };

    let code_verifier = match &req.code_verifier {
        Some(c) => c,
        None => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "invalid_request",
                "error_description": "missing code_verifier"
            }))
        }
    };

    let core = match state.manager.active_core().await {
        Ok(c) => c,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "server_error",
                "error_description": e.to_string()
            }))
        }
    };

    // Verify client_id + client_secret
    let stored_hash: String = match core.get_oauth_client_secret_hash(client_id).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return HttpResponse::Unauthorized().json(serde_json::json!({
                "error": "invalid_client"
            }))
        }
        Err(_) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "server_error"
            }))
        }
    };

    let provided_hash = hex::encode(Sha256::digest(client_secret.as_bytes()));
    if stored_hash != provided_hash {
        return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "invalid_client"
        }));
    }

    // Look up authorization code
    let code_hash = hex::encode(Sha256::digest(code.as_bytes()));

    let code_info = match core.lookup_oauth_code(&code_hash).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "invalid authorization code"
            }))
        }
        Err(_) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "server_error"
            }))
        }
    };

    let stored_client_id = code_info.client_id;
    let code_challenge = code_info.code_challenge;
    let expires_at_str = code_info.expires_at;

    if code_info.used {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "authorization code already used"
        }));
    }

    if &stored_client_id != client_id {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "client_id mismatch"
        }));
    }

    // Check expiry
    if let Ok(expires_at) = chrono::DateTime::parse_from_rfc3339(&expires_at_str) {
        if chrono::Utc::now() > expires_at {
            return HttpResponse::BadRequest().json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "authorization code expired"
            }));
        }
    }

    // Verify PKCE: BASE64URL(SHA256(code_verifier)) == code_challenge
    let computed_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    if computed_challenge != code_challenge {
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "PKCE verification failed"
        }));
    }

    // Look up client name for the token label
    let client_name: String = core
        .get_oauth_client_name(client_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "OAuth Client".to_string());

    // Create a new Atomic API token
    let (token_info, raw_token) = match core
        .create_api_token(&format!("OAuth: {}", client_name))
        .await
    {
        Ok(t) => t,
        Err(e) => {
            return HttpResponse::InternalServerError().json(serde_json::json!({
                "error": "server_error",
                "error_description": e.to_string()
            }))
        }
    };

    // Mark code as used and store token_id for auditing
    let _ = core
        .mark_oauth_code_used(&code_hash, Some(&token_info.id))
        .await;

    HttpResponse::Ok().json(TokenResponse {
        access_token: raw_token,
        token_type: "bearer".to_string(),
    })
}

// ==================== Helpers ====================

fn redirect_with_error(redirect_uri: &str, error: &str, state: Option<&str>) -> HttpResponse {
    let mut url = format!("{}?error={}", redirect_uri, error);
    if let Some(s) = state {
        url.push_str(&format!("&state={}", s));
    }
    HttpResponse::Found()
        .insert_header(("Location", url))
        .finish()
}

fn consent_page_html(
    client_name: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    code_challenge_method: &str,
    state: &str,
) -> String {
    // HTML-escape values for safe embedding
    let client_name = html_escape(client_name);
    let client_id = html_escape(client_id);
    let redirect_uri = html_escape(redirect_uri);
    let code_challenge = html_escape(code_challenge);
    let code_challenge_method = html_escape(code_challenge_method);
    let state = html_escape(state);

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Authorize — Atomic</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ background: #1e1e1e; color: #e0e0e0; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; display: flex; justify-content: center; align-items: center; min-height: 100vh; }}
  .card {{ background: #252525; border: 1px solid #333; border-radius: 12px; padding: 2rem; max-width: 420px; width: 100%; }}
  h1 {{ font-size: 1.25rem; margin-bottom: 0.5rem; }}
  .app-name {{ color: #7c3aed; font-weight: 600; }}
  p {{ color: #999; font-size: 0.9rem; margin-bottom: 1.5rem; line-height: 1.5; }}
  .token-field {{ display: none; margin-bottom: 1rem; }}
  .token-field label {{ display: block; font-size: 0.85rem; margin-bottom: 0.4rem; color: #bbb; }}
  .token-field input {{ width: 100%; padding: 0.6rem; background: #1e1e1e; border: 1px solid #444; border-radius: 6px; color: #e0e0e0; font-size: 0.9rem; }}
  .buttons {{ display: flex; gap: 0.75rem; }}
  button {{ flex: 1; padding: 0.7rem; border: none; border-radius: 8px; font-size: 0.95rem; cursor: pointer; font-weight: 500; }}
  .approve {{ background: #7c3aed; color: white; }}
  .approve:hover {{ background: #6d28d9; }}
  .deny {{ background: #333; color: #ccc; }}
  .deny:hover {{ background: #444; }}
</style>
</head>
<body>
<div class="card">
  <h1>Authorize <span class="app-name">{client_name}</span></h1>
  <p>This application wants to access your Atomic knowledge base. It will be able to search, read, and create atoms.</p>
  <form method="POST" action="/oauth/authorize" id="authForm">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="code_challenge" value="{code_challenge}">
    <input type="hidden" name="code_challenge_method" value="{code_challenge_method}">
    <input type="hidden" name="state" value="{state}">
    <input type="hidden" name="api_token" id="apiToken" value="">
    <div class="token-field" id="tokenField">
      <label for="tokenInput">Enter your API token</label>
      <input type="password" id="tokenInput" name="api_token_input" placeholder="at_...">
    </div>
    <div class="buttons">
      <button type="submit" name="action" value="deny" class="deny">Deny</button>
      <button type="submit" name="action" value="approve" class="approve">Approve</button>
    </div>
  </form>
</div>
<script>
  // Try to get the auth token from localStorage (same key the web UI uses)
  let stored = null;
  try {{
    const config = JSON.parse(localStorage.getItem('atomic-server-config') || 'null');
    if (config && config.authToken) stored = config.authToken;
  }} catch(e) {{}}
  const tokenField = document.getElementById('tokenField');
  const apiTokenHidden = document.getElementById('apiToken');
  if (stored) {{
    apiTokenHidden.value = stored;
  }} else {{
    tokenField.style.display = 'block';
  }}
  document.getElementById('authForm').addEventListener('submit', function(e) {{
    if (!stored) {{
      apiTokenHidden.value = document.getElementById('tokenInput').value;
    }}
  }});
</script>
</body>
</html>"#
    )
}

fn error_page_html(message: &str) -> String {
    let message = html_escape(message);
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Error — Atomic</title>
<style>
  body {{ background: #1e1e1e; color: #e0e0e0; font-family: -apple-system, BlinkMacSystemFont, sans-serif; display: flex; justify-content: center; align-items: center; min-height: 100vh; }}
  .card {{ background: #252525; border: 1px solid #333; border-radius: 12px; padding: 2rem; max-width: 420px; text-align: center; }}
  .error {{ color: #ef4444; margin-bottom: 1rem; }}
  a {{ color: #7c3aed; }}
</style>
</head>
<body>
<div class="card">
  <p class="error">{message}</p>
  <p><a href="javascript:history.back()">Go back</a></p>
</div>
</body>
</html>"#
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Helper: encode bytes as hex (avoids pulling in the `hex` crate)
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, ServerEvent};
    use actix_web::test as actix_test;
    use actix_web::App;
    use reqwest::Url;
    use tokio::sync::broadcast;

    fn test_state_with_oauth() -> web::Data<AppState> {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = std::sync::Arc::new(atomic_core::DatabaseManager::new(temp.path()).unwrap());
        let (event_tx, _) = broadcast::channel::<ServerEvent>(16);
        let state = web::Data::new(AppState {
            manager,
            event_tx,
            public_url: Some("https://atomic.example.com".to_string()),
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
        state
    }

    fn test_state_without_oauth() -> web::Data<AppState> {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = std::sync::Arc::new(atomic_core::DatabaseManager::new(temp.path()).unwrap());
        let (event_tx, _) = broadcast::channel::<ServerEvent>(16);
        let state = web::Data::new(AppState {
            manager,
            event_tx,
            public_url: None,
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
        state
    }

    // ==================== Discovery ====================

    #[actix_web::test]
    async fn test_resource_metadata_returns_resource() {
        let state = test_state_with_oauth();
        let app = actix_test::init_service(App::new().app_data(state.clone()).route(
            "/.well-known/oauth-protected-resource",
            web::get().to(resource_metadata),
        ))
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/.well-known/oauth-protected-resource")
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["resource"], "https://atomic.example.com");
        assert!(body["authorization_servers"].as_array().unwrap().len() > 0);
    }

    #[actix_web::test]
    async fn test_resource_metadata_404_without_public_url() {
        let state = test_state_without_oauth();
        let app = actix_test::init_service(App::new().app_data(state.clone()).route(
            "/.well-known/oauth-protected-resource",
            web::get().to(resource_metadata),
        ))
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/.well-known/oauth-protected-resource")
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn test_auth_server_metadata_returns_endpoints() {
        let state = test_state_with_oauth();
        let app = actix_test::init_service(App::new().app_data(state.clone()).route(
            "/.well-known/oauth-authorization-server",
            web::get().to(metadata),
        ))
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/.well-known/oauth-authorization-server")
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["issuer"], "https://atomic.example.com");
        assert!(body["authorization_endpoint"]
            .as_str()
            .unwrap()
            .ends_with("/oauth/authorize"));
        assert!(body["token_endpoint"]
            .as_str()
            .unwrap()
            .ends_with("/oauth/token"));
        assert!(body["registration_endpoint"]
            .as_str()
            .unwrap()
            .ends_with("/oauth/register"));
        assert_eq!(body["code_challenge_methods_supported"][0], "S256");
    }

    // ==================== Client Registration ====================

    #[actix_web::test]
    async fn test_register_client_success() {
        let state = test_state_with_oauth();
        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/register", web::post().to(register)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/register")
            .set_json(serde_json::json!({
                "client_name": "Test Client",
                "redirect_uris": ["http://localhost:3000/callback"]
            }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["client_name"], "Test Client");
        assert!(!body["client_id"].as_str().unwrap().is_empty());
        assert!(!body["client_secret"].as_str().unwrap().is_empty());
        assert_eq!(body["redirect_uris"][0], "http://localhost:3000/callback");
        assert_eq!(body["grant_types"][0], "authorization_code");
        assert_eq!(body["token_endpoint_auth_method"], "client_secret_post");
    }

    #[actix_web::test]
    async fn test_register_client_empty_redirect_uris() {
        let state = test_state_with_oauth();
        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/register", web::post().to(register)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/register")
            .set_json(serde_json::json!({
                "client_name": "Test",
                "redirect_uris": []
            }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn test_register_client_404_without_oauth() {
        let state = test_state_without_oauth();
        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/register", web::post().to(register)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/register")
            .set_json(serde_json::json!({
                "client_name": "Test",
                "redirect_uris": ["http://localhost/cb"]
            }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    // ==================== Authorization ====================

    /// Helper: register a client and return (client_id, client_secret)
    async fn register_test_client(state: &web::Data<AppState>) -> (String, String) {
        let client_secret = "test-secret-value";
        let secret_hash = hex::encode(Sha256::digest(client_secret.as_bytes()));
        let redirect_uris = serde_json::to_string(&vec!["http://localhost:3000/callback"]).unwrap();
        let client_id = state
            .manager
            .active_core()
            .await
            .unwrap()
            .create_oauth_client("Test Client", &secret_hash, &redirect_uris)
            .await
            .unwrap();
        (client_id, client_secret.to_string())
    }

    #[actix_web::test]
    async fn test_authorize_page_returns_html() {
        let state = test_state_with_oauth();
        let (client_id, _) = register_test_client(&state).await;

        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/authorize", web::get().to(authorize_page)),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri(&format!(
                "/oauth/authorize?client_id={}&redirect_uri=http://localhost:3000/callback&response_type=code&code_challenge=test_challenge&code_challenge_method=S256&state=xyz",
                client_id
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body = String::from_utf8(actix_test::read_body(resp).await.to_vec()).unwrap();
        assert!(body.contains("Test Client"));
        assert!(body.contains("Authorize"));
        assert!(body.contains("Approve"));
        assert!(body.contains("Deny"));
    }

    #[actix_web::test]
    async fn test_authorize_deny_redirects_with_error() {
        let state = test_state_with_oauth();
        let (client_id, _) = register_test_client(&state).await;

        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/authorize", web::post().to(authorize_approve)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/authorize")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "client_id={}&redirect_uri=http://localhost:3000/callback&code_challenge=ch&code_challenge_method=S256&state=xyz&api_token=whatever&action=deny",
                client_id
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 302);

        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        assert!(location.contains("error=access_denied"));
        assert!(location.contains("state=xyz"));
    }

    #[actix_web::test]
    async fn test_authorize_approve_with_invalid_token_shows_error() {
        let state = test_state_with_oauth();
        let (client_id, _) = register_test_client(&state).await;

        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/authorize", web::post().to(authorize_approve)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/authorize")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "client_id={}&redirect_uri=http://localhost:3000/callback&code_challenge=ch&code_challenge_method=S256&api_token=bad_token&action=approve",
                client_id
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body = String::from_utf8(actix_test::read_body(resp).await.to_vec()).unwrap();
        assert!(body.contains("Invalid API token"));
    }

    #[actix_web::test]
    async fn test_authorize_approve_success_redirects_with_code() {
        let state = test_state_with_oauth();
        let (client_id, _) = register_test_client(&state).await;
        let (_, api_token) = state
            .manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("test")
            .await
            .unwrap();

        let code_verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));

        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/authorize", web::post().to(authorize_approve)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/authorize")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "client_id={}&redirect_uri=http://localhost:3000/callback&code_challenge={}&code_challenge_method=S256&state=mystate&api_token={}&action=approve",
                client_id, code_challenge, api_token
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 302);

        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        assert!(location.starts_with("http://localhost:3000/callback?code="));
        assert!(location.contains("state=mystate"));
    }

    // ==================== Token Exchange ====================

    #[actix_web::test]
    async fn test_token_unsupported_grant_type() {
        let state = test_state_with_oauth();
        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/token", web::post().to(token)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/token")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload("grant_type=refresh_token")
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["error"], "unsupported_grant_type");
    }

    #[actix_web::test]
    async fn test_token_invalid_client_secret() {
        let state = test_state_with_oauth();
        let (client_id, _) = register_test_client(&state).await;

        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/token", web::post().to(token)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/token")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "grant_type=authorization_code&code=fakecode&client_id={}&client_secret=wrong&code_verifier=v&redirect_uri=http://localhost:3000/callback",
                client_id
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["error"], "invalid_client");
    }

    #[actix_web::test]
    async fn test_full_oauth_flow() {
        let state = test_state_with_oauth();
        let (client_id, client_secret) = register_test_client(&state).await;
        let (_, api_token) = state
            .manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("test")
            .await
            .unwrap();

        // Step 1: Generate PKCE pair
        let code_verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));

        // Step 2: Approve authorization (simulates form POST)
        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/authorize", web::post().to(authorize_approve))
                .route("/oauth/token", web::post().to(token)),
        )
        .await;

        let req = actix_test::TestRequest::post()
            .uri("/oauth/authorize")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "client_id={}&redirect_uri=http://localhost:3000/callback&code_challenge={}&code_challenge_method=S256&state=s1&api_token={}&action=approve",
                client_id, code_challenge, api_token
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 302);

        // Extract the authorization code from the redirect
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        let url = Url::parse(location).unwrap();
        let auth_code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .to_string();

        // Step 3: Exchange code for token
        let req = actix_test::TestRequest::post()
            .uri("/oauth/token")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "grant_type=authorization_code&code={}&client_id={}&client_secret={}&code_verifier={}&redirect_uri=http://localhost:3000/callback",
                auth_code, client_id, client_secret, code_verifier
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["token_type"], "bearer");
        let access_token = body["access_token"].as_str().unwrap();
        assert!(access_token.starts_with("at_"));

        // Step 4: Verify the issued token works
        let verified = state
            .manager
            .active_core()
            .await
            .unwrap()
            .verify_api_token(access_token)
            .await
            .unwrap();
        assert!(verified.is_some());
        let info = verified.unwrap();
        assert!(info.name.contains("OAuth: Test Client"));
    }

    #[actix_web::test]
    async fn test_code_cannot_be_reused() {
        let state = test_state_with_oauth();
        let (client_id, client_secret) = register_test_client(&state).await;
        let (_, api_token) = state
            .manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("test")
            .await
            .unwrap();

        let code_verifier = "test-verifier-string-for-pkce-flow";
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));

        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/authorize", web::post().to(authorize_approve))
                .route("/oauth/token", web::post().to(token)),
        )
        .await;

        // Authorize
        let req = actix_test::TestRequest::post()
            .uri("/oauth/authorize")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "client_id={}&redirect_uri=http://localhost:3000/callback&code_challenge={}&code_challenge_method=S256&api_token={}&action=approve",
                client_id, code_challenge, api_token
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        let url = Url::parse(location).unwrap();
        let auth_code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .to_string();

        let token_payload = format!(
            "grant_type=authorization_code&code={}&client_id={}&client_secret={}&code_verifier={}&redirect_uri=http://localhost:3000/callback",
            auth_code, client_id, client_secret, code_verifier
        );

        // First exchange — should succeed
        let req = actix_test::TestRequest::post()
            .uri("/oauth/token")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(token_payload.clone())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        // Second exchange with same code — should fail
        let req = actix_test::TestRequest::post()
            .uri("/oauth/token")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(token_payload)
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["error"], "invalid_grant");
    }

    #[actix_web::test]
    async fn test_wrong_pkce_verifier_rejected() {
        let state = test_state_with_oauth();
        let (client_id, client_secret) = register_test_client(&state).await;
        let (_, api_token) = state
            .manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("test")
            .await
            .unwrap();

        let code_verifier = "correct-verifier";
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));

        let app = actix_test::init_service(
            App::new()
                .app_data(state.clone())
                .route("/oauth/authorize", web::post().to(authorize_approve))
                .route("/oauth/token", web::post().to(token)),
        )
        .await;

        // Authorize
        let req = actix_test::TestRequest::post()
            .uri("/oauth/authorize")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "client_id={}&redirect_uri=http://localhost:3000/callback&code_challenge={}&code_challenge_method=S256&api_token={}&action=approve",
                client_id, code_challenge, api_token
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        let url = Url::parse(location).unwrap();
        let auth_code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .to_string();

        // Exchange with wrong verifier
        let req = actix_test::TestRequest::post()
            .uri("/oauth/token")
            .insert_header(("Content-Type", "application/x-www-form-urlencoded"))
            .set_payload(format!(
                "grant_type=authorization_code&code={}&client_id={}&client_secret={}&code_verifier=wrong-verifier&redirect_uri=http://localhost:3000/callback",
                auth_code, client_id, client_secret
            ))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);

        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["error_description"], "PKCE verification failed");
    }

    // ==================== HTML Escape ====================

    #[test]
    fn test_html_escape() {
        assert_eq!(
            html_escape("<script>alert('xss')</script>"),
            "&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"
        );
        assert_eq!(html_escape("a&b\"c"), "a&amp;b&quot;c");
    }
}
