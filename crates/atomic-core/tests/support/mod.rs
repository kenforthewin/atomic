//! Shared infrastructure for pipeline integration tests.
//!
//! The wiremock-backed `MockAiServer` and the Postgres truncation helper
//! both live in the workspace's `atomic-test-support` crate so atomic-server
//! can reuse them without duplication. This file owns only the pieces tied
//! to atomic-core's concrete `AtomicCore` shape: the `Backend` switch, the
//! `setup_core` / `open_bare` constructors, chunk-id / pipeline-job helpers,
//! and the `EmbeddingEvent` awaiter.

#![allow(dead_code)] // Referenced by multiple test binaries; some helpers are per-test.

use std::sync::Arc;
use std::time::Duration;

use atomic_core::AtomicCore;
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

// Re-export the mock + constants so existing test code keeps using the
// `support::MockAiServer` / `support::EMBED_DIM` paths it already imports.
// `unused_imports` is allowed because each integration-test binary compiles
// this module fresh and only some of them use the mock surface — re-exports
// look unused to a binary that doesn't reach into them.
#[allow(unused_imports)]
pub use atomic_test_support::{MockAiServer, EDGE_SIMILARITY_THRESHOLD, EMBED_DIM};

#[cfg(feature = "postgres")]
#[allow(unused_imports)]
pub use atomic_test_support::truncate_postgres_for_test;

// ==================== Backend switch + test harness ====================

pub enum Backend {
    Sqlite,
    #[cfg(feature = "postgres")]
    Postgres,
}

/// Per-test resources that must outlive the `AtomicCore`. Drop order matters
/// — the temp dir needs to live until after the core is dropped (SQLite has
/// the DB file open). For Postgres, holding nothing extra is fine.
pub struct CoreHandle {
    pub core: AtomicCore,
    _tempdir: Option<TempDir>,
}

/// Build an `AtomicCore` on the chosen backend and wire it up to the mock:
///
/// 1. Open a fresh DB (SQLite temp dir / Postgres truncated).
/// 2. Seed settings pointing at the mock's base URL with the
///    `openai_compat` provider selected.
/// 3. Seed a single auto-tag-target category ("Topics") so the tagging
///    path runs instead of short-circuiting on an empty tag tree.
///
/// Postgres: returns `None` if `ATOMIC_TEST_DATABASE_URL` isn't set so callers
/// can gracefully skip the test on CI configurations without a database.
pub async fn setup_core(backend: Backend, mock_url: &str) -> Option<CoreHandle> {
    let (core, tempdir) = match backend {
        Backend::Sqlite => {
            let dir = TempDir::new().expect("create tempdir");
            let core =
                AtomicCore::open_or_create(dir.path().join("pipeline.db")).expect("open sqlite");
            (core, Some(dir))
        }
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let url = std::env::var("ATOMIC_TEST_DATABASE_URL").ok()?;
            // Fresh schema per test run — truncate leaves the schema intact
            // but wipes seeded tags/settings so `open_postgres` re-seeds.
            truncate_postgres_for_test(&url).await;
            let core = AtomicCore::open_postgres(&url, "pipeline_test", None)
                .await
                .expect("open postgres");
            (core, None)
        }
    };

    // Point the pipeline at the mock HTTP server.
    for (k, v) in [
        ("provider", "openai_compat"),
        ("openai_compat_base_url", mock_url),
        ("openai_compat_api_key", "test-key"),
        ("openai_compat_embedding_model", "mock-embed"),
        ("openai_compat_llm_model", "mock-llm"),
        ("openai_compat_embedding_dimension", "1536"),
        ("auto_tagging_enabled", "true"),
    ] {
        core.set_setting(k, v).await.expect("seed test setting");
    }

    // Ensure at least one top-level auto-tag target exists so
    // `get_tag_tree_for_llm` returns a non-empty tree and the tagging path
    // actually runs. For SQLite we start with an empty tags table; for
    // Postgres `open_postgres` seeds default categories but leaves the
    // is_autotag_target flag off.
    core.configure_autotag_targets(&["Topics".to_string()], &[])
        .await
        .expect("configure autotag targets");

    Some(CoreHandle {
        core,
        _tempdir: tempdir,
    })
}

/// Open a fresh core on either backend without seeding provider settings.
/// Used by tests that need to exercise the no-provider failure path — the
/// "happy path" `setup_core` plumbs a working mock provider in.
pub async fn open_bare(backend: Backend) -> Option<CoreHandle> {
    match backend {
        Backend::Sqlite => {
            let dir = TempDir::new().expect("create tempdir");
            let core = AtomicCore::open_or_create(dir.path().join("pipeline.db"))
                .expect("open sqlite test db");
            Some(CoreHandle {
                core,
                _tempdir: Some(dir),
            })
        }
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let url = std::env::var("ATOMIC_TEST_DATABASE_URL").ok()?;
            truncate_postgres_for_test(&url).await;
            let core = AtomicCore::open_postgres(&url, "pipeline_test", None)
                .await
                .expect("open postgres");
            Some(CoreHandle {
                core,
                _tempdir: None,
            })
        }
    }
}

/// Return chunk IDs for an atom, ordered by chunk_index. Cross-backend so the
/// same assertion ("chunks preserved across a re-embed") works against both
/// SQLite and Postgres.
pub async fn chunk_ids_for_atom(core: &AtomicCore, atom_id: &str) -> Vec<String> {
    if core.database().is_some() {
        let conn = rusqlite::Connection::open(core.db_path()).expect("open sqlite db");
        let mut stmt = conn
            .prepare("SELECT id FROM atom_chunks WHERE atom_id = ?1 ORDER BY chunk_index")
            .expect("prepare chunk query");
        stmt.query_map([atom_id], |row| row.get::<_, String>(0))
            .expect("query chunk ids")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect chunk ids")
    } else {
        #[cfg(feature = "postgres")]
        {
            use sqlx::postgres::PgPoolOptions;
            let url =
                std::env::var("ATOMIC_TEST_DATABASE_URL").expect("ATOMIC_TEST_DATABASE_URL unset");
            let pool = PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .expect("connect chunk-id pool");
            let rows: Vec<(String,)> = sqlx::query_as(
                "SELECT id FROM atom_chunks WHERE atom_id = $1 ORDER BY chunk_index",
            )
            .bind(atom_id)
            .fetch_all(&pool)
            .await
            .expect("query chunk ids");
            rows.into_iter().map(|(id,)| id).collect()
        }
        #[cfg(not(feature = "postgres"))]
        panic!("Postgres backend reached without postgres feature");
    }
}

/// Count rows in `atom_pipeline_jobs`. Used by tests that assert the ledger
/// is cleared after terminal states fire.
pub async fn pending_pipeline_job_count(core: &AtomicCore) -> i64 {
    if core.database().is_some() {
        let conn = rusqlite::Connection::open(core.db_path()).expect("open sqlite db");
        conn.query_row("SELECT COUNT(*) FROM atom_pipeline_jobs", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count pipeline jobs")
    } else {
        #[cfg(feature = "postgres")]
        {
            use sqlx::postgres::PgPoolOptions;
            let url =
                std::env::var("ATOMIC_TEST_DATABASE_URL").expect("ATOMIC_TEST_DATABASE_URL unset");
            let pool = PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .expect("connect job-count pool");
            sqlx::query_scalar("SELECT COUNT(*) FROM atom_pipeline_jobs")
                .fetch_one(&pool)
                .await
                .expect("count pipeline jobs")
        }
        #[cfg(not(feature = "postgres"))]
        panic!("Postgres backend reached without postgres feature");
    }
}

// ==================== Pipeline completion awaiter ====================

/// Event channel returned to a test so it can await specific pipeline
/// milestones without sprinkling `sleep`s.
pub type EventRx = UnboundedReceiver<atomic_core::EmbeddingEvent>;

/// Make an `on_event` callback that forwards every event into a channel.
/// Returns the callback (to hand to `create_atom`) and the receiver (to poll
/// in the test). The callback is `Arc`-backed because `create_atom`'s bound
/// is `Fn + Send + Sync + 'static`.
pub fn event_collector() -> (
    impl Fn(atomic_core::EmbeddingEvent) + Send + Sync + Clone + 'static,
    EventRx,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = Arc::new(tx);
    let cb = move |ev| {
        let _ = tx.send(ev);
    };
    (cb, rx)
}

/// Wait until both `EmbeddingComplete`, a terminal tagging event
/// (`TaggingComplete` / `TaggingSkipped` / `TaggingFailed`), and the owning
/// queue run's completion have fired. Returns the captured target-atom events
/// so tests can assert on payloads.
pub async fn await_pipeline(rx: &mut EventRx, atom_id: &str) -> Vec<atomic_core::EmbeddingEvent> {
    use atomic_core::EmbeddingEvent;

    let mut captured = Vec::new();
    let mut embedding_done = false;
    let mut tagging_done = false;
    let mut queue_done = false;

    // A generous budget — the mock responds instantly, but CI runners can
    // stall under load. Fails loudly instead of hanging forever.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

    while !(embedding_done && tagging_done && queue_done) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "pipeline did not complete for {atom_id} within 15s. Captured: {:?}",
                captured
            );
        }

        let ev = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => panic!("event channel closed before pipeline finished"),
            Err(_) => panic!(
                "timed out waiting for pipeline events for {atom_id}. Captured: {:?}",
                captured
            ),
        };

        let matches_target = match &ev {
            EmbeddingEvent::Started { atom_id: id }
            | EmbeddingEvent::EmbeddingComplete { atom_id: id }
            | EmbeddingEvent::EmbeddingFailed { atom_id: id, .. }
            | EmbeddingEvent::TaggingComplete { atom_id: id, .. }
            | EmbeddingEvent::TaggingSkipped { atom_id: id }
            | EmbeddingEvent::TaggingFailed { atom_id: id, .. } => id == atom_id,
            EmbeddingEvent::BatchProgress { .. }
            | EmbeddingEvent::PipelineQueueStarted { .. }
            | EmbeddingEvent::PipelineQueueProgress { .. } => false,
            EmbeddingEvent::PipelineQueueCompleted { .. } => {
                queue_done = true;
                false
            }
        };

        if matches_target {
            match &ev {
                EmbeddingEvent::EmbeddingComplete { .. } => embedding_done = true,
                EmbeddingEvent::EmbeddingFailed { error, .. } => {
                    panic!("embedding failed for {atom_id}: {error}")
                }
                EmbeddingEvent::TaggingComplete { .. } | EmbeddingEvent::TaggingSkipped { .. } => {
                    tagging_done = true
                }
                EmbeddingEvent::TaggingFailed { error, .. } => {
                    panic!("tagging failed for {atom_id}: {error}")
                }
                _ => {}
            }
            captured.push(ev);
        }
    }

    captured
}
