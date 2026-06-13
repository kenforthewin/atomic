//! End-to-end tests for the URL ingestion, Obsidian import, and markdown
//! export routes.
//!
//! Ingestion: `POST /api/ingest/url` fetches a URL with the shared HTTP
//! client, hands the body to readability, and creates an atom. The
//! `MockUrlServer` fixture provides both an article-shaped HTML doc
//! (passes the gate) and a non-HTML path (gets rejected).
//!
//! Import: `POST /api/import/obsidian` scans a directory of markdown
//! files, parses frontmatter, and creates atoms with tags. We write a
//! tiny vault into a tempdir and assert the import counters.
//!
//! Export: `POST /api/databases/{id}/exports/markdown` queues an async
//! job. `GET /api/exports/{id}` polls; once complete the response
//! includes a one-time download URL. We then GET that URL and verify a
//! non-empty zip body comes back.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use std::time::Duration;
use support::{spawn_live_server, test_app, Backend, MockUrlServer, TestCtx};

// ==================== I1. Ingest article ====================

#[actix_web::test]
async fn ingest_url_creates_atom_sqlite() {
    run_ingest_url_creates_atom(Backend::Sqlite).await;
}

#[actix_web::test]
async fn ingest_url_creates_atom_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("ingest_url_creates_atom_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_ingest_url_creates_atom(Backend::Postgres).await;
}

async fn run_ingest_url_creates_atom(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;
    let url = mock.article_url(1);

    let req = actix_test::TestRequest::post()
        .uri("/api/ingest/url")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "url": url }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "ingest must succeed, got {}",
        resp.status()
    );
    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["url"], url);
    assert!(body["content_length"].as_u64().unwrap_or(0) > 200);
    let atom_id = body["atom_id"].as_str().expect("atom_id").to_string();

    // Atom should be fetchable with the recorded source_url.
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/atoms/{atom_id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let atom: Value = actix_test::read_body_json(resp).await;
    assert_eq!(atom["source_url"], url);
}

// ==================== I2. Ingest dedup ====================

#[actix_web::test]
async fn ingest_dedups_existing_source_url_sqlite() {
    run_ingest_dedups_existing_source_url(Backend::Sqlite).await;
}

#[actix_web::test]
async fn ingest_dedups_existing_source_url_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("ingest_dedups_existing_source_url_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_ingest_dedups_existing_source_url(Backend::Postgres).await;
}

async fn run_ingest_dedups_existing_source_url(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;
    let url = mock.article_url(2);

    let req = actix_test::TestRequest::post()
        .uri("/api/ingest/url")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "url": url }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Second ingest of the same URL — must reject via the dedup branch
    // (`source_url_exists_sync` short-circuits with a Validation error).
    let req = actix_test::TestRequest::post()
        .uri("/api/ingest/url")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "url": url }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(
        !resp.status().is_success(),
        "duplicate ingest must fail; got {}",
        resp.status()
    );
}

// ==================== I3. Non-HTML rejected ====================

#[actix_web::test]
async fn ingest_non_html_rejected_sqlite() {
    run_ingest_non_html_rejected(Backend::Sqlite).await;
}

#[actix_web::test]
async fn ingest_non_html_rejected_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("ingest_non_html_rejected_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_ingest_non_html_rejected(Backend::Postgres).await;
}

async fn run_ingest_non_html_rejected(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;

    let req = actix_test::TestRequest::post()
        .uri("/api/ingest/url")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "url": mock.plaintext_url() }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(
        !resp.status().is_success(),
        "non-HTML content must be rejected before atom creation"
    );
}

// ==================== IM1. Obsidian import ====================

#[actix_web::test]
async fn import_obsidian_vault_creates_atoms_sqlite() {
    run_import_obsidian_vault_creates_atoms(Backend::Sqlite).await;
}

#[actix_web::test]
async fn import_obsidian_vault_creates_atoms_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("import_obsidian_vault_creates_atoms_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_import_obsidian_vault_creates_atoms(Backend::Postgres).await;
}

async fn run_import_obsidian_vault_creates_atoms(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Build a vault with frontmatter tags AND a nested folder hierarchy so
    // both the flat-tag and the parent_id-chained-tag paths get exercised.
    let vault = tempfile::tempdir().expect("vault tempdir");
    let nested = vault.path().join("Topics").join("Science");
    std::fs::create_dir_all(&nested).unwrap();
    let note_a =
        "---\ntags: [imported, physics]\n---\n\n# Note A\n\nQuantum particles and waves.\n";
    let note_b =
        "---\ntags: [imported, cooking]\n---\n\n# Note B\n\nSourdough fermentation timing.\n";
    let nested_note = "# Nested\n\nA deep note inside Topics/Science.\n";
    std::fs::write(vault.path().join("note-a.md"), note_a).unwrap();
    std::fs::write(vault.path().join("note-b.md"), note_b).unwrap();
    std::fs::write(nested.join("nested.md"), nested_note).unwrap();

    let req = actix_test::TestRequest::post()
        .uri("/api/import/obsidian")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "vault_path": vault.path().to_str().unwrap(),
            "max_notes": null,
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let status = resp.status();
    let body: Value = actix_test::read_body_json(resp).await;

    assert!(
        status.is_success(),
        "import must succeed, got {status}: {body}"
    );
    assert_eq!(body["imported"], 3, "expected 3 imported atoms; got {body}");
    assert!(
        body["tags_created"].as_i64().unwrap_or(0) >= 1,
        "import should create at least one tag from frontmatter / folders; got {body}"
    );
    assert!(
        body["tags_linked"].as_i64().unwrap_or(0) >= 1,
        "import should link at least one tag to an atom; got {body}"
    );

    // The atoms should appear in the list.
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms?limit=50")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let list: Value = actix_test::read_body_json(resp).await;
    assert!(
        list["total_count"].as_i64().unwrap_or(0) >= 3,
        "list should report at least 3 atoms after import"
    );
}

// ==================== E1. Export lifecycle ====================

#[actix_web::test]
async fn markdown_export_job_completes_sqlite() {
    run_markdown_export_job_completes(Backend::Sqlite).await;
}

#[actix_web::test]
async fn markdown_export_job_completes_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "markdown_export_job_completes_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_markdown_export_job_completes(Backend::Postgres).await;
}

async fn run_markdown_export_job_completes(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    // Export download is a real-file response served as a `NamedFile`. It
    // works in `actix_web::test`, but using a real listener keeps the test
    // close to how the route runs in production and lets us follow the
    // download URL with reqwest.
    let server = spawn_live_server(&ctx).await;
    let client = reqwest::Client::new();

    // Seed an atom so the export has at least one row to write.
    let resp = client
        .post(format!("{}/api/atoms", server.base_url))
        .bearer_auth(&ctx.token)
        .json(&json!({ "content": "exportable atom content" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // Resolve the active database id. `list_databases` returns the active
    // id directly (always Some when any DB exists), so we use that and
    // fall back to the first row only as a defensive default.
    let db_id = {
        let (dbs, active) = ctx
            .state
            .manager
            .list_databases()
            .await
            .expect("list databases");
        if active.is_empty() {
            dbs.first().expect("at least one db").id.clone()
        } else {
            active
        }
    };

    // Start the job.
    let resp = client
        .post(format!(
            "{}/api/databases/{db_id}/exports/markdown",
            server.base_url
        ))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let job: Value = resp.json().await.unwrap();
    let job_id = job["id"].as_str().unwrap().to_string();

    // Poll until complete.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let download_path = loop {
        let resp = client
            .get(format!("{}/api/exports/{job_id}", server.base_url))
            .bearer_auth(&ctx.token)
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        if body["status"] == "complete" {
            break body["download_path"]
                .as_str()
                .expect("download_path on complete job")
                .to_string();
        }
        if body["status"] == "failed" {
            panic!("export job failed: {body}");
        }
        if std::time::Instant::now() >= deadline {
            panic!("export job did not complete within 20s; last body: {body}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // The download URL embeds a one-time token; no bearer auth needed.
    let resp = client
        .get(format!("{}{}", server.base_url, download_path))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "download must succeed, got {}",
        resp.status()
    );
    let bytes = resp.bytes().await.unwrap();
    assert!(bytes.len() > 100, "zip body should be non-empty");
    // Zip files start with `PK\x03\x04` (or `PK\x05\x06` for an empty archive).
    assert!(
        bytes.starts_with(b"PK"),
        "downloaded body should be a zip archive"
    );

    server.stop().await;
}

// ==================== I4. Auth ====================

#[actix_web::test]
async fn ingest_requires_auth_sqlite() {
    run_ingest_requires_auth(Backend::Sqlite).await;
}

#[actix_web::test]
async fn ingest_requires_auth_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("ingest_requires_auth_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_ingest_requires_auth(Backend::Postgres).await;
}

async fn run_ingest_requires_auth(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let req = actix_test::TestRequest::post()
        .uri("/api/ingest/url")
        .set_json(json!({ "url": "http://example.com" }))
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(resp.is_err(), "unauthenticated ingest must be rejected");
}
