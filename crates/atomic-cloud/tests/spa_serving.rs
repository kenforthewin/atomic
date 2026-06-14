//! Static-serving tests for the account-plane SPA fallback
//! ([`atomic_cloud::spa`]).
//!
//! These are **Postgres-free**: they exercise the fallback's routing
//! semantics (the SPA shell for deep links, real files for assets, explicit
//! routes never shadowed, traversal refused, base-domain injected) over real
//! actix HTTP routing, without the full multi-tenant composition. The
//! composed-server property — the SPA served on the app host while JSON
//! routes still resolve under CloudAuth — is pinned PG-gated in
//! `tests/e2e_cloud.rs`.

use actix_web::{test, web, App, HttpResponse};
use atomic_cloud::SpaServer;

/// Build a minimal fixture `dist/` (a base-domain-meta `index.html` and one
/// hashed asset) in a tempdir and load a [`SpaServer`] over it.
async fn fixture_spa(base_domain: &str) -> (tempfile::TempDir, SpaServer) {
    let dir = tempfile::tempdir().expect("tempdir");
    let index = r#"<!doctype html><html><head>
<meta name="atomic-cloud-base-domain" content="__ATOMIC_CLOUD_BASE_DOMAIN__" />
</head><body><div id="root"></div></body></html>"#;
    tokio::fs::write(dir.path().join("index.html"), index)
        .await
        .expect("write index.html");
    tokio::fs::create_dir_all(dir.path().join("assets"))
        .await
        .expect("mkdir assets");
    tokio::fs::write(
        dir.path().join("assets/index-deadbeef.js"),
        "console.log('atomic cloud');\n",
    )
    .await
    .expect("write asset");
    let spa = SpaServer::load(dir.path(), base_domain)
        .await
        .expect("load SPA");
    (dir, spa)
}

/// The SPA fallback registered behind a single explicit canary route, the
/// minimal shape that proves the shadowing contract: explicit services win,
/// everything else falls through to the shell.
#[actix_web::test]
async fn fallback_serves_shell_assets_and_never_shadows_explicit_routes() {
    let (_dir, spa) = fixture_spa("atomic.cloud").await;
    let app = test::init_service(
        App::new()
            // An explicit JSON route, exactly like `/health` / `/api/*` in the
            // real composition: it must win over the fallback.
            .route(
                "/health",
                web::get()
                    .to(|| async { HttpResponse::Ok().json(serde_json::json!({"ok": true})) }),
            )
            .configure(|cfg| spa.clone().configure_fallback(cfg)),
    )
    .await;

    // 1. A client-routed deep link with no file on disk → the SPA shell
    //    (HTML 200), with the base domain injected into the meta tag.
    for path in ["/", "/login", "/account/provider", "/signup"] {
        let resp = test::call_service(&app, test::TestRequest::get().uri(path).to_request()).await;
        assert_eq!(resp.status(), 200, "{path} serves the shell");
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.contains("text/html"),
            "{path} is HTML: {content_type}"
        );
        let body = test::read_body(resp).await;
        let text = String::from_utf8(body.to_vec()).expect("utf8 shell");
        assert!(
            text.contains(r#"content="atomic.cloud""#),
            "{path} carries the injected base domain: {text}"
        );
        assert!(
            !text.contains("__ATOMIC_CLOUD_BASE_DOMAIN__"),
            "{path} placeholder replaced"
        );
    }

    // 2. A real build asset is served as that file, with its JS content type
    //    and an immutable cache header (hashed name).
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/assets/index-deadbeef.js")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200, "asset served");
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.contains("javascript"),
        "asset is JS: {content_type}"
    );
    let cache = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        cache.contains("immutable"),
        "hashed asset is immutable: {cache}"
    );
    let body = test::read_body(resp).await;
    assert_eq!(&body[..], b"console.log('atomic cloud');\n", "asset bytes");

    // 3. The explicit JSON route is NOT shadowed — it returns its JSON, not
    //    the HTML shell.
    let resp = test::call_service(&app, test::TestRequest::get().uri("/health").to_request()).await;
    assert_eq!(resp.status(), 200, "health resolves");
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.contains("application/json"),
        "health stays JSON: {content_type}"
    );

    // 4. A missing asset under a real-looking path falls through to the shell
    //    (a deep link, not a 404), but a non-GET on an unmatched path is a
    //    genuine 404 — a mistyped API POST must not become an HTML 200.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/assets/does-not-exist.js")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200, "missing asset path → shell");
    let resp = test::call_service(
        &app,
        test::TestRequest::post().uri("/api/nope").to_request(),
    )
    .await;
    assert_eq!(resp.status(), 404, "non-GET unmatched path is a real 404");
}

/// `load_optional` returns `None` for a directory with no `index.html`, so a
/// pure-API deployment boots cleanly without the fallback.
#[actix_web::test]
async fn load_optional_none_without_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let spa = SpaServer::load_optional(dir.path(), "atomic.cloud")
        .await
        .expect("probe ok");
    assert!(spa.is_none(), "no index.html → no SPA fallback");
}

/// With a product app attached (`--product-dir`), the tenant root serves the
/// PRODUCT shell while the app host still serves the ACCOUNT shell, and an
/// asset resolves from whichever of the two dist roots holds it (Vite's
/// content-hashed names never collide). Mirrors the real dev composition; the
/// `/account/*` gate that overrides this on a tenant host is PG-gated
/// elsewhere.
#[actix_web::test]
async fn product_app_serves_at_tenant_root_account_on_app_host() {
    let base = "cloudtest.local";
    let (_acct_dir, spa) = fixture_spa(base).await; // account dist: index + assets/index-deadbeef.js

    // A separate product `dist-web`: a distinct shell marker and its own asset.
    let prod_dir = tempfile::tempdir().expect("product tempdir");
    tokio::fs::write(
        prod_dir.path().join("index.html"),
        r#"<!doctype html><html><body><div id="product-root"></div></body></html>"#,
    )
    .await
    .expect("write product index");
    tokio::fs::create_dir_all(prod_dir.path().join("assets"))
        .await
        .expect("mkdir product assets");
    tokio::fs::write(
        prod_dir.path().join("assets/product-cafe.js"),
        "console.log('knowledge base');\n",
    )
    .await
    .expect("write product asset");

    let spa = spa
        .with_product_dir(prod_dir.path())
        .await
        .expect("attach product");

    let app = test::init_service(
        App::new()
            .route(
                "/health",
                web::get()
                    .to(|| async { HttpResponse::Ok().json(serde_json::json!({"ok": true})) }),
            )
            .configure(|cfg| spa.clone().configure_fallback(cfg)),
    )
    .await;

    let body_of = |resp: actix_web::dev::ServiceResponse| async move {
        String::from_utf8(test::read_body(resp).await.to_vec()).expect("utf8")
    };
    let tenant = format!("alpha.{base}");
    let app_host = format!("app.{base}");

    // Tenant root + a tenant deep link → the PRODUCT shell.
    for path in ["/", "/some/canvas/view"] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(path)
                .insert_header(("host", tenant.as_str()))
                .to_request(),
        )
        .await;
        let text = body_of(resp).await;
        assert!(
            text.contains("product-root"),
            "tenant {path} serves the product shell, got: {text}"
        );
    }

    // App host root → the ACCOUNT shell (product app is tenant-only).
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/")
            .insert_header(("host", app_host.as_str()))
            .to_request(),
    )
    .await;
    let text = body_of(resp).await;
    assert!(
        text.contains(r#"content="cloudtest.local""#) && !text.contains("product-root"),
        "app host serves the account shell, got: {text}"
    );

    // Assets resolve across both roots on a tenant host (unique hashed names).
    for (asset, marker) in [
        ("/assets/index-deadbeef.js", "atomic cloud"), // account dist
        ("/assets/product-cafe.js", "knowledge base"), // product dist
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(asset)
                .insert_header(("host", tenant.as_str()))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200, "{asset} resolves");
        let text = body_of(resp).await;
        assert!(text.contains(marker), "{asset} is the right file: {text}");
    }

    // The explicit route still wins over the product fallback.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/health")
            .insert_header(("host", tenant.as_str()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert!(
        body_of(resp).await.contains("\"ok\":true"),
        "/health not shadowed"
    );
}
