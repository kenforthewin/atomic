//! Request-shape tests for [`OpenRouterProvisioning`] against a wiremock
//! server — the one place the real HTTP client is exercised (NO REAL
//! PROVIDERS; the lifecycle tests use the recording implementation).
//!
//! Not Postgres-gated: these touch no database. They pin the auth header,
//! body fields, URL shapes, response parsing, the delete idempotency
//! contract, and the secret-hygiene property that errors never carry the
//! provisioning key or a minted key plaintext.

use atomic_cloud::{CloudError, OpenRouterProvisioning, ProvisioningApi, SecretKey};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROVISIONING_KEY: &str = "sk-or-prov-test-3f9a1c";

async fn client_against(mock: &MockServer) -> OpenRouterProvisioning {
    OpenRouterProvisioning::new(
        &format!("{}/api/v1", mock.uri()),
        SecretKey::new(PROVISIONING_KEY.to_string()),
    )
    .expect("construct client")
}

#[tokio::test]
async fn create_key_sends_the_documented_shape_and_parses_the_response() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/keys"))
        .and(header(
            "authorization",
            format!("Bearer {PROVISIONING_KEY}"),
        ))
        // The documented body: name, USD limit (50¢ → 0.5), monthly reset.
        .and(body_partial_json(serde_json::json!({
            "name": "atomic-cloud/acct-1",
            "limit": 0.5,
            "limit_reset": "monthly",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "sk-or-v1-minted-plaintext",
            "data": {
                "hash": "keyhash-abc123",
                "name": "atomic-cloud/acct-1",
                "limit": 0.5,
                "limit_reset": "monthly",
                "usage": 0,
                "disabled": false,
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let api = client_against(&mock).await;
    let created = api
        .create_key("atomic-cloud/acct-1", 50, true)
        .await
        .expect("create");
    assert_eq!(created.external_key_id, "keyhash-abc123");
    assert_eq!(created.plaintext_key.expose(), "sk-or-v1-minted-plaintext");
    // The plaintext rides in a SecretKey: Debug is redacted.
    let rendered = format!("{created:?}");
    assert!(!rendered.contains("sk-or-v1-minted-plaintext"));
    assert!(
        rendered.contains("keyhash-abc123"),
        "the id is not a secret"
    );
}

#[tokio::test]
async fn create_key_without_monthly_reset_omits_limit_reset() {
    let mock = MockServer::start().await;
    // Match the exact body: no limit_reset field at all.
    Mock::given(method("POST"))
        .and(path("/api/v1/keys"))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "name": "atomic-cloud/acct-2",
            "limit": 1.0,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "key": "sk-or-v1-other",
            "data": { "hash": "keyhash-def456" }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let api = client_against(&mock).await;
    let created = api
        .create_key("atomic-cloud/acct-2", 100, false)
        .await
        .expect("create");
    assert_eq!(created.external_key_id, "keyhash-def456");
}

#[tokio::test]
async fn update_key_limit_patches_the_key_path() {
    let mock = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/keys/keyhash-abc123"))
        .and(header(
            "authorization",
            format!("Bearer {PROVISIONING_KEY}"),
        ))
        .and(wiremock::matchers::body_json(
            serde_json::json!({ "limit": 2.5 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "hash": "keyhash-abc123", "limit": 2.5 }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let api = client_against(&mock).await;
    api.update_key_limit("keyhash-abc123", 250)
        .await
        .expect("update limit");
}

#[tokio::test]
async fn delete_key_hits_the_key_path_and_tolerates_404() {
    let mock = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/keys/keyhash-abc123"))
        .and(header(
            "authorization",
            format!("Bearer {PROVISIONING_KEY}"),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "deleted": true })),
        )
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/keys/keyhash-gone"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": { "message": "key not found" }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let api = client_against(&mock).await;
    api.delete_key("keyhash-abc123").await.expect("delete");
    // Already-gone is the cleanup paths' success condition (idempotent
    // delete contract on the trait).
    api.delete_key("keyhash-gone")
        .await
        .expect("404 must map to success");
}

#[tokio::test]
async fn get_key_usage_parses_the_data_envelope() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/keys/keyhash-abc123"))
        .and(header(
            "authorization",
            format!("Bearer {PROVISIONING_KEY}"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "hash": "keyhash-abc123",
                "usage": 0.31,
                "limit": 0.5,
                "limit_remaining": 0.19,
                "limit_reset": "monthly",
                "disabled": false,
            }
        })))
        .expect(1)
        .mount(&mock)
        .await;

    let api = client_against(&mock).await;
    let usage = api.get_key_usage("keyhash-abc123").await.expect("usage");
    assert_eq!(usage.usage_usd, 0.31);
    assert_eq!(usage.limit_usd, Some(0.5));
    assert_eq!(usage.limit_remaining_usd, Some(0.19));
    assert!(!usage.disabled);
}

#[tokio::test]
async fn provider_errors_map_typed_and_never_leak_the_provisioning_key() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/keys"))
        .respond_with(ResponseTemplate::new(402).set_body_json(serde_json::json!({
            "error": { "message": "Insufficient credits on the provisioning account" }
        })))
        .mount(&mock)
        .await;

    let api = client_against(&mock).await;
    let err = api
        .create_key("atomic-cloud/acct-3", 50, true)
        .await
        .expect_err("402 must fail");
    let CloudError::ProviderProvisioning { context, message } = &err else {
        panic!("expected ProviderProvisioning, got {err:?}");
    };
    assert_eq!(context, "creating runtime key");
    // SEC-1: the upstream body is never echoed — the message is the fixed
    // generic rejection carrying only the status, not the provider detail.
    assert!(message.contains("402"), "status surfaces: {message}");
    assert!(
        !message.contains("Insufficient credits"),
        "the provider's upstream body must not surface: {message}"
    );
    // SECRET HYGIENE: the rendered error never carries the bearer token.
    let rendered = format!("{err} / {err:?}");
    assert!(!rendered.contains(PROVISIONING_KEY), "error leaked the key");
}

#[tokio::test]
async fn create_decode_failure_withholds_the_body() {
    let mock = MockServer::start().await;
    // A 200 whose body doesn't match the documented shape but DOES contain
    // a key-like string — the error must not echo it.
    Mock::given(method("POST"))
        .and(path("/api/v1/keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "unexpected": "sk-or-v1-leaky-plaintext"
        })))
        .mount(&mock)
        .await;

    let api = client_against(&mock).await;
    let err = api
        .create_key("atomic-cloud/acct-4", 50, true)
        .await
        .expect_err("shape mismatch must fail");
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains("sk-or-v1-leaky-plaintext"),
        "decode error echoed a success body: {rendered}"
    );
}
