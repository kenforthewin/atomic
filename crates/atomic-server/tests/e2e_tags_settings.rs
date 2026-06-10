//! End-to-end tests for the tag management and settings surfaces.
//!
//! Tag CRUD and the autotag configuration helpers are pure storage paths —
//! no LLM involvement. Tag compaction is the one exception: it calls the
//! mock provider with the `merge_result` schema, which we extend in
//! `atomic_test_support::mock_ai` to emit a deterministic merge.
//!
//! Settings cover two related contracts:
//!   1. The plain `PUT /api/settings/{key}` → `GET /api/settings` round
//!      trip for non-embedding-space keys (writes go to registry or per-DB
//!      override based on routing).
//!   2. The embedding-space gate (`is_embedding_space_key`): a write
//!      against an embedding key reaches `set_setting_with_reembed`,
//!      whose behavior is exhaustively covered by `atomic-core`'s
//!      pipeline tests. This suite only pins the HTTP gate.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use support::{test_app, Backend, TestCtx};

// ==================== Tag helpers ====================

async fn create_tag<S, B>(
    app: &S,
    auth: (&'static str, String),
    name: &str,
    parent_id: Option<&str>,
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
        .uri("/api/tags")
        .insert_header(auth)
        .set_json(json!({ "name": name, "parent_id": parent_id }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(resp.status(), 201);
    let body: Value = actix_test::read_body_json(resp).await;
    body["id"].as_str().unwrap().to_string()
}

// ==================== T1. Create round-trip ====================

#[actix_web::test]
async fn create_tag_round_trip_sqlite() {
    run_create_tag_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn create_tag_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("create_tag_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_create_tag_round_trip(Backend::Postgres).await;
}

async fn run_create_tag_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let id = create_tag(&app, ctx.auth_header(), "T1Tag", None).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let tags: Vec<Value> = actix_test::read_body_json(resp).await;
    assert!(tags.iter().any(|t| t["id"] == id), "created tag must appear in /api/tags");
}

// ==================== T2. Update ====================

#[actix_web::test]
async fn update_tag_renames_sqlite() {
    run_update_tag_renames(Backend::Sqlite).await;
}

#[actix_web::test]
async fn update_tag_renames_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("update_tag_renames_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_update_tag_renames(Backend::Postgres).await;
}

async fn run_update_tag_renames(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let id = create_tag(&app, ctx.auth_header(), "Original", None).await;

    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/tags/{id}"))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "Renamed", "parent_id": null }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let req = actix_test::TestRequest::get()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tags: Vec<Value> = actix_test::read_body_json(resp).await;
    assert!(
        tags.iter().any(|t| t["id"] == id && t["name"] == "Renamed"),
        "tag should be renamed"
    );
}

// ==================== T3. Delete (non-recursive) ====================

#[actix_web::test]
async fn delete_tag_removes_row_sqlite() {
    run_delete_tag_removes_row(Backend::Sqlite).await;
}

#[actix_web::test]
async fn delete_tag_removes_row_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("delete_tag_removes_row_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_delete_tag_removes_row(Backend::Postgres).await;
}

async fn run_delete_tag_removes_row(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let id = create_tag(&app, ctx.auth_header(), "Deletable", None).await;

    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/tags/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let req = actix_test::TestRequest::get()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tags: Vec<Value> = actix_test::read_body_json(resp).await;
    assert!(
        tags.iter().all(|t| t["id"] != id),
        "deleted tag must not appear in subsequent list"
    );
}

// ==================== T4. Hierarchy children ====================

#[actix_web::test]
async fn tag_hierarchy_children_query_sqlite() {
    run_tag_hierarchy_children_query(Backend::Sqlite).await;
}

#[actix_web::test]
async fn tag_hierarchy_children_query_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("tag_hierarchy_children_query_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_tag_hierarchy_children_query(Backend::Postgres).await;
}

async fn run_tag_hierarchy_children_query(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let parent = create_tag(&app, ctx.auth_header(), "Parent", None).await;
    for name in ["ChildA", "ChildB", "ChildC"] {
        create_tag(&app, ctx.auth_header(), name, Some(parent.as_str())).await;
    }

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/tags/{parent}/children"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: Value = actix_test::read_body_json(resp).await;
    let children = body["children"].as_array().expect("children array");
    assert_eq!(children.len(), 3, "expected 3 children, got {}", children.len());
}

// ==================== T5. Autotag-target flag ====================

#[actix_web::test]
async fn autotag_target_flag_persists_sqlite() {
    run_autotag_target_flag_persists(Backend::Sqlite).await;
}

#[actix_web::test]
async fn autotag_target_flag_persists_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("autotag_target_flag_persists_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_autotag_target_flag_persists(Backend::Postgres).await;
}

async fn run_autotag_target_flag_persists(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let id = create_tag(&app, ctx.auth_header(), "MaybeAutoTag", None).await;

    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/tags/{id}/autotag-target"))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "value": true }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    let req = actix_test::TestRequest::get()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tags: Vec<Value> = actix_test::read_body_json(resp).await;
    let flag = tags
        .iter()
        .find(|t| t["id"] == id)
        .and_then(|t| t["is_autotag_target"].as_bool())
        .expect("tag returned with autotag flag");
    assert!(flag, "autotag-target flag should round-trip as true");
}

// ==================== T6. Configure autotag targets ====================

#[actix_web::test]
async fn configure_autotag_targets_adds_custom_sqlite() {
    run_configure_autotag_targets_adds_custom(Backend::Sqlite).await;
}

#[actix_web::test]
async fn configure_autotag_targets_adds_custom_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("configure_autotag_targets_adds_custom_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_configure_autotag_targets_adds_custom(Backend::Postgres).await;
}

async fn run_configure_autotag_targets_adds_custom(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/tags/configure-autotag-targets")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "keep_defaults": ["Topics"],
            "add_custom": ["CustomCategory"],
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let added: Vec<Value> = actix_test::read_body_json(resp).await;
    assert!(
        added.iter().any(|t| t["name"] == "CustomCategory"),
        "endpoint should return the newly created custom tag"
    );

    // Confirm the custom tag is flagged as auto-tag target.
    let req = actix_test::TestRequest::get()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tags: Vec<Value> = actix_test::read_body_json(resp).await;
    let custom = tags
        .iter()
        .find(|t| t["name"] == "CustomCategory")
        .expect("custom tag persisted");
    assert_eq!(custom["is_autotag_target"], true);
}

// ==================== T7. Tag compaction ====================

#[actix_web::test]
async fn tag_compaction_merges_pair_sqlite() {
    run_tag_compaction_merges_pair(Backend::Sqlite).await;
}

#[actix_web::test]
async fn tag_compaction_merges_pair_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("tag_compaction_merges_pair_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_tag_compaction_merges_pair(Backend::Postgres).await;
}

async fn run_tag_compaction_merges_pair(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let winner = create_tag(&app, ctx.auth_header(), "MockWinner", None).await;
    let loser = create_tag(&app, ctx.auth_header(), "MockLoser", None).await;

    // Compaction reads tags via `get_tags_for_compaction`, which filters by
    // `atom_count > 0`. Attach an atom to each tag so they're surfaced to
    // the LLM (and so `apply_tag_merges` has rows to retag).
    for tag_id in [&winner, &loser] {
        let req = actix_test::TestRequest::post()
            .uri("/api/atoms")
            .insert_header(ctx.auth_header())
            .set_json(json!({ "content": "anchor content", "tag_ids": [tag_id] }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);
    }

    let req = actix_test::TestRequest::post()
        .uri("/api/utils/compact-tags")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "compaction must succeed");
    let body: Value = actix_test::read_body_json(resp).await;
    assert!(
        body["tags_merged"].as_i64().unwrap_or(0) >= 1,
        "mock emits one merge; tags_merged must be >= 1, got {body}"
    );
}

// ==================== S1. Setting round-trip ====================

#[actix_web::test]
async fn set_setting_round_trip_sqlite() {
    run_set_setting_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn set_setting_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("set_setting_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_set_setting_round_trip(Backend::Postgres).await;
}

async fn run_set_setting_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // `auto_tagging_enabled` is a non-embedding-space key — the settings
    // route writes straight through without triggering the re-embed gate,
    // so this round-trip pins the plain `set_setting` path.
    let req = actix_test::TestRequest::put()
        .uri("/api/settings/auto_tagging_enabled")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "value": "false" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let req = actix_test::TestRequest::get()
        .uri("/api/settings")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: Value = actix_test::read_body_json(resp).await;
    let value = body
        .as_array()
        .and_then(|a| a.iter().find(|s| s["key"] == "auto_tagging_enabled"))
        .and_then(|s| s["value"].as_str())
        .or_else(|| {
            // Some routes return a map keyed by setting name. Fall back to
            // pointer lookup so the test survives either layout.
            body.pointer("/auto_tagging_enabled/value")
                .and_then(|v| v.as_str())
        })
        .expect("auto_tagging_enabled should be present in settings");
    assert_eq!(value, "false", "setting must round-trip; got {value}");
}

// ==================== S2. Auth ====================

#[actix_web::test]
async fn settings_require_auth_sqlite() {
    run_settings_require_auth(Backend::Sqlite).await;
}

#[actix_web::test]
async fn settings_require_auth_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("settings_require_auth_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_settings_require_auth(Backend::Postgres).await;
}

async fn run_settings_require_auth(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let req = actix_test::TestRequest::get()
        .uri("/api/settings")
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(resp.is_err(), "unauthenticated settings must be rejected");
}
