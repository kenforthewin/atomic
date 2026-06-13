//! Explicit provider-config injection and live rotation.
//!
//! Pins the contract of `AtomicCore`'s explicit provider-config mode:
//!
//! * `open_postgres_with_provider(Some(config))` builds providers from the
//!   injected config and never consults the settings tables for provider
//!   config — proven by seeding the settings with a *dead* provider (a
//!   closed port) and watching the pipeline succeed against the mock.
//! * `update_provider_config` atomically swaps the active config so
//!   subsequent operations hit the new provider — proven with two mocks and
//!   their request counters.
//! * The swap is shared across every core resolved from the same
//!   `DatabaseManager`.
//! * `None` / settings mode regression coverage lives in the existing
//!   pipeline suite; explicit mode is additionally reachable on SQLite via
//!   `update_provider_config`, covered here.
//!
//! Postgres tests are gated on `ATOMIC_TEST_DATABASE_URL` and skip cleanly
//! without it, matching the rest of the suite.

mod support;

use atomic_core::{AtomicCore, CreateAtomRequest, ProviderConfig, ProviderType};
use support::{await_pipeline, event_collector, MockAiServer};

/// Build an explicit OpenRouter config pointed at a mock server. Exercises
/// `openrouter_base_url` end to end: the mock URL has no `/v1` suffix and the
/// provider's normalization must add it.
fn mock_openrouter_config(mock: &MockAiServer) -> ProviderConfig {
    let mut config = ProviderConfig::from_settings(&std::collections::HashMap::new());
    config.provider_type = ProviderType::OpenRouter;
    config.openrouter_api_key = Some("mock-openrouter-key".to_string());
    config.openrouter_base_url = mock.base_url();
    // 1536-dim model so no dimension reconciliation kicks in mid-test
    // (matches the mock's EMBED_DIM and the SQLite vec_chunks schema).
    config.openrouter_embedding_model = "openai/text-embedding-3-small".to_string();
    config.openrouter_llm_model = "mock-llm".to_string();
    config
}

/// Seed the settings tables with a provider config that *cannot* work — an
/// `openai_compat` endpoint on a closed port. Any code path that resolves
/// provider config from settings fails loudly instead of silently passing.
async fn seed_dead_provider_settings(core: &AtomicCore) {
    for (k, v) in [
        ("provider", "openai_compat"),
        ("openai_compat_base_url", "http://127.0.0.1:9"),
        ("openai_compat_api_key", "dead-key"),
        ("openai_compat_embedding_model", "dead-embed"),
        ("openai_compat_llm_model", "dead-llm"),
        ("openai_compat_embedding_dimension", "1536"),
        ("auto_tagging_enabled", "true"),
    ] {
        core.set_setting(k, v).await.expect("seed dead setting");
    }
}

async fn create_and_await(core: &AtomicCore, content: &str) -> String {
    let (cb, mut rx) = event_collector();
    let created = core
        .create_atom(
            CreateAtomRequest {
                content: content.to_string(),
                ..Default::default()
            },
            cb,
        )
        .await
        .expect("create_atom")
        .expect("atom inserted");
    await_pipeline(&mut rx, &created.atom.id).await;
    created.atom.id
}

// ==================== Injection: settings never consulted ====================

#[cfg(feature = "postgres")]
#[tokio::test]
async fn injected_config_ignores_settings_postgres() {
    let Ok(url) = std::env::var("ATOMIC_TEST_DATABASE_URL") else {
        eprintln!(
            "injected_config_ignores_settings_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    };
    support::truncate_postgres_for_test(&url).await;

    let mock = MockAiServer::start().await;
    let core = AtomicCore::open_postgres_with_provider(
        &url,
        "provider_injection_test",
        None,
        Some(mock_openrouter_config(&mock)),
    )
    .await
    .expect("open postgres with provider");

    // The settings tables hold a dead provider. If anything in the pipeline
    // consults them for provider config, embedding/tagging fails and
    // `await_pipeline` panics.
    seed_dead_provider_settings(&core).await;
    core.configure_autotag_targets(&["Topics".to_string()], &[])
        .await
        .expect("configure autotag targets");

    create_and_await(
        &core,
        "explicit provider config routes every call to the mock",
    )
    .await;

    assert!(
        mock.embedding_request_count() >= 1,
        "embedding should have hit the injected mock provider, got {} requests",
        mock.embedding_request_count()
    );
    assert!(
        mock.chat_request_count() >= 1,
        "tagging should have hit the injected mock provider, got {} requests",
        mock.chat_request_count()
    );

    // Semantic search resolves the same injected provider for the query
    // embedding (the Postgres search path reads no settings for it).
    let before = mock.embedding_request_count();
    let results = core
        .search(atomic_core::search::SearchOptions::new(
            "explicit provider config",
            atomic_core::search::SearchMode::Semantic,
            5,
        ))
        .await
        .expect("semantic search through injected provider");
    assert!(!results.is_empty(), "search should find the seeded atom");
    assert!(
        mock.embedding_request_count() > before,
        "query embedding should have hit the injected mock provider"
    );

    // And the explicit config is what the facade reports as configured.
    assert!(
        core.verify_provider_configured()
            .await
            .expect("verify provider"),
        "injected OpenRouter config with a key should report configured"
    );
}

// ==================== Live rotation ====================

#[cfg(feature = "postgres")]
#[tokio::test]
async fn update_provider_config_switches_live_postgres() {
    let Ok(url) = std::env::var("ATOMIC_TEST_DATABASE_URL") else {
        eprintln!(
            "update_provider_config_switches_live_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    };
    support::truncate_postgres_for_test(&url).await;

    let mock_a = MockAiServer::start().await;
    let mock_b = MockAiServer::start().await;

    let core = AtomicCore::open_postgres_with_provider(
        &url,
        "provider_rotation_test",
        None,
        Some(mock_openrouter_config(&mock_a)),
    )
    .await
    .expect("open postgres with provider");
    seed_dead_provider_settings(&core).await;
    core.configure_autotag_targets(&["Topics".to_string()], &[])
        .await
        .expect("configure autotag targets");

    create_and_await(&core, "first atom embeds through mock a").await;
    let a_after_first = mock_a.embedding_request_count();
    assert!(a_after_first >= 1, "first embed should hit mock A");
    assert_eq!(
        mock_b.embedding_request_count(),
        0,
        "mock B must be untouched before rotation"
    );

    core.update_provider_config(mock_openrouter_config(&mock_b));

    create_and_await(&core, "second atom embeds through mock b after rotation").await;
    assert!(
        mock_b.embedding_request_count() >= 1,
        "post-rotation embed should hit mock B, got {} requests",
        mock_b.embedding_request_count()
    );
    assert_eq!(
        mock_a.embedding_request_count(),
        a_after_first,
        "mock A must see no further requests after rotation"
    );
}

#[tokio::test]
async fn update_provider_config_switches_live_sqlite() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let core =
        AtomicCore::open_or_create(dir.path().join("rotation.db")).expect("open sqlite test db");

    // Settings hold a dead provider; the explicit config set via
    // update_provider_config must win for every subsequent operation.
    seed_dead_provider_settings(&core).await;
    core.configure_autotag_targets(&["Topics".to_string()], &[])
        .await
        .expect("configure autotag targets");

    let mock = MockAiServer::start().await;
    core.update_provider_config(mock_openrouter_config(&mock));

    create_and_await(&core, "sqlite atom embeds through the explicit mock config").await;
    assert!(
        mock.embedding_request_count() >= 1,
        "embedding should have hit the mock after update_provider_config, got {} requests",
        mock.embedding_request_count()
    );
    assert!(
        mock.chat_request_count() >= 1,
        "tagging should have hit the mock after update_provider_config, got {} requests",
        mock.chat_request_count()
    );
}

/// Model selection is provider config: in explicit mode the per-task
/// `wiki_model`/`chat_model` settings keys are pinned to the config's
/// `llm_model` (`ProviderConfig::apply_to_settings`), so a settings write
/// cannot route traffic on the explicitly configured credential to a model
/// the config didn't choose. Proven end to end: with both keys written to a
/// frontier model, every LLM request the mock receives — tagging at create
/// time and the chat agent — still carries the config's model. Remove the
/// pinning overlay and the chat leg fails with the frontier model in the
/// request body.
#[tokio::test]
async fn explicit_config_pins_per_task_models_sqlite() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let core =
        AtomicCore::open_or_create(dir.path().join("pinning.db")).expect("open sqlite test db");
    seed_dead_provider_settings(&core).await;
    core.configure_autotag_targets(&["Topics".to_string()], &[])
        .await
        .expect("configure autotag targets");

    let mock = MockAiServer::start().await;
    core.update_provider_config(mock_openrouter_config(&mock));

    // An out-of-band settings write points the per-task keys elsewhere.
    for key in ["chat_model", "wiki_model"] {
        core.set_setting(key, "frontier/expensive")
            .await
            .expect("write per-task model setting");
    }

    // Tagging (create pipeline) + the chat agent both resolve their model
    // through `settings_for_ai`.
    create_and_await(&core, "pinned-model atom about explicit configs").await;
    let conversation = core
        .create_conversation(&[], Some("pinning"))
        .await
        .expect("create conversation");
    core.send_chat_message(
        &conversation.conversation.id,
        "what do my notes say about explicit configs?",
        |_| {},
    )
    .await
    .expect("chat through the explicit config");

    let models = mock.chat_request_models();
    assert!(
        models.len() >= 2,
        "expected tagging and chat LLM traffic, got {models:?}"
    );
    for model in &models {
        assert_eq!(
            model, "mock-llm",
            "every LLM call must carry the explicit config's model, \
             never the settings-written one: {models:?}"
        );
    }
}

/// In explicit mode, embedding-space settings writes through
/// `set_setting_with_reembed` are inert end to end: with an atom already
/// embedded through the explicit config, writing `provider`,
/// `openai_compat_embedding_dimension`, and `embedding_model` stores the
/// values but recreates no vector index and queues no re-embedding — the
/// atom keeps its `complete` status and its vectors keep answering semantic
/// search, and the next embed still flows through the explicit config.
#[tokio::test]
async fn explicit_mode_embedding_space_settings_writes_are_inert_sqlite() {
    let dir = tempfile::TempDir::new().expect("create tempdir");
    let core =
        AtomicCore::open_or_create(dir.path().join("inert.db")).expect("open sqlite test db");
    seed_dead_provider_settings(&core).await;
    core.configure_autotag_targets(&["Topics".to_string()], &[])
        .await
        .expect("configure autotag targets");

    let mock = MockAiServer::start().await;
    core.update_provider_config(mock_openrouter_config(&mock));

    let atom_id = create_and_await(&core, "an explicit-mode note about vector spaces").await;
    let embeds_after_create = mock.embedding_request_count();

    // Each write would, in settings mode, either recreate the index at a
    // foreign dimension (3072) or queue a full re-embed (model change).
    for (key, value) in [
        ("provider", "openai_compat"),
        ("openai_compat_embedding_dimension", "3072"),
        ("embedding_model", "frontier/other-space"),
    ] {
        let result = core
            .set_setting_with_reembed(key, value, |_| {})
            .await
            .expect("settings write succeeds");
        assert!(!result.embedding_space_changed, "{key} must be inert");
        assert!(
            !result.dimension_changed,
            "{key} must not recreate the index"
        );
        assert_eq!(result.total_atom_count, 0, "{key} must queue no re-embeds");
        assert_eq!(result.retried_failed_count, 0, "{key} must retry nothing");
    }

    // No re-embedding was scheduled: the atom's status survived untouched
    // (an index recreation would have reset it to 'pending').
    let atom = core
        .get_atom(&atom_id)
        .await
        .expect("get atom")
        .expect("atom exists");
    assert_eq!(
        atom.atom.embedding_status, "complete",
        "the embedded atom must keep its vectors"
    );
    assert_eq!(
        mock.embedding_request_count(),
        embeds_after_create,
        "no re-embed traffic may follow the inert writes"
    );

    // The stored vectors still answer semantic search (the query embedding
    // accounts for the +1 below).
    let results = core
        .search(atomic_core::search::SearchOptions::new(
            "vector spaces",
            atomic_core::search::SearchMode::Semantic,
            5,
        ))
        .await
        .expect("semantic search after inert writes");
    assert!(
        results.iter().any(|r| r.atom.atom.id == atom_id),
        "existing vectors must survive the settings writes"
    );
    assert_eq!(mock.embedding_request_count(), embeds_after_create + 1);

    // And new embeds still run at the explicit config's space.
    create_and_await(&core, "a second note embedded after the inert writes").await;
    assert!(
        mock.embedding_request_count() > embeds_after_create + 1,
        "post-write embeds still flow through the explicit config"
    );
}

// ==================== Manager-wide sharing ====================

#[cfg(feature = "postgres")]
#[tokio::test]
async fn manager_shares_provider_config_across_cores_postgres() {
    use atomic_core::manager::DatabaseManager;

    let Ok(url) = std::env::var("ATOMIC_TEST_DATABASE_URL") else {
        eprintln!(
            "manager_shares_provider_config_across_cores_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    };
    support::truncate_postgres_for_test(&url).await;

    let mock_a = MockAiServer::start().await;
    let mock_b = MockAiServer::start().await;
    let config_a = mock_openrouter_config(&mock_a);
    let config_b = mock_openrouter_config(&mock_b);

    let manager = DatabaseManager::new_postgres_with_pool_and_provider(
        ".",
        &url,
        atomic_core::storage::PgPoolConfig::from_env(),
        Some(config_a.clone()),
    )
    .await
    .expect("manager with provider");

    let default_core = manager.active_core().await.expect("default core");
    assert_eq!(
        default_core.active_provider_config(),
        Some(config_a.clone()),
        "injected config should be active on the default core"
    );

    let second_db = manager
        .create_database("Second")
        .await
        .expect("create second database");
    let second_core = manager.get_core(&second_db.id).await.expect("second core");
    assert_eq!(
        second_core.active_provider_config(),
        Some(config_a),
        "lazily resolved cores must share the manager's injected config"
    );

    // Rotating through ONE core must cover the whole manager.
    default_core.update_provider_config(config_b.clone());
    assert_eq!(
        second_core.active_provider_config(),
        Some(config_b),
        "update_provider_config must propagate to every core of the manager"
    );
}
