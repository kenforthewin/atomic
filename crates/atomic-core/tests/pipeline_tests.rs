//! End-to-end pipeline tests.
//!
//! One atom creation → chunk → embed (via mock HTTP) → auto-tag (via mock HTTP)
//! → deferred graph maintenance. Verifies every persisted artifact at the end.
//!
//! The same test body runs against both storage backends:
//!   - SQLite: always runs (uses a tempfile DB).
//!   - Postgres: runs only when `ATOMIC_TEST_DATABASE_URL` is set and the
//!     `postgres` feature is on.

mod support;

use atomic_core::{AtomicCore, CreateAtomRequest, EmbeddingEvent, UpdateAtomRequest};
use support::{
    await_pipeline, chunk_ids_for_atom, event_collector, open_bare, pending_pipeline_job_count,
    setup_core, Backend, EventRx, MockAiServer, EDGE_SIMILARITY_THRESHOLD,
};

#[tokio::test]
async fn full_pipeline_sqlite() {
    run_full_pipeline(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn full_pipeline_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("full_pipeline_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_full_pipeline(Backend::Postgres).await;
}

async fn run_full_pipeline(backend: Backend) {
    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;

    // Two atoms sharing most vocabulary. The bag-of-words mock embedder
    // lands them at overlapping positions so their cosine similarity clears
    // the 0.5 edge threshold. Keep both short so they produce a single
    // chunk each — simplifies assertions and keeps the request count
    // predictable.
    let atom_a = create_and_await(
        core,
        "quantum mechanics is the study of particles and waves at atomic scales",
    )
    .await;
    let atom_b = create_and_await(
        core,
        "quantum physics explores particles waves and the strange behavior of atomic systems",
    )
    .await;
    let deferred_edges = core
        .get_semantic_edges(EDGE_SIMILARITY_THRESHOLD)
        .await
        .unwrap();
    assert!(
        deferred_edges.is_empty(),
        "semantic edges should wait for graph maintenance; got {:?}",
        deferred_edges
    );
    run_graph_maintenance(core).await;

    // --- Embedding phase: status flipped to complete on both atoms ---
    let fetched_a = core
        .get_atom(&atom_a)
        .await
        .unwrap()
        .expect("atom_a persisted");
    let fetched_b = core
        .get_atom(&atom_b)
        .await
        .unwrap()
        .expect("atom_b persisted");
    assert_eq!(
        fetched_a.atom.embedding_status, "complete",
        "atom_a embedding should be complete"
    );
    assert_eq!(
        fetched_b.atom.embedding_status, "complete",
        "atom_b embedding should be complete"
    );

    // --- Tagging phase: the mock returned Physics→Topics, and the pipeline
    // wired the extracted tag up to the atom. The tag row must also carry
    // the correct parent linkage.
    assert!(
        !fetched_a.tags.is_empty(),
        "atom_a should have at least one tag after tagging: {:?}",
        fetched_a.tags
    );
    let physics_tag = fetched_a
        .tags
        .iter()
        .find(|t| t.name == "Physics")
        .expect("expected a Physics tag applied to atom_a");
    let topics = core
        .get_all_tags()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.tag.name == "Topics")
        .expect("Topics category should exist");
    assert_eq!(
        physics_tag.parent_id,
        Some(topics.tag.id.clone()),
        "Physics should hang off Topics, got parent_id {:?}",
        physics_tag.parent_id
    );

    // --- Semantic edge phase: an edge between A and B crosses the 0.5
    // threshold. With B created second, the edge is stored source=B→A.
    let edges = core
        .get_semantic_edges(EDGE_SIMILARITY_THRESHOLD)
        .await
        .unwrap();
    let edge = edges.iter().find(|e| {
        (e.source_atom_id == atom_a && e.target_atom_id == atom_b)
            || (e.source_atom_id == atom_b && e.target_atom_id == atom_a)
    });
    let edge = edge.unwrap_or_else(|| {
        panic!(
            "expected a semantic edge between atom_a ({atom_a}) and atom_b ({atom_b}); \
             got {} edges total: {:?}",
            edges.len(),
            edges
        )
    });
    assert!(
        edge.similarity_score >= EDGE_SIMILARITY_THRESHOLD,
        "edge similarity should clear the threshold, got {}",
        edge.similarity_score
    );
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
        .expect("atom was inserted (not skipped)");
    await_pipeline(&mut rx, &created.atom.id).await;
    created.atom.id
}

async fn run_graph_maintenance(core: &AtomicCore) {
    core.process_graph_maintenance()
        .await
        .expect("graph maintenance");
}

async fn await_queue_completed(rx: &mut EventRx) -> Vec<EmbeddingEvent> {
    let mut captured = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "queue did not complete within 15s. Captured: {:?}",
                captured
            );
        }

        let ev = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => panic!("event channel closed before queue completed"),
            Err(_) => panic!(
                "timed out waiting for queue completion. Captured: {:?}",
                captured
            ),
        };
        let completed = matches!(ev, EmbeddingEvent::PipelineQueueCompleted { .. });
        captured.push(ev);
        if completed {
            return captured;
        }
    }
}

// ==================== Queue modes and progress ====================

#[tokio::test]
async fn queued_embedding_missing_provider_marks_failed_sqlite() {
    run_queued_embedding_missing_provider_marks_failed(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn queued_embedding_missing_provider_marks_failed_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "queued_embedding_missing_provider_marks_failed_postgres: skipping \
             (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_queued_embedding_missing_provider_marks_failed(Backend::Postgres).await;
}

async fn run_queued_embedding_missing_provider_marks_failed(backend: Backend) {
    let handle = open_bare(backend).await.expect("open bare core");
    let core = &handle.core;

    let (cb, mut rx) = event_collector();
    let created = core
        .create_atom(
            CreateAtomRequest {
                content: "embedding should fail before provider call".to_string(),
                ..Default::default()
            },
            cb,
        )
        .await
        .expect("create_atom")
        .expect("atom inserted");
    let events = await_queue_completed(&mut rx).await;

    assert!(
        events.iter().any(|ev| matches!(
            ev,
            EmbeddingEvent::EmbeddingFailed { atom_id, .. } if atom_id == &created.atom.id
        )),
        "missing OpenRouter key should emit an embedding failure: {:?}",
        events
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            EmbeddingEvent::PipelineQueueCompleted {
                total_jobs: 1,
                failed_jobs: 1,
                ..
            }
        )),
        "queue completion should count the early provider exit as failed: {:?}",
        events
    );

    let after = core
        .get_atom(&created.atom.id)
        .await
        .expect("get atom")
        .expect("atom exists");
    assert_eq!(after.atom.embedding_status, "failed");
    assert!(
        after.atom.embedding_error.is_some(),
        "failed embedding status should persist an error"
    );

    assert_eq!(
        pending_pipeline_job_count(core).await,
        0,
        "terminal failed job should be cleared from atom_pipeline_jobs"
    );
}

#[tokio::test]
async fn queue_progress_reports_tagging_only_after_embedding_sqlite() {
    run_queue_progress_reports_tagging_only_after_embedding(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn queue_progress_reports_tagging_only_after_embedding_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "queue_progress_reports_tagging_only_after_embedding_postgres: skipping \
             (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_queue_progress_reports_tagging_only_after_embedding(Backend::Postgres).await;
}

async fn run_queue_progress_reports_tagging_only_after_embedding(backend: Backend) {
    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;

    let (cb, mut rx) = event_collector();
    let result = core
        .create_atoms_bulk(
            vec![
                CreateAtomRequest {
                    content: "quantum particles waves".to_string(),
                    ..Default::default()
                },
                CreateAtomRequest {
                    content: "biology cells dna".to_string(),
                    ..Default::default()
                },
                CreateAtomRequest {
                    content: "cooking pasta sauce".to_string(),
                    ..Default::default()
                },
            ],
            cb,
        )
        .await
        .expect("bulk create");
    assert_eq!(result.count, 3);

    let events = await_queue_completed(&mut rx).await;
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            EmbeddingEvent::PipelineQueueStarted {
                total_jobs: 3,
                embedding_total: 3,
                ..
            }
        )),
        "queue start event should report 3 total jobs and 3 embedding jobs: {:?}",
        events
    );

    let embedding_done_index = events
        .iter()
        .position(|ev| {
            matches!(
                ev,
                EmbeddingEvent::PipelineQueueProgress {
                    stage,
                    completed: 3,
                    total: 3,
                    ..
                } if stage == "embedding"
            )
        })
        .expect("expected terminal embedding progress");
    let first_tagging_index = events
        .iter()
        .position(|ev| {
            matches!(
                ev,
                EmbeddingEvent::PipelineQueueProgress {
                    stage,
                    total: 3,
                    ..
                } if stage == "tagging"
            )
        })
        .expect("expected tagging progress");
    assert!(
        first_tagging_index > embedding_done_index,
        "tagging progress should be announced after embedding finishes: {:?}",
        events
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            EmbeddingEvent::PipelineQueueProgress {
                stage,
                completed: 3,
                total: 3,
                ..
            } if stage == "tagging"
        )),
        "tagging should complete 3/3 after eligible atoms are known: {:?}",
        events
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            EmbeddingEvent::PipelineQueueCompleted {
                total_jobs: 3,
                failed_jobs: 0,
                ..
            }
        )),
        "queue completion should report no failures: {:?}",
        events
    );
}

#[tokio::test]
async fn reembed_all_is_embedding_only_sqlite() {
    run_reembed_all_is_embedding_only(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn reembed_all_is_embedding_only_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "reembed_all_is_embedding_only_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_reembed_all_is_embedding_only(Backend::Postgres).await;
}

async fn run_reembed_all_is_embedding_only(backend: Backend) {
    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;
    let atom_id = create_and_await(core, "quantum mechanics particles waves").await;
    let before = core.get_atom(&atom_id).await.unwrap().expect("atom exists");
    assert!(
        before.tags.iter().any(|tag| tag.name == "Physics"),
        "initial auto-tagging should apply Physics: {:?}",
        before.tags
    );
    let chunk_ids_before = chunk_ids_for_atom(core, &atom_id).await;

    mock.reset_counts();
    let (cb, mut rx) = event_collector();
    let queued = core.reembed_all_atoms(cb).await.expect("reembed all");
    assert_eq!(queued, 1);
    let events = await_queue_completed(&mut rx).await;

    assert!(
        mock.embedding_request_count() > 0,
        "re-embedding should call the embedding provider"
    );
    assert_eq!(
        mock.chat_request_count(),
        0,
        "embed-only re-embedding must not call the tagging LLM"
    );
    assert!(
        events.iter().all(|ev| {
            !matches!(
                ev,
                EmbeddingEvent::TaggingComplete { .. }
                    | EmbeddingEvent::TaggingSkipped { .. }
                    | EmbeddingEvent::TaggingFailed { .. }
            ) && !matches!(
                ev,
                EmbeddingEvent::PipelineQueueProgress { stage, .. } if stage == "tagging"
            )
        }),
        "embed-only re-embedding should not emit tagging progress or events: {:?}",
        events
    );

    let after = core.get_atom(&atom_id).await.unwrap().expect("atom exists");
    assert_eq!(after.atom.embedding_status, "complete");
    assert_eq!(after.atom.tagging_status, "complete");
    assert!(
        after.tags.iter().any(|tag| tag.name == "Physics"),
        "existing tags should be preserved by embed-only re-embedding: {:?}",
        after.tags
    );
    assert_eq!(
        chunk_ids_for_atom(core, &atom_id).await,
        chunk_ids_before,
        "embed-only re-embedding should preserve existing chunk rows"
    );
}

#[tokio::test]
async fn embedding_model_change_reembeds_existing_chunks_sqlite() {
    run_embedding_model_change_reembeds_existing_chunks(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn embedding_model_change_reembeds_existing_chunks_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "embedding_model_change_reembeds_existing_chunks_postgres: skipping \
             (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_embedding_model_change_reembeds_existing_chunks(Backend::Postgres).await;
}

async fn run_embedding_model_change_reembeds_existing_chunks(backend: Backend) {
    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;
    let atom_id = create_and_await(core, "quantum mechanics particles waves").await;
    let chunk_ids_before = chunk_ids_for_atom(core, &atom_id).await;

    mock.reset_counts();
    let (cb, mut rx) = event_collector();
    let result = core
        .set_setting_with_reembed("openai_compat_embedding_model", "mock-embed-v2", cb)
        .await
        .expect("change embedding model");
    assert!(result.embedding_space_changed);
    assert!(!result.dimension_changed);
    assert_eq!(result.total_atom_count, 1);
    let events = await_queue_completed(&mut rx).await;

    assert!(
        mock.embedding_request_count() > 0,
        "embedding model changes should re-embed existing chunks"
    );
    assert_eq!(
        mock.chat_request_count(),
        0,
        "embedding model changes should not re-run auto-tagging"
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            EmbeddingEvent::PipelineQueueCompleted {
                total_jobs: 1,
                failed_jobs: 0,
                ..
            }
        )),
        "embedding model change should complete one embed-only queue job: {:?}",
        events
    );
    assert_eq!(
        chunk_ids_for_atom(core, &atom_id).await,
        chunk_ids_before,
        "embedding model changes should preserve existing chunk rows"
    );
}

#[tokio::test]
async fn retry_tagging_is_tagging_only_sqlite() {
    run_retry_tagging_is_tagging_only(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn retry_tagging_is_tagging_only_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "retry_tagging_is_tagging_only_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_retry_tagging_is_tagging_only(Backend::Postgres).await;
}

async fn run_retry_tagging_is_tagging_only(backend: Backend) {
    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;
    let atom_id = create_and_await(core, "biology cells dna evolution").await;

    mock.reset_counts();
    let (cb, mut rx) = event_collector();
    core.retry_tagging(&atom_id, cb)
        .await
        .expect("retry tagging");
    let events = await_queue_completed(&mut rx).await;

    assert_eq!(
        mock.embedding_request_count(),
        0,
        "tagging-only retry must not call the embedding provider"
    );
    assert!(
        mock.chat_request_count() > 0,
        "tagging-only retry should call the tagging LLM"
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            EmbeddingEvent::PipelineQueueStarted {
                total_jobs: 1,
                embedding_total: 0,
                ..
            }
        )),
        "tag-only queue run should report zero embedding work: {:?}",
        events
    );
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            EmbeddingEvent::PipelineQueueProgress {
                stage,
                completed: 1,
                total: 1,
                ..
            } if stage == "tagging"
        )),
        "tagging-only run should report 1/1 tagging progress: {:?}",
        events
    );

    let after = core.get_atom(&atom_id).await.unwrap().expect("atom exists");
    assert_eq!(after.atom.embedding_status, "complete");
    assert_eq!(after.atom.tagging_status, "complete");
    assert!(
        after.tags.iter().any(|tag| tag.name == "Biology"),
        "tagging retry should apply the content-derived Biology tag: {:?}",
        after.tags
    );
}

// ==================== Update lifecycle ====================

#[tokio::test]
async fn update_lifecycle_sqlite() {
    run_update_lifecycle(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn update_lifecycle_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("update_lifecycle_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_update_lifecycle(Backend::Postgres).await;
}

/// Editing an atom's content must re-run both halves of the pipeline:
/// embeddings/chunks/edges and auto-tagging. This test swaps vocabulary
/// completely, verifies the old semantic edge disappears, and proves the
/// tagger actually ran again by expecting a new content-derived tag.
async fn run_update_lifecycle(backend: Backend) {
    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;

    // Vocabulary A — atoms a and b land near each other.
    let a = create_and_await(core, "quantum mechanics particles waves atomic scales").await;
    let b = create_and_await(core, "quantum waves physics atomic particles systems").await;
    run_graph_maintenance(core).await;

    // Sanity: edge exists before update so the delete-after-update
    // assertion is actually meaningful.
    let initial_edges = core
        .get_semantic_edges(EDGE_SIMILARITY_THRESHOLD)
        .await
        .unwrap();
    assert!(
        initial_edges.iter().any(|e| involves(e, &a, &b)),
        "expected edge between a and b before update; got {:?}",
        initial_edges
    );

    // Replace a's content with disjoint vocabulary. The bag-of-words embedder
    // will place it far from b, so the old edge must be cleaned up.
    let (cb, mut rx) = event_collector();
    let new_content = "biology cells organisms dna evolution".to_string();
    core.update_atom(
        &a,
        UpdateAtomRequest {
            content: new_content.clone(),
            source_url: None,
            published_at: None,
            tag_ids: None,
        },
        cb,
    )
    .await
    .expect("update_atom");
    await_pipeline(&mut rx, &a).await;
    run_graph_maintenance(core).await;

    let a_after = core.get_atom(&a).await.unwrap().expect("a still exists");
    assert_eq!(a_after.atom.content, new_content);
    assert_eq!(a_after.atom.embedding_status, "complete");
    assert_eq!(a_after.atom.tagging_status, "complete");
    assert!(
        a_after.tags.iter().any(|t| t.name == "Physics"),
        "tags should be preserved across update; got {:?}",
        a_after.tags
    );
    assert!(
        a_after.tags.iter().any(|t| t.name == "Biology"),
        "updated content should trigger a fresh tagging pass; got {:?}",
        a_after.tags
    );

    let edges_after = core
        .get_semantic_edges(EDGE_SIMILARITY_THRESHOLD)
        .await
        .unwrap();
    assert!(
        !edges_after
            .iter()
            .any(|e| e.source_atom_id == a || e.target_atom_id == a),
        "no edges should reference a after its vocabulary swap; got {:?}",
        edges_after
    );
}

#[tokio::test]
async fn draft_save_then_finalize_sqlite() {
    run_draft_save_then_finalize(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn draft_save_then_finalize_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("draft_save_then_finalize_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_draft_save_then_finalize(Backend::Postgres).await;
}

async fn run_draft_save_then_finalize(backend: Backend) {
    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;

    let atom_id = create_and_await(core, "quantum mechanics particles waves atomic scales").await;

    core.update_atom_content_only(
        &atom_id,
        UpdateAtomRequest {
            content: "biology cells organisms dna evolution".to_string(),
            source_url: None,
            published_at: None,
            tag_ids: None,
        },
    )
    .await
    .expect("draft save should succeed");

    let after_draft = core.get_atom(&atom_id).await.unwrap().expect("atom exists");
    assert_eq!(after_draft.atom.embedding_status, "pending");
    assert_eq!(after_draft.atom.tagging_status, "pending");

    let (cb, mut rx) = event_collector();
    core.process_atom_pipeline(&atom_id, cb)
        .await
        .expect("finalize pipeline");
    await_pipeline(&mut rx, &atom_id).await;

    let finalized = core.get_atom(&atom_id).await.unwrap().expect("atom exists");
    assert_eq!(finalized.atom.embedding_status, "complete");
    assert_eq!(finalized.atom.tagging_status, "complete");
    assert!(
        finalized.tags.iter().any(|t| t.name == "Biology"),
        "finalized draft should have fresh biology tag: {:?}",
        finalized.tags
    );
}

// ==================== Delete cascade ====================

#[tokio::test]
async fn delete_cascade_sqlite() {
    run_delete_cascade(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn delete_cascade_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("delete_cascade_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_delete_cascade(Backend::Postgres).await;
}

/// Deleting an atom must cascade: the atom row, its chunk/embedding rows, and
/// every semantic edge it participates in. Tags survive — they're shared
/// state and may be attached to other atoms.
async fn run_delete_cascade(backend: Backend) {
    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;

    let a = create_and_await(core, "apple banana cherry mango lychee").await;
    let b = create_and_await(core, "apple banana cherry dragonfruit lychee").await;
    run_graph_maintenance(core).await;

    // Capture the Physics tag id off one of the atoms before deletion so we
    // can check the tag row itself survives. `get_all_tags` only returns
    // top-level rows with children nested inside — simpler to grab the
    // applied tag straight from the atom.
    let a_before = core.get_atom(&a).await.unwrap().expect("a persisted");
    let physics_id = a_before
        .tags
        .iter()
        .find(|t| t.name == "Physics")
        .expect("Physics tag should be applied to a")
        .id
        .clone();

    let initial_edges = core
        .get_semantic_edges(EDGE_SIMILARITY_THRESHOLD)
        .await
        .unwrap();
    assert!(
        initial_edges.iter().any(|e| involves(e, &a, &b)),
        "expected edge between a and b before delete; got {:?}",
        initial_edges
    );

    core.delete_atom(&a).await.expect("delete_atom");

    // Atom row gone; other atoms untouched.
    assert!(
        core.get_atom(&a).await.unwrap().is_none(),
        "a should be gone"
    );
    assert!(
        core.get_atom(&b).await.unwrap().is_some(),
        "b should survive deletion of a"
    );

    // Edges referencing a are cascaded out on both sides of the relation.
    let edges_after = core
        .get_semantic_edges(EDGE_SIMILARITY_THRESHOLD)
        .await
        .unwrap();
    assert!(
        !edges_after
            .iter()
            .any(|e| e.source_atom_id == a || e.target_atom_id == a),
        "no edges should reference the deleted atom; got {:?}",
        edges_after
    );

    // Physics tag is shared state — still present, now only linked to b.
    let remaining = core
        .get_atoms_by_tag(&physics_id, &atomic_core::models::KindFilter::All)
        .await
        .expect("get_atoms_by_tag");
    let ids: Vec<String> = remaining.iter().map(|a| a.atom.id.clone()).collect();
    assert_eq!(
        ids,
        vec![b.clone()],
        "Physics tag should list only b after a is deleted; got {:?}",
        ids
    );
}

fn involves(edge: &atomic_core::SemanticEdge, a: &str, b: &str) -> bool {
    (edge.source_atom_id == a && edge.target_atom_id == b)
        || (edge.source_atom_id == b && edge.target_atom_id == a)
}

// ==================== Cross-backend search threshold parity ====================

#[tokio::test]
async fn search_threshold_parity_sqlite() {
    run_search_threshold_parity(Backend::Sqlite).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn search_threshold_parity_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("search_threshold_parity_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_search_threshold_parity(Backend::Postgres).await;
}

/// Both backends measure cosine similarity, but their underlying distance
/// operators differ: sqlite-vec returns Euclidean distance on normalized
/// vectors (similarity = 1 − d²/2), pgvector's `<=>` returns cosine distance
/// directly (similarity = 1 − d). For normalized embeddings the two converge
/// on the same number, so the same threshold must admit the same atom set on
/// either backend. The bag-of-words mock embedder normalizes by construction,
/// making the input shape match production usage.
///
/// This test pins that contract: three atoms (two physics-vocab, one biology),
/// a physics-vocab query, threshold 0.5. The two physics atoms must clear the
/// threshold, the biology atom must not, and the top score must be inside a
/// generous-but-bounded interval so a metric regression on either side trips
/// the assertion.
async fn run_search_threshold_parity(backend: Backend) {
    use atomic_core::{SearchMode, SearchOptions};

    let mock = MockAiServer::start().await;
    let handle = setup_core(backend, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;

    let physics_a = create_and_await(core, "quantum particles atomic waves momentum spin").await;
    let physics_b = create_and_await(
        core,
        "atomic particles quantum waves spin momentum scattering",
    )
    .await;
    let _biology = create_and_await(core, "biology cells dna evolution organisms genetics").await;

    let options = SearchOptions {
        query: "quantum particles atomic waves".to_string(),
        mode: SearchMode::Semantic,
        limit: 10,
        threshold: 0.5,
        ..Default::default()
    };
    let results = core.search(options).await.expect("semantic search");

    let returned: std::collections::HashSet<String> =
        results.iter().map(|r| r.atom.atom.id.clone()).collect();
    let expected: std::collections::HashSet<String> =
        [physics_a.clone(), physics_b.clone()].into_iter().collect();

    assert_eq!(
        returned,
        expected,
        "threshold=0.5 should admit exactly the two physics atoms on both backends; \
         got scores={:?}",
        results
            .iter()
            .map(|r| (r.atom.atom.id.clone(), r.similarity_score))
            .collect::<Vec<_>>()
    );

    // Sanity-check the score range. Self-similarity of a normalized unit vector
    // is 1.0; near-duplicates land in [0.5, 1.0]. A score outside that range
    // means the distance→similarity transform is wrong on this backend.
    let top_score = results
        .iter()
        .map(|r| r.similarity_score)
        .fold(f32::NEG_INFINITY, f32::max);
    assert!(
        (0.5..=1.0).contains(&top_score),
        "top similarity {} should land in [0.5, 1.0] for a normalized cosine metric",
        top_score
    );
}

// ==================== Deferred pipeline execution ====================

/// With inline pipeline execution switched off (`set_inline_pipeline(false)`
/// — the knob hosts with a dedicated pipeline worker use), an atom save
/// still enqueues its durable `atom_pipeline_jobs` row and returns normally,
/// but nothing in this process claims or executes it: no lease is taken, no
/// pipeline events fire, and the atom's embedding status stays `pending`.
/// The parked job is then proven executable — restoring inline mode and
/// draining the queue completes it — pinning the contract that jobs persist
/// in the ledger either way and a deferred job is never lost or leased.
///
/// The default-on behavior (inline execution byte-identical to before the
/// knob existed) is pinned by every other test in this suite.
#[tokio::test]
async fn deferred_pipeline_leaves_jobs_in_ledger() {
    let mock = MockAiServer::start().await;
    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("test harness setup");
    let core = &handle.core;

    assert!(core.inline_pipeline(), "inline execution is the default");
    core.set_inline_pipeline(false);

    let (cb, mut rx) = event_collector();
    let created = core
        .create_atom(
            CreateAtomRequest {
                content: "quantum mechanics is the study of particles and waves".to_string(),
                ..Default::default()
            },
            cb,
        )
        .await
        .expect("create_atom")
        .expect("atom was inserted (not skipped)");
    let atom_id = created.atom.id.clone();

    // The save returned with the job parked in the durable ledger.
    assert_eq!(
        support::pending_pipeline_job_count(core).await,
        1,
        "deferred save must leave its pipeline job in the ledger"
    );

    // Nothing processed: status untouched, no events. The settle window
    // catches a stray spawned task that would otherwise race the asserts.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let fetched = core.get_atom(&atom_id).await.unwrap().expect("persisted");
    assert_eq!(
        fetched.atom.embedding_status, "pending",
        "deferred save must not execute the pipeline"
    );
    assert!(
        rx.try_recv().is_err(),
        "deferred save must emit no pipeline events"
    );

    // The parked job stays claimable: a worker (here: inline mode restored
    // and the queue drained) runs it to completion.
    core.set_inline_pipeline(true);
    let (cb, mut rx) = event_collector();
    let queued = core
        .process_queued_pipeline_jobs(cb)
        .await
        .expect("drain queue");
    assert_eq!(queued, 1, "the deferred job must still be claimable");
    await_pipeline(&mut rx, &atom_id).await;

    let fetched = core.get_atom(&atom_id).await.unwrap().expect("persisted");
    assert_eq!(fetched.atom.embedding_status, "complete");
    assert_eq!(
        support::pending_pipeline_job_count(core).await,
        0,
        "executed job must be cleared from the ledger"
    );
}
