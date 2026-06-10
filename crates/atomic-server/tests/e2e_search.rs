//! End-to-end search route tests across both storage backends.
//!
//! Exercises `POST /api/search` in semantic, keyword, and hybrid modes plus
//! the tag-scope filter and a couple of contract negatives. The atomic-core
//! search unit suite already pins the storage-level threshold semantics; this
//! file pins the HTTP contract — request shape in, result array out — on both
//! SQLite (FTS5 + sqlite-vec) and Postgres (tsvector + pgvector).
//!
//! The mock embedder is a bag-of-words unit vector hasher (see
//! `atomic_test_support::mock_ai`). Queries that share words with seeded
//! atoms produce highly aligned vectors, so semantic matches are
//! deterministic without a real model in the loop.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use support::{poll_until_embedding_done, test_app, Backend, TestCtx};

// ==================== Helpers ====================

/// Seed an atom with the given content and (optionally) tag IDs, then poll
/// until the embedding pipeline finishes. Returns the atom id.
///
/// Search hits the same FTS / vector tables the pipeline writes — without
/// the poll, the search assertions race against background ingestion.
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
        .set_json(json!({
            "content": content,
            "tag_ids": tag_ids,
        }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(resp.status(), 201, "POST /api/atoms must succeed");
    let body: Value = actix_test::read_body_json(resp).await;
    let id = body["id"].as_str().expect("id").to_string();
    poll_until_embedding_done(app, auth, &id).await;
    id
}

/// POST /api/search and return the parsed result array.
async fn run_search<S, B>(
    app: &S,
    auth: (&'static str, String),
    payload: Value,
) -> Vec<Value>
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::post()
        .uri("/api/search")
        .insert_header(auth)
        .set_json(payload)
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert!(
        resp.status().is_success(),
        "search should return 2xx, got {}",
        resp.status()
    );
    let body: Value = actix_test::read_body_json(resp).await;
    body.as_array().expect("array body").clone()
}

fn atom_ids(results: &[Value]) -> Vec<String> {
    // `SemanticSearchResult` flattens `AtomWithTags` which flattens `Atom`, so
    // the atom's id lives at the top-level `id` key in the response payload.
    results
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect()
}

// ==================== 1. Semantic search ====================

#[actix_web::test]
async fn semantic_search_returns_matching_atoms_sqlite() {
    run_semantic_search_returns_matching_atoms(Backend::Sqlite).await;
}

#[actix_web::test]
async fn semantic_search_returns_matching_atoms_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("semantic_search_returns_matching_atoms_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_semantic_search_returns_matching_atoms(Backend::Postgres).await;
}

async fn run_semantic_search_returns_matching_atoms(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let physics_a = seed_atom(
        &app,
        ctx.auth_header(),
        "quantum particles atomic waves momentum",
        &[],
    )
    .await;
    let physics_b = seed_atom(
        &app,
        ctx.auth_header(),
        "quantum field theory particles spin",
        &[],
    )
    .await;
    let _biology = seed_atom(
        &app,
        ctx.auth_header(),
        "ribosome protein synthesis mitochondria",
        &[],
    )
    .await;

    // Bag-of-words mock embeddings → shared-vocabulary queries land near the
    // physics atoms. Threshold 0.3 mirrors the production semantic-search
    // default so the test exercises a realistic cut.
    let results = run_search(
        &app,
        ctx.auth_header(),
        json!({
            "query": "quantum particles",
            "mode": "semantic",
            "threshold": 0.3,
            "limit": 10,
        }),
    )
    .await;

    let ids = atom_ids(&results);
    assert!(
        ids.contains(&physics_a) && ids.contains(&physics_b),
        "both physics atoms should match; got {:?}",
        ids
    );
    assert!(
        !ids.iter().any(|id| id == &_biology),
        "biology atom must not match physics query; got {:?}",
        ids
    );
}

// ==================== 2. Keyword search ====================

#[actix_web::test]
async fn keyword_search_matches_substring_sqlite() {
    run_keyword_search_matches_substring(Backend::Sqlite).await;
}

#[actix_web::test]
async fn keyword_search_matches_substring_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "keyword_search_matches_substring_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_keyword_search_matches_substring(Backend::Postgres).await;
}

async fn run_keyword_search_matches_substring(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let target = seed_atom(
        &app,
        ctx.auth_header(),
        "the philharmonic orchestra rehearsal began at dawn",
        &[],
    )
    .await;
    let _decoy = seed_atom(
        &app,
        ctx.auth_header(),
        "compilers and parser combinators in haskell",
        &[],
    )
    .await;

    // "philharmonic" is a whole word, no FTS stopword, no stemming surprises
    // across SQLite FTS5 and Postgres tsvector defaults.
    let results = run_search(
        &app,
        ctx.auth_header(),
        json!({
            "query": "philharmonic",
            "mode": "keyword",
            "limit": 10,
        }),
    )
    .await;

    let ids = atom_ids(&results);
    assert!(
        ids.contains(&target),
        "keyword search must surface the philharmonic atom; got {:?}",
        ids
    );
}

// ==================== 3. Hybrid search ====================

#[actix_web::test]
async fn hybrid_search_combines_both_sqlite() {
    run_hybrid_search_combines_both(Backend::Sqlite).await;
}

#[actix_web::test]
async fn hybrid_search_combines_both_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("hybrid_search_combines_both_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_hybrid_search_combines_both(Backend::Postgres).await;
}

async fn run_hybrid_search_combines_both(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let semantic_hit = seed_atom(
        &app,
        ctx.auth_header(),
        "quantum particles atomic waves momentum",
        &[],
    )
    .await;
    let keyword_hit = seed_atom(
        &app,
        ctx.auth_header(),
        "the philharmonic orchestra performed quantum repertoire",
        &[],
    )
    .await;

    // The RRF merge can reorder, so assert set membership not position.
    let results = run_search(
        &app,
        ctx.auth_header(),
        json!({
            "query": "quantum philharmonic",
            "mode": "hybrid",
            "threshold": 0.0,
            "limit": 20,
        }),
    )
    .await;

    let ids = atom_ids(&results);
    assert!(
        ids.contains(&semantic_hit),
        "hybrid should include semantic-only match; got {:?}",
        ids
    );
    assert!(
        ids.contains(&keyword_hit),
        "hybrid should include keyword-only match; got {:?}",
        ids
    );
}

// ==================== 4. Tag-scoped search ====================

#[actix_web::test]
async fn tag_scoped_search_excludes_other_tags_sqlite() {
    run_tag_scoped_search_excludes_other_tags(Backend::Sqlite).await;
}

#[actix_web::test]
async fn tag_scoped_search_excludes_other_tags_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("tag_scoped_search_excludes_other_tags_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_tag_scoped_search_excludes_other_tags(Backend::Postgres).await;
}

async fn run_tag_scoped_search_excludes_other_tags(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Use a tag name the mock auto-tagger never emits ("Physics"/"Biology"/
    // "Cooking" are reserved by the mock). Otherwise the pipeline would re-
    // attach a same-named tag to every atom and defeat the scope filter.
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "ScopedTopic", "parent_id": null }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201, "tag create must succeed");
    let tag: Value = actix_test::read_body_json(resp).await;
    let tag_id = tag["id"].as_str().expect("tag id").to_string();

    let tagged = seed_atom(
        &app,
        ctx.auth_header(),
        "quantum particles atomic waves momentum",
        &[tag_id.as_str()],
    )
    .await;
    let _untagged = seed_atom(
        &app,
        ctx.auth_header(),
        "quantum field theory particles spin",
        &[],
    )
    .await;

    let results = run_search(
        &app,
        ctx.auth_header(),
        json!({
            "query": "quantum particles",
            "mode": "semantic",
            "threshold": 0.3,
            "scope_tag_ids": [tag_id],
        }),
    )
    .await;
    let ids = atom_ids(&results);
    assert!(
        ids.contains(&tagged),
        "scoped search must include tagged atom; got {:?}",
        ids
    );
    assert_eq!(
        ids.len(),
        1,
        "scope must exclude the untagged atom; got {:?}",
        ids
    );
}

// ==================== 5. Empty corpus ====================

#[actix_web::test]
async fn empty_corpus_returns_empty_results_sqlite() {
    run_empty_corpus_returns_empty_results(Backend::Sqlite).await;
}

#[actix_web::test]
async fn empty_corpus_returns_empty_results_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "empty_corpus_returns_empty_results_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_empty_corpus_returns_empty_results(Backend::Postgres).await;
}

async fn run_empty_corpus_returns_empty_results(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // No seeded atoms — every mode must return 200 + empty array.
    for mode in ["semantic", "keyword", "hybrid"] {
        let results = run_search(
            &app,
            ctx.auth_header(),
            json!({ "query": "anything", "mode": mode }),
        )
        .await;
        assert!(
            results.is_empty(),
            "{mode} search on empty corpus must return empty array; got {:?}",
            results
        );
    }
}

// ==================== 6. Unauthorized ====================

#[actix_web::test]
async fn unauthorized_search_rejected_sqlite() {
    run_unauthorized_search_rejected(Backend::Sqlite).await;
}

#[actix_web::test]
async fn unauthorized_search_rejected_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "unauthorized_search_rejected_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_unauthorized_search_rejected(Backend::Postgres).await;
}

async fn run_unauthorized_search_rejected(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/search")
        .set_json(json!({ "query": "x", "mode": "semantic" }))
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(
        resp.is_err(),
        "search without Authorization header must be rejected"
    );
}
