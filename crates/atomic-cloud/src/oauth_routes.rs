//! Cloud's per-account OAuth 2.0 flow on the tenant subdomain (plan: "Auth &
//! tenant routing" → "OAuth", "MCP token UX").
//!
//! Claude Desktop (and any MCP client) bootstraps access to a tenant's
//! knowledge base by pointing at `https://<slug>.<base>/mcp` and walking the
//! standard discovery → Dynamic Client Registration → Authorization Code +
//! PKCE → token exchange flow. This module is that flow, **reimplemented for
//! cloud** rather than reused from atomic-server.
//!
//! # Why a separate flow (not atomic-server's)
//!
//! atomic-server's OAuth handlers (`routes/oauth.rs`) store clients and codes
//! in atomic-core's *registry* and verify the approving user with a pasted
//! `api_token`. Cloud runs in Postgres mode with **no registry**
//! (`registry: None`), and its users are logged in on their subdomain via the
//! session cookie, not by pasting a token. So the storage lives in the control
//! plane ([`crate::oauth_store`], every row scoped by `account_id`) and the
//! approve step authenticates the **session**. The endpoint shapes mirror
//! atomic-server's (same JSON, S256-only, `client_secret_post`) so MCP clients
//! can't tell the difference; only the backing store and the approving
//! identity differ. The plan decision is explicit: do not extend
//! atomic-server's handlers — they stay serving self-hosted untouched.
//!
//! # Composition: a host-scoped sibling plane, NOT behind CloudAuth
//!
//! The discovery, register, and token endpoints are how a client *bootstraps*
//! — it has no token yet — so they cannot sit behind
//! [`CloudAuth`](crate::auth::CloudAuth), which 401s an un-credentialed
//! request. Instead this plane resolves the account from `Host` itself, with
//! the same rules CloudAuth uses ([`crate::auth::subdomain_from_host`] +
//! `accounts WHERE subdomain AND status='active'`). That resolution is still
//! the cross-tenant chokepoint: every [`crate::oauth_store`] query is scoped
//! by the resolved `account_id`, so a `client_id` minted under account A
//! presented on account B's subdomain resolves to nothing
//! (`invalid_client`). The endpoints are unreachable on the app host — the
//! bare base / `app.<base>` yields no subdomain label, so account resolution
//! returns `not_found` exactly as CloudAuth would.
//!
//! The authorize **approve** step (`POST /oauth/authorize`) additionally
//! requires a valid [`SESSION_COOKIE`](crate::auth::SESSION_COOKIE) for the
//! resolved account — the logged-in user on their own subdomain consenting to
//! the client. No session → the request is bounced to the app-host login page
//! with a `return_to` back to the authorize URL.
//!
//! The `/mcp` endpoint itself is the one piece that DOES sit behind CloudAuth
//! (it carries the bearer MCP token this flow mints); it is wired in
//! [`crate::server`], not here.
//!
//! # MCP token default scope (resolves the plan's open question)
//!
//! Tokens minted by this flow default to **account scope**
//! ([`TokenScope::Account`], `allowed_db_id = NULL`) — one MCP URL per
//! account, full access to its knowledge bases, matching "one account = one
//! user" in v1. The db-pinned case still works: an authorization carrying an
//! `allowed_db_id` mints a [`TokenScope::Mcp`] token pinned to that KB, and
//! CloudAuth's chokepoint enforces the pin (a db-scoped token can't read
//! another KB via header override). Per-KB-MCP-by-default is deferred.

use actix_web::http::header;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::{subdomain_from_host, SESSION_COOKIE};
use crate::control_plane::ControlPlane;
use crate::oauth_store::{
    consume_oauth_code, create_oauth_client, get_oauth_client, insert_oauth_code,
    record_oauth_code_token, NewOAuthCode, OAUTH_CODE_TTL,
};
use crate::tokens::{self, TokenScope};

/// The cloud OAuth plane as a registrable unit: construct once, clone into
/// every worker's `configure_cloud_app` call. Cheap to clone.
#[derive(Clone)]
pub struct OAuthPlane {
    state: web::Data<PlaneState>,
}

/// Everything the OAuth handlers need, shared across workers.
struct PlaneState {
    control: ControlPlane,
    /// Normalized (lowercase, no leading dot) base domain, e.g.
    /// `atomic.cloud` — or `localhost` for dev/test. Used to extract the
    /// subdomain from `Host` exactly as [`crate::auth::CloudAuth`] does.
    base_domain: String,
    /// Scheme of the public origin (`https` in production; `http` for dev
    /// deployments). The issuer/endpoint URLs in discovery and the redirect
    /// are built as `{scheme}://{request Host}` — the same origin the client
    /// already addressed, so discovery is self-consistent without a second
    /// configured hostname to drift from the routing one.
    public_scheme: String,
    /// The app host's public origin (`https://app.<base>`), where an
    /// un-logged-in `GET /oauth/authorize` sends the user to log in. No
    /// trailing slash.
    app_public_url: String,
}

impl OAuthPlane {
    /// Build the plane over the composition's control plane. `base_domain` is
    /// normalized like [`crate::auth::CloudAuth::new`]; `public_scheme` is
    /// `https` in production (`http` for local/dev); `app_public_url` is the
    /// app-host origin login redirects target (no trailing slash required).
    pub fn new(
        control: ControlPlane,
        base_domain: impl Into<String>,
        public_scheme: impl Into<String>,
        app_public_url: impl Into<String>,
    ) -> Self {
        let base_domain = base_domain
            .into()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        Self {
            state: web::Data::new(PlaneState {
                control,
                base_domain,
                public_scheme: public_scheme.into(),
                app_public_url: app_public_url.into().trim_end_matches('/').to_string(),
            }),
        }
    }

    /// Register the OAuth discovery + flow routes on `cfg`.
    ///
    /// These carry **no auth middleware**: account scoping is by `Host`
    /// inside each handler (module docs), and the bootstrap endpoints must
    /// work before any token exists. The approve step verifies the session
    /// itself. Registered without a scope prefix so the discovery URLs sit at
    /// the well-known root MCP clients probe.
    ///
    /// On the app host these routes resolve no subdomain and answer
    /// `not_found`, exactly as a tenant route would (module docs).
    ///
    /// Registered as individual exact-path [`web::resource`]s (NOT a
    /// `web::scope("")`, which would prefix-match — and thus swallow — every
    /// other route, including `/api/*`). Each resource carries the plane's
    /// app data.
    pub(crate) fn configure(&self, cfg: &mut web::ServiceConfig) {
        let state = self.state.clone();
        let resource = |path: &str| web::resource(path).app_data(state.clone());
        cfg.service(
            resource("/.well-known/oauth-authorization-server")
                .route(web::get().to(authorization_server_metadata)),
        )
        .service(
            resource("/.well-known/oauth-protected-resource")
                .route(web::get().to(protected_resource_metadata)),
        )
        .service(
            resource("/.well-known/oauth-protected-resource/mcp")
                .route(web::get().to(protected_resource_metadata)),
        )
        .service(resource("/oauth/register").route(web::post().to(register)))
        .service(
            resource("/oauth/authorize")
                .route(web::get().to(authorize_page))
                .route(web::post().to(authorize_approve)),
        )
        .service(resource("/oauth/token").route(web::post().to(token)));
    }
}

// ==================== Host → account resolution ====================

/// The account a request is scoped to, resolved from its `Host` (module
/// docs). Carries the resolved `account_id` and the issuer origin the
/// discovery/redirect URLs are built from.
struct ResolvedOAuthAccount {
    account_id: String,
    /// `{public_scheme}://{request Host}` — the tenant's own origin, no
    /// trailing slash.
    issuer: String,
}

/// Resolve `req`'s tenant account from its `Host`, or an OAuth-style denial.
///
/// Mirrors CloudAuth steps 1–2: `Host` → subdomain → `accounts WHERE
/// subdomain AND status='active'`. Anything that isn't a live tenant
/// subdomain (app host, unknown slug, non-active account) is an opaque
/// `not_found` — the same uniform answer CloudAuth gives, leaking nothing
/// about which subdomains exist or their state.
async fn resolve_account(
    state: &PlaneState,
    req: &HttpRequest,
) -> Result<ResolvedOAuthAccount, HttpResponse> {
    let host = request_host(req).ok_or_else(not_found)?;
    let subdomain = subdomain_from_host(host, &state.base_domain).ok_or_else(not_found)?;

    let account_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE subdomain = $1 AND status = 'active'")
            .bind(&subdomain)
            .fetch_optional(state.control.pool())
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "oauth account lookup failed");
                server_error()
            })?;
    let account_id = account_id.ok_or_else(not_found)?;

    Ok(ResolvedOAuthAccount {
        account_id,
        issuer: format!("{}://{host}", state.public_scheme),
    })
}

/// The host the client addressed: `Host` header, then the URI authority
/// (HTTP/2 `:authority`). Same source CloudAuth reads.
fn request_host(req: &HttpRequest) -> Option<&str> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())
}

// ==================== Discovery ====================

#[derive(Debug, Serialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
    response_types_supported: Vec<String>,
    grant_types_supported: Vec<String>,
    code_challenge_methods_supported: Vec<String>,
    token_endpoint_auth_methods_supported: Vec<String>,
}

/// `GET /.well-known/oauth-authorization-server` — per-tenant metadata
/// pointing at the tenant's own `/oauth/*` endpoints (module docs: the issuer
/// is the origin the client already addressed).
async fn authorization_server_metadata(
    state: web::Data<PlaneState>,
    req: HttpRequest,
) -> HttpResponse {
    let account = match resolve_account(&state, &req).await {
        Ok(account) => account,
        Err(denial) => return denial,
    };
    let issuer = account.issuer;
    HttpResponse::Ok().json(AuthorizationServerMetadata {
        authorization_endpoint: format!("{issuer}/oauth/authorize"),
        token_endpoint: format!("{issuer}/oauth/token"),
        registration_endpoint: format!("{issuer}/oauth/register"),
        response_types_supported: vec!["code".to_string()],
        grant_types_supported: vec!["authorization_code".to_string()],
        code_challenge_methods_supported: vec!["S256".to_string()],
        token_endpoint_auth_methods_supported: vec!["client_secret_post".to_string()],
        issuer,
    })
}

#[derive(Debug, Serialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    bearer_methods_supported: Vec<String>,
}

/// `GET /.well-known/oauth-protected-resource[/mcp]` — the protected resource
/// is the tenant's `/mcp` endpoint; the authorization server is the tenant
/// origin (module docs).
async fn protected_resource_metadata(
    state: web::Data<PlaneState>,
    req: HttpRequest,
) -> HttpResponse {
    let account = match resolve_account(&state, &req).await {
        Ok(account) => account,
        Err(denial) => return denial,
    };
    let issuer = account.issuer;
    HttpResponse::Ok().json(ProtectedResourceMetadata {
        resource: format!("{issuer}/mcp"),
        authorization_servers: vec![issuer],
        bearer_methods_supported: vec!["header".to_string()],
    })
}

// ==================== Dynamic Client Registration ====================

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    client_name: String,
    redirect_uris: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RegisterResponse {
    client_id: String,
    client_secret: String,
    client_name: String,
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    token_endpoint_auth_method: String,
}

/// `POST /oauth/register` (DCR): create an OAuth client for the host's
/// account and return `client_id` + `client_secret` (the secret once; only
/// its hash is stored). `redirect_uris` must be non-empty — the authorize and
/// token endpoints validate the presented `redirect_uri` against this set.
async fn register(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    body: Option<web::Json<RegisterRequest>>,
) -> HttpResponse {
    let account = match resolve_account(&state, &req).await {
        Ok(account) => account,
        Err(denial) => return denial,
    };
    let Some(body) = body.map(web::Json::into_inner) else {
        return bad_request(
            "invalid_client_metadata",
            "Body must be {\"client_name\": \"...\", \"redirect_uris\": [\"...\"]}.",
        );
    };
    if body.redirect_uris.is_empty() {
        return bad_request(
            "invalid_client_metadata",
            "redirect_uris must not be empty.",
        );
    }

    let (client_id, client_secret) = match create_oauth_client(
        &state.control,
        &account.account_id,
        &body.client_name,
        &body.redirect_uris,
    )
    .await
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "registering oauth client failed");
            return server_error();
        }
    };

    HttpResponse::Created().json(RegisterResponse {
        client_id,
        client_secret,
        client_name: body.client_name,
        redirect_uris: body.redirect_uris,
        grant_types: vec!["authorization_code".to_string()],
        response_types: vec!["code".to_string()],
        token_endpoint_auth_method: "client_secret_post".to_string(),
    })
}

// ==================== Authorization ====================

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    client_id: String,
    redirect_uri: String,
    response_type: String,
    code_challenge: String,
    code_challenge_method: String,
    state: Option<String>,
}

/// `GET /oauth/authorize`: validate the request, then either render the
/// consent page (logged-in user) or bounce to the app-host login page.
///
/// Validation order matters for where errors land: a bad `client_id` /
/// `redirect_uri` can't be redirected back safely (we can't trust an
/// unverified `redirect_uri`), so those are direct 400s; an otherwise-valid
/// request with an unsupported `response_type` / challenge method redirects
/// the OAuth error to the (now-validated) `redirect_uri`.
async fn authorize_page(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    query: web::Query<AuthorizeQuery>,
) -> HttpResponse {
    let account = match resolve_account(&state, &req).await {
        Ok(account) => account,
        Err(denial) => return denial,
    };
    let q = query.into_inner();

    // Validate the client + redirect_uri FIRST (account-scoped): only once we
    // trust the redirect_uri can we safely bounce OAuth errors to it.
    let client = match get_oauth_client(&state.control, &account.account_id, &q.client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => return bad_request("invalid_client", "Unknown client_id for this account."),
        Err(e) => {
            tracing::error!(error = %e, "oauth client lookup failed");
            return server_error();
        }
    };
    if !client.redirect_uris.contains(&q.redirect_uri) {
        return bad_request(
            "invalid_request",
            "redirect_uri is not registered for this client.",
        );
    }

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

    // Logged-in? The approving identity is the session on THIS subdomain.
    if !session_valid(&state, &account.account_id, &req).await {
        return login_redirect(&state, &account.issuer, &req);
    }

    let html = consent_page_html(
        &client.client_name,
        &q.client_id,
        &q.redirect_uri,
        &q.code_challenge,
        &q.code_challenge_method,
        q.state.as_deref().unwrap_or(""),
    );
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(html)
}

#[derive(Debug, Deserialize)]
struct AuthorizeApproveForm {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    code_challenge_method: String,
    state: Option<String>,
    action: String,
}

/// `POST /oauth/authorize` (approve): require a valid session for this
/// account, re-validate the client + redirect_uri, mint an authorization code
/// (account-scope MCP by default — module docs), and 302 to the redirect.
async fn authorize_approve(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    form: Option<web::Form<AuthorizeApproveForm>>,
) -> HttpResponse {
    let account = match resolve_account(&state, &req).await {
        Ok(account) => account,
        Err(denial) => return denial,
    };
    let Some(f) = form.map(web::Form::into_inner) else {
        return bad_request("invalid_request", "Malformed approval form.");
    };

    // Re-validate the client + redirect_uri (account-scoped) before honoring
    // anything — the GET's validation is advisory; the POST is authoritative.
    let client = match get_oauth_client(&state.control, &account.account_id, &f.client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => return bad_request("invalid_client", "Unknown client_id for this account."),
        Err(e) => {
            tracing::error!(error = %e, "oauth client lookup failed");
            return server_error();
        }
    };
    if !client.redirect_uris.contains(&f.redirect_uri) {
        return bad_request(
            "invalid_request",
            "redirect_uri is not registered for this client.",
        );
    }

    // The approving identity: a valid session for THIS account. A cross-tenant
    // cookie (the `.base` cookie rides every subdomain) verifies nothing here
    // — `verify_session` is account-scoped. No session → no code is minted;
    // bounce to login.
    if !session_valid(&state, &account.account_id, &req).await {
        return login_redirect(&state, &account.issuer, &req);
    }

    if f.action != "approve" {
        return redirect_with_error(&f.redirect_uri, "access_denied", f.state.as_deref());
    }
    if f.code_challenge_method != "S256" {
        return redirect_with_error(&f.redirect_uri, "invalid_request", f.state.as_deref());
    }

    // Mint the authorization code bound to client + challenge + redirect_uri,
    // carrying the default MCP authorization scope (account-scope; module
    // docs). allowed_db_id is NULL — the issued token is account-scope.
    let code = match insert_oauth_code(
        &state.control,
        NewOAuthCode {
            account_id: &account.account_id,
            client_id: &f.client_id,
            code_challenge: &f.code_challenge,
            code_challenge_method: &f.code_challenge_method,
            redirect_uri: &f.redirect_uri,
            scope: TokenScope::Mcp,
            allowed_db_id: None,
        },
        OAUTH_CODE_TTL,
    )
    .await
    {
        Ok(code) => code,
        Err(e) => {
            tracing::error!(error = %e, "minting oauth code failed");
            return server_error();
        }
    };

    let mut location = format!("{}?code={}", f.redirect_uri, code);
    if let Some(s) = &f.state {
        location.push_str(&format!("&state={}", urlencode(s)));
    }
    HttpResponse::Found()
        .insert_header((header::LOCATION, location))
        .finish()
}

// ==================== Token Exchange ====================

#[derive(Debug, Deserialize)]
struct TokenRequest {
    grant_type: String,
    code: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
}

/// `POST /oauth/token`: verify the client secret, consume the authorization
/// code (single-use, unexpired, account-scoped), verify PKCE S256 and the
/// `redirect_uri` match, then mint the MCP-scoped cloud token. Every failure
/// is the OAuth-standard error JSON.
async fn token(
    state: web::Data<PlaneState>,
    req: HttpRequest,
    form: Option<web::Form<TokenRequest>>,
) -> HttpResponse {
    let account = match resolve_account(&state, &req).await {
        Ok(account) => account,
        Err(denial) => return denial,
    };
    let Some(r) = form.map(web::Form::into_inner) else {
        return token_error("invalid_request", "Malformed token request.");
    };

    if r.grant_type != "authorization_code" {
        return token_error(
            "unsupported_grant_type",
            "Only authorization_code is supported.",
        );
    }
    let (Some(code), Some(client_id), Some(client_secret), Some(code_verifier), Some(redirect_uri)) = (
        r.code.as_deref(),
        r.client_id.as_deref(),
        r.client_secret.as_deref(),
        r.code_verifier.as_deref(),
        r.redirect_uri.as_deref(),
    ) else {
        return token_error(
            "invalid_request",
            "code, client_id, client_secret, code_verifier, and redirect_uri are required.",
        );
    };

    // Verify the client + secret (account-scoped). An unknown client_id — or
    // one minted under another account presented here — is invalid_client,
    // the same cross-tenant chokepoint as everywhere else.
    let client = match get_oauth_client(&state.control, &account.account_id, client_id).await {
        Ok(Some(client)) => client,
        Ok(None) => return client_error(),
        Err(e) => {
            tracing::error!(error = %e, "oauth client lookup failed");
            return server_error();
        }
    };
    if client.client_secret_hash != sha256_hex(client_secret) {
        return client_error();
    }

    // Consume the code: single-use + unexpired + account-scoped, all in the
    // one UPDATE inside `consume_oauth_code`. A replayed, expired, or
    // cross-tenant code consumes nothing → invalid_grant.
    let consumed = match consume_oauth_code(&state.control, &account.account_id, code).await {
        Ok(Some(consumed)) => consumed,
        Ok(None) => return token_error("invalid_grant", "Invalid, expired, or used code."),
        Err(e) => {
            tracing::error!(error = %e, "consuming oauth code failed");
            return server_error();
        }
    };

    // Bindings: the code is bound to a client_id and a redirect_uri at
    // issuance; both must match the exchange request.
    if consumed.client_id != client_id {
        return token_error("invalid_grant", "Code was issued to a different client.");
    }
    if consumed.redirect_uri != redirect_uri {
        return token_error(
            "invalid_grant",
            "redirect_uri does not match the authorization.",
        );
    }

    // PKCE S256: BASE64URL(SHA256(code_verifier)) must equal the stored
    // challenge. We only ever issue S256 challenges, so a non-S256 method on
    // the consumed code is corruption — reject rather than fall through to a
    // weaker check.
    if consumed.code_challenge_method != "S256" {
        return token_error("invalid_grant", "Unsupported code_challenge_method.");
    }
    let computed = data_encoding::BASE64URL_NOPAD.encode(&Sha256::digest(code_verifier.as_bytes()));
    if computed != consumed.code_challenge {
        return token_error("invalid_grant", "PKCE verification failed.");
    }

    // Mint the token with the scope/pin the code carried (account-scope MCP by
    // default; a db-pinned authorization carries TokenScope::Mcp +
    // allowed_db_id, which CloudAuth's chokepoint then enforces — module docs).
    let raw = match tokens::issue_token(
        &state.control,
        &account.account_id,
        consumed.scope,
        consumed.allowed_db_id.as_deref(),
        &format!("mcp-oauth: {}", client.client_name),
    )
    .await
    {
        Ok(raw) => raw,
        Err(e) => {
            tracing::error!(error = %e, "issuing mcp token failed");
            return server_error();
        }
    };

    // Best-effort forensic link from the spent code to the token it minted.
    // The token id is its SHA-256 hash (the cloud_tokens primary key).
    if let Err(e) = record_oauth_code_token(
        &state.control,
        &account.account_id,
        &consumed.code_hash,
        &sha256_hex(&raw),
    )
    .await
    {
        tracing::warn!(error = %e, "failed to record oauth code -> token link");
    }

    HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(TokenResponse {
            access_token: raw,
            token_type: "Bearer".to_string(),
        })
}

// ==================== Session check ====================

/// Whether `req` carries a session cookie that verifies for `account_id`.
/// Account-scoped (the cross-tenant chokepoint): a session for another
/// account on the shared `.base` cookie verifies nothing here.
async fn session_valid(state: &PlaneState, account_id: &str, req: &HttpRequest) -> bool {
    let Some(cookie) = req.cookie(SESSION_COOKIE) else {
        return false;
    };
    match tokens::verify_session(&state.control, account_id, cookie.value()).await {
        Ok(record) => record.is_some(),
        Err(e) => {
            tracing::error!(error = %e, "oauth session verification failed");
            false
        }
    }
}

// ==================== Helpers ====================

/// SHA-256 hex of a plaintext — the same digest [`crate::tokens`] /
/// [`crate::oauth_store`] persist. Used to verify the presented client secret
/// against the stored hash and to derive a token's id from its plaintext.
fn sha256_hex(plaintext: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(plaintext.as_bytes()))
}

/// Redirect the user to the app-host login page, with a `return_to` back to
/// the full authorize URL so the post-login flow resumes the consent.
fn login_redirect(state: &PlaneState, issuer: &str, req: &HttpRequest) -> HttpResponse {
    let authorize_url = format!(
        "{issuer}{}{}",
        req.path(),
        req.query_string()
            .is_empty()
            .then(String::new)
            .unwrap_or_else(|| format!("?{}", req.query_string())),
    );
    let location = format!(
        "{}/login?return_to={}",
        state.app_public_url,
        urlencode(&authorize_url)
    );
    HttpResponse::Found()
        .insert_header((header::LOCATION, location))
        .finish()
}

/// Redirect an OAuth error back to the (validated) `redirect_uri`.
fn redirect_with_error(redirect_uri: &str, error: &str, state: Option<&str>) -> HttpResponse {
    let mut url = format!("{redirect_uri}?error={error}");
    if let Some(s) = state {
        url.push_str(&format!("&state={}", urlencode(s)));
    }
    HttpResponse::Found()
        .insert_header((header::LOCATION, url))
        .finish()
}

/// Minimal application/x-www-form-urlencoded component encoding for the bits
/// we place into redirect query strings (`state`, a `return_to` URL). Encodes
/// everything outside the RFC 3986 unreserved set — conservative but correct.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// --- Denial responses -------------------------------------------------------

/// Uniform 404 for a request that isn't a live tenant subdomain (module
/// docs): app host, unknown slug, or non-active account — indistinguishable,
/// like CloudAuth's.
fn not_found() -> HttpResponse {
    HttpResponse::NotFound().json(serde_json::json!({ "error": "not_found" }))
}

/// Generic 400 with an OAuth-style `error`/`error_description` body.
fn bad_request(error: &str, description: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(serde_json::json!({
        "error": error,
        "error_description": description,
    }))
}

/// 400 for the token endpoint's request-level failures (RFC 6749 §5.2 uses
/// 400 for `invalid_grant`/`invalid_request`/`unsupported_grant_type`).
fn token_error(error: &str, description: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(serde_json::json!({
        "error": error,
        "error_description": description,
    }))
}

/// 401 `invalid_client` (RFC 6749 §5.2): bad/unknown client_id or secret.
fn client_error() -> HttpResponse {
    HttpResponse::Unauthorized().json(serde_json::json!({ "error": "invalid_client" }))
}

fn server_error() -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({
        "error": "server_error",
        "error_description": "Something went wrong; try again.",
    }))
}

/// The minimal server-rendered consent page (plan: "the OAuth consent/login
/// UI is API + a minimal server-rendered approve form only, no SPA"). All
/// dynamic values are HTML-escaped before embedding. The approve/deny buttons
/// POST the hidden authorization parameters back to `/oauth/authorize`; the
/// session cookie rides along and is what authenticates the approval.
fn consent_page_html(
    client_name: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    code_challenge_method: &str,
    state: &str,
) -> String {
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
  <form method="POST" action="/oauth/authorize">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="code_challenge" value="{code_challenge}">
    <input type="hidden" name="code_challenge_method" value="{code_challenge_method}">
    <input type="hidden" name="state" value="{state}">
    <div class="buttons">
      <button type="submit" name="action" value="deny" class="deny">Deny</button>
      <button type="submit" name="action" value="approve" class="approve">Approve</button>
    </div>
  </form>
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc_7636_fixture() {
        // RFC 7636 Appendix B: the canonical verifier/challenge pair.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let computed = data_encoding::BASE64URL_NOPAD.encode(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(computed, challenge, "S256 of the fixture verifier");

        // A different verifier must not collide with the fixture challenge.
        let wrong = data_encoding::BASE64URL_NOPAD.encode(&Sha256::digest(b"not-the-verifier"));
        assert_ne!(wrong, challenge);
    }

    #[test]
    fn sha256_hex_is_lowercase_64_hex() {
        let hex = sha256_hex("some-client-secret");
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        // Stable: the same input always hashes the same.
        assert_eq!(hex, sha256_hex("some-client-secret"));
    }

    #[test]
    fn urlencode_escapes_reserved_preserves_unreserved() {
        assert_eq!(urlencode("abcXYZ-_.~09"), "abcXYZ-_.~09");
        assert_eq!(
            urlencode("https://claude.ai/cb?a=b&c=d"),
            "https%3A%2F%2Fclaude.ai%2Fcb%3Fa%3Db%26c%3Dd"
        );
        assert_eq!(urlencode("a b"), "a%20b");
    }

    #[test]
    fn html_escape_neutralizes_markup() {
        assert_eq!(
            html_escape("<script>alert('x')</script>"),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"
        );
    }
}
