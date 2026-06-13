//! End-to-end tests for the OAuth Dynamic Client Registration + Authorization
//! Code + PKCE flow.
//!
//! Endpoints exercised (all public, no bearer):
//!
//! - `POST /oauth/register` — DCR; issues `client_id` + `client_secret`.
//! - `POST /oauth/authorize` — submits the consent form with the user's
//!   API token; on approve it 302-redirects with the authorization `code`.
//! - `POST /oauth/token` — exchanges (`code`, `code_verifier`,
//!   `client_secret`) for a bearer token. The bearer must then work
//!   against `/api/atoms`.
//!
//! These endpoints early-return 404 when `state.public_url` is unset, so
//! the suite spins up a `TestCtx` with `public_url: Some(<server URL>)`
//! and serves the full production route table via `spawn_live_server`.

mod support;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::redirect::Policy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::{spawn_live_server, Backend, TestCtx, TestCtxOptions};

// ==================== PKCE helpers ====================

/// Generate a fresh PKCE code_verifier (S256 challenge applied below).
/// 64 base64url chars sits well above the spec minimum (43); 96 random
/// bytes encoded gives stable length without padding.
fn random_verifier() -> String {
    let mut buf = [0u8; 64];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

fn s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Spin up a TestCtx with `public_url` populated so the OAuth handlers
/// stop early-returning 404, then serve the full route table.
async fn boot(backend: Backend) -> Option<(TestCtx, support::LiveServer)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let addr = listener.local_addr().ok()?;
    drop(listener);
    let public_url = format!("http://{}", addr);
    let opts = TestCtxOptions {
        public_url: Some(public_url),
        ..Default::default()
    };
    let ctx = TestCtx::new_with(backend, opts).await?;
    let server = spawn_live_server(&ctx).await;
    Some((ctx, server))
}

// ==================== O1. DCR ====================

#[actix_web::test]
async fn oauth_dcr_issues_client_credentials_sqlite() {
    run_oauth_dcr_issues_client_credentials(Backend::Sqlite).await;
}

#[actix_web::test]
async fn oauth_dcr_issues_client_credentials_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("oauth_dcr_issues_client_credentials_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_oauth_dcr_issues_client_credentials(Backend::Postgres).await;
}

async fn run_oauth_dcr_issues_client_credentials(backend: Backend) {
    let Some((_ctx, server)) = boot(backend).await else {
        return;
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/oauth/register", server.base_url))
        .json(&json!({
            "client_name": "Test Client",
            "redirect_uris": ["http://localhost:9876/callback"],
        }))
        .send()
        .await
        .expect("register");
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.expect("parse register");
    assert!(body["client_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .is_some());
    assert!(body["client_secret"]
        .as_str()
        .filter(|s| !s.is_empty())
        .is_some());

    server.stop().await;
}

// ==================== O2. Full flow ====================

#[actix_web::test]
async fn oauth_authorize_then_token_with_pkce_sqlite() {
    run_oauth_authorize_then_token_with_pkce(Backend::Sqlite).await;
}

#[actix_web::test]
async fn oauth_authorize_then_token_with_pkce_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("oauth_authorize_then_token_with_pkce_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_oauth_authorize_then_token_with_pkce(Backend::Postgres).await;
}

async fn run_oauth_authorize_then_token_with_pkce(backend: Backend) {
    let Some((ctx, server)) = boot(backend).await else {
        return;
    };
    // Do not follow redirects — the authorize POST returns a 302 whose
    // Location header carries the authorization code.
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");

    // 1. Register a client.
    let reg: Value = client
        .post(format!("{}/oauth/register", server.base_url))
        .json(&json!({
            "client_name": "PKCE Tester",
            "redirect_uris": ["http://localhost:9876/callback"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = reg["client_id"].as_str().unwrap().to_string();
    let client_secret = reg["client_secret"].as_str().unwrap().to_string();

    // 2. Generate PKCE pair + drive the consent POST.
    let verifier = random_verifier();
    let challenge = s256_challenge(&verifier);
    let redirect_uri = "http://localhost:9876/callback";

    let form = [
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect_uri),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", "xyz"),
        ("api_token", ctx.token.as_str()),
        ("action", "approve"),
    ];
    let resp = client
        .post(format!("{}/oauth/authorize", server.base_url))
        .form(&form)
        .send()
        .await
        .expect("authorize approve");
    assert_eq!(resp.status().as_u16(), 302);
    let location = resp
        .headers()
        .get("Location")
        .expect("Location header")
        .to_str()
        .expect("location utf8")
        .to_string();
    let code = location
        .split_once("code=")
        .and_then(|(_, tail)| tail.split('&').next())
        .expect("code in redirect")
        .to_string();
    assert!(!code.is_empty());

    // 3. Exchange the code at /oauth/token.
    let token_form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", redirect_uri),
    ];
    let resp = client
        .post(format!("{}/oauth/token", server.base_url))
        .form(&token_form)
        .send()
        .await
        .expect("token exchange");
    assert!(
        resp.status().is_success(),
        "token exchange must succeed, got {}",
        resp.status()
    );
    let body: Value = resp.json().await.expect("parse token");
    let access_token = body["access_token"].as_str().expect("access_token");

    // 4. The new bearer must work against /api.
    let resp = client
        .get(format!("{}/api/atoms", server.base_url))
        .bearer_auth(access_token)
        .send()
        .await
        .expect("api atoms with new bearer");
    assert_eq!(resp.status(), 200);

    server.stop().await;
}

// ==================== O3. Invalid verifier ====================

#[actix_web::test]
async fn oauth_token_rejects_invalid_verifier_sqlite() {
    run_oauth_token_rejects_invalid_verifier(Backend::Sqlite).await;
}

#[actix_web::test]
async fn oauth_token_rejects_invalid_verifier_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("oauth_token_rejects_invalid_verifier_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_oauth_token_rejects_invalid_verifier(Backend::Postgres).await;
}

async fn run_oauth_token_rejects_invalid_verifier(backend: Backend) {
    let Some((ctx, server)) = boot(backend).await else {
        return;
    };
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");

    let reg: Value = client
        .post(format!("{}/oauth/register", server.base_url))
        .json(&json!({
            "client_name": "PKCE NegTester",
            "redirect_uris": ["http://localhost:9876/callback"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = reg["client_id"].as_str().unwrap().to_string();
    let client_secret = reg["client_secret"].as_str().unwrap().to_string();

    let verifier = random_verifier();
    let challenge = s256_challenge(&verifier);
    let redirect_uri = "http://localhost:9876/callback";

    let approve_form = [
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect_uri),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", ""),
        ("api_token", ctx.token.as_str()),
        ("action", "approve"),
    ];
    let resp = client
        .post(format!("{}/oauth/authorize", server.base_url))
        .form(&approve_form)
        .send()
        .await
        .unwrap();
    let location = resp
        .headers()
        .get("Location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let code = location
        .split_once("code=")
        .and_then(|(_, t)| t.split('&').next())
        .unwrap()
        .to_string();

    // Submit a deliberately wrong code_verifier — the token endpoint must
    // reject with `invalid_grant`.
    let bad_verifier = random_verifier();
    let token_form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("code_verifier", bad_verifier.as_str()),
        ("redirect_uri", redirect_uri),
    ];
    let resp = client
        .post(format!("{}/oauth/token", server.base_url))
        .form(&token_form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "wrong PKCE verifier must produce 400 invalid_grant"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant");

    server.stop().await;
}
