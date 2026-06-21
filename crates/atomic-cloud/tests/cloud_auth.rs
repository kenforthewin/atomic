//! CloudAuth middleware integration tests: subdomain routing, token and
//! session verification, the cross-tenant chokepoint, account-status
//! handling, and the `allowed_db_id` database-scope chokepoint.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. The harness mounts a tiny echo route
//! under [`CloudAuth`] — the full composed cloud server gets its own e2e
//! suite in the next slice.

mod support;

use std::sync::Arc;

use actix_web::http::header;
use actix_web::{test as actix_test, web, App, HttpMessage, HttpRequest, HttpResponse};
use atomic_cloud::{
    create_session, issue_token, provision_account, AccountCache, AccountCacheConfig, CloudAuth,
    ClusterConfig, ControlPlane, ManagedKeys, NewAccount, ProvisionedAccount, ResolvedTenant,
    TokenScope, SESSION_COOKIE,
};
use atomic_core::DatabaseManager;
use atomic_server::db_extractor::{resolve_core, RequestDatabaseManager};
use atomic_server::event_channel::RequestEventChannel;
use support::with_control_db;

/// Base domain the middleware is configured with; accounts are addressed as
/// `<subdomain>.atomic.test`.
const BASE_DOMAIN: &str = "atomic.test";

/// Migrated control plane + a cluster config pointing at the test cluster.
async fn setup(control_url: &str) -> (ControlPlane, ClusterConfig) {
    let control = ControlPlane::connect(
        control_url,
        atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
    )
    .await
    .expect("connect control plane");
    control.initialize().await.expect("migrate control plane");
    let cluster = ClusterConfig {
        cluster_id: "test-cluster-1".to_string(),
        cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
            .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
    };
    (control, cluster)
}

async fn provision(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    subdomain: &str,
) -> ProvisionedAccount {
    provision_account(
        control,
        cluster,
        &ManagedKeys::Disabled,
        NewAccount {
            email: format!("{subdomain}@example.com"),
            subdomain: subdomain.to_string(),
        },
    )
    .await
    .expect("provision account")
}

/// What a request looks like *after* the middleware: the installed
/// extensions and the (possibly injected) database-selection header.
async fn echo(req: HttpRequest) -> HttpResponse {
    let (tenant, manager, has_event_channel) = {
        let ext = req.extensions();
        (
            ext.get::<ResolvedTenant>().cloned(),
            ext.get::<RequestDatabaseManager>()
                .map(|m| Arc::clone(&m.0)),
            ext.get::<RequestEventChannel>().is_some(),
        )
    };
    let tenant = tenant.expect("CloudAuth must install ResolvedTenant");
    let manager = manager.expect("CloudAuth must install RequestDatabaseManager");
    // Resolve through atomic-server's real selection rules to prove the
    // injected manager serves whatever database this request addresses.
    let resolves = resolve_core(&manager, &req).await.is_ok();
    let db_header = req
        .headers()
        .get("X-Atomic-Database")
        .and_then(|v| v.to_str().ok());
    HttpResponse::Ok().json(serde_json::json!({
        "account_id": tenant.principal.account_id,
        "scope": tenant.principal.scope.as_str(),
        "source": tenant.principal.source.as_str(),
        "subdomain": tenant.subdomain,
        "db_header": db_header,
        "has_event_channel": has_event_channel,
        "resolves": resolves,
    }))
}

/// `GET /echo` guarded by [`CloudAuth`], with a fresh default-config cache.
fn echo_service(
    control: &ControlPlane,
    cluster: &ClusterConfig,
) -> impl actix_web::dev::HttpServiceFactory + 'static {
    let cache = Arc::new(AccountCache::new(
        control.clone(),
        cluster.clone(),
        support::test_vault(),
        AccountCacheConfig::default(),
    ));
    web::resource("/echo")
        .wrap(CloudAuth::new(control.clone(), cache, BASE_DOMAIN))
        .route(web::get().to(echo))
}

fn get(host: &str) -> actix_test::TestRequest {
    actix_test::TestRequest::get()
        .uri("/echo")
        .insert_header((header::HOST, host))
}

fn bearer(req: actix_test::TestRequest, token: &str) -> actix_test::TestRequest {
    req.insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
}

fn with_session(req: actix_test::TestRequest, secret: &str) -> actix_test::TestRequest {
    req.cookie(actix_web::cookie::Cookie::new(SESSION_COOKIE, secret))
}

#[tokio::test]
async fn token_and_session_happy_paths() {
    with_control_db("token_and_session_happy_paths", |url| async move {
        let (control, cluster) = setup(&url).await;
        let acct = provision(&control, &cluster, "kenny").await;
        let app =
            actix_test::init_service(App::new().service(echo_service(&control, &cluster))).await;

        let token = issue_token(&control, &acct.account_id, TokenScope::Account, None, "t")
            .await
            .expect("issue token");
        let resp =
            actix_test::call_service(&app, bearer(get("kenny.atomic.test"), &token).to_request())
                .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["account_id"], acct.account_id);
        assert_eq!(body["scope"], "account");
        assert_eq!(body["source"], "token");
        assert_eq!(body["subdomain"], "kenny");
        assert_eq!(body["has_event_channel"], true);
        assert_eq!(body["resolves"], true);
        assert!(
            body["db_header"].is_null(),
            "account-scoped credential must not pin a database"
        );

        let session = create_session(
            &control,
            &acct.account_id,
            std::time::Duration::from_secs(3600),
            None,
            None,
        )
        .await
        .expect("create session");
        let resp = actix_test::call_service(
            &app,
            with_session(get("kenny.atomic.test"), &session).to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["source"], "session");
        assert_eq!(body["scope"], "account");

        // No credential at all → 401.
        let resp = actix_test::call_service(&app, get("kenny.atomic.test").to_request()).await;
        assert_eq!(resp.status(), 401);
    })
    .await;
}

#[tokio::test]
async fn cross_tenant_credentials_rejected() {
    with_control_db("cross_tenant_credentials_rejected", |url| async move {
        let (control, cluster) = setup(&url).await;
        let alpha = provision(&control, &cluster, "alpha").await;
        let _bravo = provision(&control, &cluster, "bravo").await;
        let app =
            actix_test::init_service(App::new().service(echo_service(&control, &cluster))).await;

        let token = issue_token(&control, &alpha.account_id, TokenScope::Account, None, "t")
            .await
            .expect("issue token");
        let session = create_session(
            &control,
            &alpha.account_id,
            std::time::Duration::from_secs(3600),
            None,
            None,
        )
        .await
        .expect("create session");

        // Alpha's credentials work on alpha's subdomain…
        let resp =
            actix_test::call_service(&app, bearer(get("alpha.atomic.test"), &token).to_request())
                .await;
        assert_eq!(resp.status(), 200);

        // …and verify nothing on bravo's. This is the cross-tenant
        // chokepoint (decision 2026-06-09): the session cookie in
        // particular crosses subdomains by design, so only the
        // account-scoped verification stands between tenants.
        let resp =
            actix_test::call_service(&app, bearer(get("bravo.atomic.test"), &token).to_request())
                .await;
        assert_eq!(resp.status(), 401, "token must not cross tenants");
        let resp = actix_test::call_service(
            &app,
            with_session(get("bravo.atomic.test"), &session).to_request(),
        )
        .await;
        assert_eq!(resp.status(), 401, "session must not cross tenants");
    })
    .await;
}

#[tokio::test]
async fn unknown_and_malformed_hosts_are_404() {
    with_control_db("unknown_and_malformed_hosts_are_404", |url| async move {
        let (control, cluster) = setup(&url).await;
        let acct = provision(&control, &cluster, "kenny").await;
        let token = issue_token(&control, &acct.account_id, TokenScope::Account, None, "t")
            .await
            .expect("issue token");
        let app =
            actix_test::init_service(App::new().service(echo_service(&control, &cluster))).await;

        // Even with a valid credential attached, routing fails first.
        for host in [
            "ghost.atomic.test", // no such account
            "atomic.test",       // bare base domain
            "a.b.atomic.test",   // nested labels
            "kenny.example.com", // foreign domain
            "kennyatomic.test",  // lookalike suffix
        ] {
            let resp = actix_test::call_service(&app, bearer(get(host), &token).to_request()).await;
            assert_eq!(resp.status(), 404, "host {host:?} must 404");
            let body: serde_json::Value = actix_test::read_body_json(resp).await;
            assert_eq!(body["error"], "not_found");
        }

        // No Host header at all (and no URI authority) → 404.
        let resp = actix_test::call_service(
            &app,
            bearer(actix_test::TestRequest::get().uri("/echo"), &token).to_request(),
        )
        .await;
        assert_eq!(resp.status(), 404);
    })
    .await;
}

#[tokio::test]
async fn revoked_and_expired_credentials_rejected() {
    with_control_db(
        "revoked_and_expired_credentials_rejected",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let acct = provision(&control, &cluster, "kenny").await;
            let app =
                actix_test::init_service(App::new().service(echo_service(&control, &cluster)))
                    .await;

            // Revoked token.
            let revoked = issue_token(&control, &acct.account_id, TokenScope::Account, None, "r")
                .await
                .expect("issue token");
            sqlx::query("UPDATE cloud_tokens SET revoked_at = NOW() WHERE account_id = $1")
                .bind(&acct.account_id)
                .execute(control.pool())
                .await
                .expect("revoke token");
            let resp = actix_test::call_service(
                &app,
                bearer(get("kenny.atomic.test"), &revoked).to_request(),
            )
            .await;
            assert_eq!(resp.status(), 401, "revoked token must 401");

            // Expired token.
            let expired = issue_token(&control, &acct.account_id, TokenScope::Account, None, "e")
                .await
                .expect("issue token");
            sqlx::query(
                "UPDATE cloud_tokens SET expires_at = NOW() - INTERVAL '1 hour' \
             WHERE account_id = $1 AND revoked_at IS NULL",
            )
            .bind(&acct.account_id)
            .execute(control.pool())
            .await
            .expect("expire token");
            let resp = actix_test::call_service(
                &app,
                bearer(get("kenny.atomic.test"), &expired).to_request(),
            )
            .await;
            assert_eq!(resp.status(), 401, "expired token must 401");

            // Expired session.
            let session = create_session(
                &control,
                &acct.account_id,
                std::time::Duration::from_secs(3600),
                None,
                None,
            )
            .await
            .expect("create session");
            sqlx::query(
                "UPDATE sessions SET expires_at = NOW() - INTERVAL '1 hour' WHERE account_id = $1",
            )
            .bind(&acct.account_id)
            .execute(control.pool())
            .await
            .expect("expire session");
            let resp = actix_test::call_service(
                &app,
                with_session(get("kenny.atomic.test"), &session).to_request(),
            )
            .await;
            assert_eq!(resp.status(), 401, "expired session must 401");

            // Garbage credentials never verify.
            let resp = actix_test::call_service(
                &app,
                bearer(get("kenny.atomic.test"), "atm_not_a_real_token").to_request(),
            )
            .await;
            assert_eq!(resp.status(), 401);
        },
    )
    .await;
}

#[tokio::test]
async fn non_active_account_blocked() {
    with_control_db("non_active_account_blocked", |url| async move {
        let (control, cluster) = setup(&url).await;
        let acct = provision(&control, &cluster, "limbo").await;
        let token = issue_token(&control, &acct.account_id, TokenScope::Account, None, "t")
            .await
            .expect("issue token");
        let app =
            actix_test::init_service(App::new().service(echo_service(&control, &cluster))).await;

        let set_status = |status: &'static str| {
            let control = control.clone();
            let id = acct.account_id.clone();
            async move {
                sqlx::query("UPDATE accounts SET status = $1 WHERE id = $2")
                    .bind(status)
                    .bind(&id)
                    .execute(control.pool())
                    .await
                    .expect("set account status");
            }
        };

        // Provisioning: structured 503 hold message, even with a valid
        // credential (status is checked before credentials are read).
        set_status("provisioning").await;
        let resp =
            actix_test::call_service(&app, bearer(get("limbo.atomic.test"), &token).to_request())
                .await;
        assert_eq!(resp.status(), 503);
        assert_eq!(
            resp.headers()
                .get(header::RETRY_AFTER)
                .map(|v| v.as_bytes()),
            Some("60".as_bytes())
        );
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["error"], "account_provisioning");
        assert_eq!(body["retry_after_seconds"], 60);

        // Failed (reaper rolled it back): indistinguishable from absent.
        set_status("failed").await;
        let resp =
            actix_test::call_service(&app, bearer(get("limbo.atomic.test"), &token).to_request())
                .await;
        assert_eq!(resp.status(), 404);

        // Back to active: serves again.
        set_status("active").await;
        let resp =
            actix_test::call_service(&app, bearer(get("limbo.atomic.test"), &token).to_request())
                .await;
        assert_eq!(resp.status(), 200);
    })
    .await;
}

#[tokio::test]
async fn allowed_db_id_chokepoint() {
    with_control_db("allowed_db_id_chokepoint", |url| async move {
        let (control, cluster) = setup(&url).await;
        let acct = provision(&control, &cluster, "scoped").await;

        // Give the tenant a second knowledge base and make it the tenant's
        // default/active one, so "fall back to the active database" and
        // "pin to the credential's database" observably differ.
        let tenant_url = cluster.tenant_db_url(&acct.db_name).expect("tenant url");
        let second_id = {
            let manager = DatabaseManager::new_postgres(".", &tenant_url)
                .await
                .expect("open tenant manager");
            let info = manager
                .create_database("Second")
                .await
                .expect("create second KB");
            manager
                .set_default_database(&info.id)
                .await
                .expect("make second KB the default");
            info.id
        };

        let app =
            actix_test::init_service(App::new().service(echo_service(&control, &cluster))).await;
        let scoped = issue_token(
            &control,
            &acct.account_id,
            TokenScope::Database,
            Some("default"),
            "kb-pinned",
        )
        .await
        .expect("issue database-scoped token");

        // Explicit selection of a different KB — header or query — is
        // rejected before the handler (and before the tenant is even
        // loaded).
        let resp = actix_test::call_service(
            &app,
            bearer(get("scoped.atomic.test"), &scoped)
                .insert_header(("X-Atomic-Database", second_id.as_str()))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403, "header override must 403");
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["error"], "database_forbidden");

        let resp = actix_test::call_service(
            &app,
            bearer(
                actix_test::TestRequest::get()
                    .uri(&format!("/echo?db={second_id}"))
                    .insert_header((header::HOST, "scoped.atomic.test")),
                &scoped,
            )
            .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403, "?db= override must 403");

        // No explicit selection: the middleware injects the header, so the
        // request resolves to the credential's KB — not the tenant's
        // active one (which is `second_id` here).
        let resp = actix_test::call_service(
            &app,
            bearer(get("scoped.atomic.test"), &scoped).to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["scope"], "database");
        assert_eq!(
            body["db_header"], "default",
            "middleware must pin the request to allowed_db_id"
        );
        assert_eq!(body["resolves"], true);

        // Explicitly naming the allowed KB is fine.
        let resp = actix_test::call_service(
            &app,
            bearer(get("scoped.atomic.test"), &scoped)
                .insert_header(("X-Atomic-Database", "default"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);

        // An account-scoped token is unrestricted: it may select the
        // second KB explicitly, and unselective requests stay unpinned.
        let account_token = issue_token(&control, &acct.account_id, TokenScope::Account, None, "t")
            .await
            .expect("issue account token");
        let resp = actix_test::call_service(
            &app,
            bearer(get("scoped.atomic.test"), &account_token)
                .insert_header(("X-Atomic-Database", second_id.as_str()))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = actix_test::read_body_json(resp).await;
        assert_eq!(body["db_header"], second_id.as_str());
        assert_eq!(body["resolves"], true);
    })
    .await;
}
