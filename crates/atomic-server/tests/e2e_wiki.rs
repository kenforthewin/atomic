//! End-to-end wiki article tests across both storage backends.
//!
//! Wiki generation runs through the same LLM call shape as auto-tagging — a
//! non-streaming `chat/completions` with `response_format.json_schema`. The
//! mock provider branches on the schema name (`wiki_generation_result` for
//! full rewrites, `wiki_update_section_ops` for incremental updates) and
//! emits a deterministic article body with `[N]` markers that the citation
//! extractor maps back to the prompt's numbered source list.
//!
//! This file pins the HTTP surface of the wiki module:
//! `POST /api/wiki/{tag_id}/generate`, `GET /api/wiki/{tag_id}`,
//! `POST /api/wiki/{tag_id}/update`, `DELETE /api/wiki/{tag_id}`.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use support::{poll_until_embedding_done, test_app, Backend, TestCtx};

// ==================== Helpers ====================

/// Create a top-level tag and return its id.
async fn create_tag<S, B>(app: &S, auth: (&'static str, String), name: &str) -> String
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(auth)
        .set_json(json!({ "name": name, "parent_id": null }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(resp.status(), 201, "tag create must succeed");
    let body: Value = actix_test::read_body_json(resp).await;
    body["id"].as_str().expect("tag id").to_string()
}

/// Seed an atom with the given content and tags, then wait for embedding to
/// finish so the chunk is available to the wiki generator's source query.
async fn seed_atom<S, B>(
    app: &S,
    auth: (&'static str, String),
    content: &str,
    tag_ids: &[&str],
) -> String
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(auth.clone())
        .set_json(json!({ "content": content, "tag_ids": tag_ids }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(resp.status(), 201, "POST /api/atoms must succeed");
    let body: Value = actix_test::read_body_json(resp).await;
    let id = body["id"].as_str().expect("id").to_string();
    poll_until_embedding_done(app, auth, &id).await;
    id
}

/// POST /api/wiki/{tag_id}/generate. Returns the parsed
/// `WikiArticleWithCitations` body.
async fn generate_wiki<S, B>(
    app: &S,
    auth: (&'static str, String),
    tag_id: &str,
    tag_name: &str,
) -> Value
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/wiki/{tag_id}/generate"))
        .insert_header(auth)
        .set_json(json!({ "tag_name": tag_name }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert!(
        resp.status().is_success(),
        "generate_wiki must succeed, got {}",
        resp.status()
    );
    actix_test::read_body_json(resp).await
}

// ==================== 1. Generate returns an article ====================

#[actix_web::test]
async fn generate_wiki_for_tag_returns_article_sqlite() {
    run_generate_wiki_for_tag_returns_article(Backend::Sqlite).await;
}

#[actix_web::test]
async fn generate_wiki_for_tag_returns_article_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "generate_wiki_for_tag_returns_article_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_generate_wiki_for_tag_returns_article(Backend::Postgres).await;
}

async fn run_generate_wiki_for_tag_returns_article(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let tag_id = create_tag(&app, ctx.auth_header(), "PhysicsWiki").await;
    for content in [
        "quantum particles atomic waves",
        "quantum field theory spin",
        "classical mechanics newton orbit",
    ] {
        seed_atom(&app, ctx.auth_header(), content, &[tag_id.as_str()]).await;
    }

    let body = generate_wiki(&app, ctx.auth_header(), &tag_id, "PhysicsWiki").await;

    let content = body["article"]["content"].as_str().expect("content");
    assert!(!content.is_empty(), "article content must be non-empty");
    let citations = body["citations"].as_array().expect("citations");
    assert!(
        !citations.is_empty(),
        "mock article cites [1]/[2]; extractor should produce citations"
    );
}

// ==================== 2. Citations link to source atoms ====================

#[actix_web::test]
async fn generated_article_links_back_to_source_atoms_sqlite() {
    run_generated_article_links_back_to_source_atoms(Backend::Sqlite).await;
}

#[actix_web::test]
async fn generated_article_links_back_to_source_atoms_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "generated_article_links_back_to_source_atoms_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_generated_article_links_back_to_source_atoms(Backend::Postgres).await;
}

async fn run_generated_article_links_back_to_source_atoms(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let tag_id = create_tag(&app, ctx.auth_header(), "BiologyWiki").await;
    let mut atom_ids = Vec::new();
    for content in [
        "ribosome protein synthesis cell membrane",
        "mitochondria atp respiration",
        "dna replication helicase polymerase",
    ] {
        atom_ids.push(seed_atom(&app, ctx.auth_header(), content, &[tag_id.as_str()]).await);
    }

    let body = generate_wiki(&app, ctx.auth_header(), &tag_id, "BiologyWiki").await;
    let citations = body["citations"].as_array().unwrap();

    for citation in citations {
        let atom_id = citation["atom_id"].as_str().expect("atom_id");
        assert!(
            atom_ids.iter().any(|seeded| seeded == atom_id),
            "citation atom_id {atom_id} must resolve to a seeded atom (seeded: {atom_ids:?})"
        );
        let req = actix_test::TestRequest::get()
            .uri(&format!("/api/atoms/{atom_id}"))
            .insert_header(ctx.auth_header())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "cited atom must be fetchable");
    }
}

// ==================== 3. Incremental update integrates new atoms ====================

#[actix_web::test]
async fn incremental_update_integrates_new_atoms_sqlite() {
    run_incremental_update_integrates_new_atoms(Backend::Sqlite).await;
}

#[actix_web::test]
async fn incremental_update_integrates_new_atoms_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "incremental_update_integrates_new_atoms_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_incremental_update_integrates_new_atoms(Backend::Postgres).await;
}

async fn run_incremental_update_integrates_new_atoms(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let tag_id = create_tag(&app, ctx.auth_header(), "CookingWiki").await;
    seed_atom(
        &app,
        ctx.auth_header(),
        "sourdough bread bulk fermentation",
        &[tag_id.as_str()],
    )
    .await;
    seed_atom(
        &app,
        ctx.auth_header(),
        "knife skills julienne brunoise dice",
        &[tag_id.as_str()],
    )
    .await;

    let first = generate_wiki(&app, ctx.auth_header(), &tag_id, "CookingWiki").await;
    let first_content = first["article"]["content"].as_str().unwrap().to_string();

    // Add a new atom under the same tag and run the incremental update path.
    let new_atom_id = seed_atom(
        &app,
        ctx.auth_header(),
        "espresso extraction puck preparation",
        &[tag_id.as_str()],
    )
    .await;

    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/wiki/{tag_id}/update"))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "tag_name": "CookingWiki" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "wiki update must succeed, got {}",
        resp.status()
    );
    let updated: Value = actix_test::read_body_json(resp).await;
    let updated_content = updated["article"]["content"].as_str().unwrap();
    assert_ne!(
        updated_content, first_content,
        "update should produce a different article body"
    );
    let cited_atoms: Vec<String> = updated["citations"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["atom_id"].as_str().map(str::to_string))
        .collect();
    assert!(
        cited_atoms.iter().any(|a| a == &new_atom_id),
        "new atom must appear in updated article's citations; got {cited_atoms:?}"
    );
}

// ==================== 4. Unknown tag returns 404 ====================

#[actix_web::test]
async fn wiki_for_unknown_tag_returns_error_sqlite() {
    run_wiki_for_unknown_tag_returns_error(Backend::Sqlite).await;
}

#[actix_web::test]
async fn wiki_for_unknown_tag_returns_error_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "wiki_for_unknown_tag_returns_error_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_wiki_for_unknown_tag_returns_error(Backend::Postgres).await;
}

async fn run_wiki_for_unknown_tag_returns_error(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // The read path returns `200 null` for "no article" — the frontend store
    // depends on null-as-empty rather than an error path. This test pins
    // that contract: status 200, body literal `null`.
    let req = actix_test::TestRequest::get()
        .uri("/api/wiki/00000000-0000-0000-0000-000000000000")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "GET for unknown tag must return 200 + null");
    let body: Value = actix_test::read_body_json(resp).await;
    assert!(
        body.is_null(),
        "GET for unknown tag's wiki must return null body; got {body}"
    );
}

// ==================== 5. Delete wiki ====================

#[actix_web::test]
async fn delete_wiki_article_sqlite() {
    run_delete_wiki_article(Backend::Sqlite).await;
}

#[actix_web::test]
async fn delete_wiki_article_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("delete_wiki_article_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_delete_wiki_article(Backend::Postgres).await;
}

async fn run_delete_wiki_article(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let tag_id = create_tag(&app, ctx.auth_header(), "DeletableWiki").await;
    seed_atom(
        &app,
        ctx.auth_header(),
        "tag-anchored content for deletable wiki",
        &[tag_id.as_str()],
    )
    .await;
    generate_wiki(&app, ctx.auth_header(), &tag_id, "DeletableWiki").await;

    // Sanity: it's there.
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/wiki/{tag_id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/wiki/{tag_id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "wiki delete must succeed, got {}",
        resp.status()
    );

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/wiki/{tag_id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "deleted-then-GET wiki returns 200 + null");
    let body: Value = actix_test::read_body_json(resp).await;
    assert!(body.is_null(), "deleted wiki must return null body; got {body}");
}

// ==================== 6. Auth required ====================

#[actix_web::test]
async fn wiki_generation_requires_auth_sqlite() {
    run_wiki_generation_requires_auth(Backend::Sqlite).await;
}

#[actix_web::test]
async fn wiki_generation_requires_auth_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "wiki_generation_requires_auth_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_wiki_generation_requires_auth(Backend::Postgres).await;
}

async fn run_wiki_generation_requires_auth(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/wiki/any-tag/generate")
        .set_json(json!({ "tag_name": "any" }))
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(
        resp.is_err(),
        "unauthenticated wiki generation must be rejected"
    );
}
