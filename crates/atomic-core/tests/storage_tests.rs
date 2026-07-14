//! Parameterized storage tests that run against both SQLite and Postgres backends.
//!
//! SQLite tests always run. Postgres tests require:
//!   - The `postgres` feature enabled
//!   - `ATOMIC_TEST_DATABASE_URL` env var set to a Postgres connection string
//!
//! Usage:
//!   cargo test -p atomic-core --test storage_tests                         # SQLite only
//!   cargo test -p atomic-core --test storage_tests --features postgres     # Both
//!
//! Note: Postgres tests must run serially (they share one DB):
//!   cargo test -p atomic-core --test storage_tests --features postgres -- postgres_tests --test-threads=1

use atomic_core::db::Database;
use atomic_core::models::*;
use atomic_core::storage::traits::*;
use atomic_core::storage::SqliteStorage;
use atomic_core::{AtomicCoreError, CreateAtomRequest, ListAtomsParams, UpdateAtomRequest};
use std::sync::Arc;
use tempfile::TempDir;

// ==================== Test Helpers ====================

async fn sqlite_storage() -> (SqliteStorage, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Database::open_or_create(dir.path().join("test.db")).unwrap();
    let storage = SqliteStorage::new(Arc::new(db));
    (storage, dir)
}

#[cfg(feature = "postgres")]
async fn postgres_storage() -> Option<atomic_core::storage::PostgresStorage> {
    let url = match std::env::var("ATOMIC_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return None,
    };
    let storage = atomic_core::storage::PostgresStorage::connect(&url, "test")
        .await
        .unwrap();
    storage.initialize().await.unwrap();

    // Truncate data tables for a clean test (preserve schema)
    sqlx::raw_sql(
        "TRUNCATE atoms, tags, atom_tags, atom_chunks, atom_positions, atom_pipeline_jobs, \
         semantic_edges, atom_clusters, tag_embeddings, \
         wiki_articles, wiki_citations, wiki_links, wiki_article_versions, atom_links, \
         conversations, conversation_tags, chat_messages, chat_tool_calls, chat_citations, \
         feeds, feed_tags, feed_items, settings, task_runs, \
         briefing_citations, briefings, oauth_codes, oauth_clients, api_tokens \
         CASCADE",
    )
    .execute(storage.pool())
    .await
    .ok();

    Some(storage)
}

// ==================== AtomStore Tests ====================

async fn test_create_and_get_atom(storage: &dyn AtomStore) {
    let request = CreateAtomRequest {
        content: "# Test Atom\n\nThis is a test.".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: vec![],
        ..Default::default()
    };

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let created = storage.insert_atom(&id, &request, &now).await.unwrap();

    assert_eq!(created.atom.id, id);
    assert_eq!(created.atom.content, "# Test Atom\n\nThis is a test.");
    assert_eq!(created.atom.embedding_status, "pending");

    // Retrieve it
    let fetched = storage.get_atom(&id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.atom.id, id);
    assert_eq!(fetched.atom.content, created.atom.content);
}

async fn test_get_atom_not_found(storage: &dyn AtomStore) {
    let result = storage.get_atom("nonexistent-id").await.unwrap();
    assert!(result.is_none());
}

async fn test_delete_atom(storage: &dyn AtomStore) {
    let request = CreateAtomRequest {
        content: "To be deleted".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: vec![],
        ..Default::default()
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    storage.insert_atom(&id, &request, &now).await.unwrap();

    storage.delete_atom(&id).await.unwrap();
    let result = storage.get_atom(&id).await.unwrap();
    assert!(result.is_none());
}

async fn test_update_atom(storage: &dyn AtomStore) {
    let request = CreateAtomRequest {
        content: "Original content".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: vec![],
        ..Default::default()
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    storage.insert_atom(&id, &request, &now).await.unwrap();

    let update = UpdateAtomRequest {
        content: "Updated content".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: None,
    };
    let updated = storage.update_atom(&id, &update, &now).await.unwrap();
    assert_eq!(updated.atom.content, "Updated content");

    let fetched = storage.get_atom(&id).await.unwrap().unwrap();
    assert_eq!(fetched.atom.content, "Updated content");
}

async fn test_update_atom_if_unchanged_rejects_stale_write(storage: &dyn AtomStore) {
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = "2024-01-01T00:00:00Z";
    storage
        .insert_atom(
            &id,
            &CreateAtomRequest {
                content: "Original content".to_string(),
                ..Default::default()
            },
            created_at,
        )
        .await
        .unwrap();

    let update = UpdateAtomRequest {
        content: "First edit".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: None,
    };
    storage
        .update_atom_if_unchanged(&id, &update, "2024-01-02T00:00:00Z", created_at)
        .await
        .unwrap();

    let stale_update = UpdateAtomRequest {
        content: "Stale edit".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: None,
    };
    let error = storage
        .update_atom_if_unchanged(&id, &stale_update, "2024-01-03T00:00:00Z", created_at)
        .await
        .unwrap_err();

    assert!(matches!(error, AtomicCoreError::Conflict(_)));
    let fetched = storage.get_atom(&id).await.unwrap().unwrap();
    assert_eq!(fetched.atom.content, "First edit");
}

async fn test_atom_links_materialized(storage: &dyn AtomStore) {
    let target_id = uuid::Uuid::new_v4().to_string();
    let source_id = uuid::Uuid::new_v4().to_string();
    let missing_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    storage
        .insert_atom(
            &target_id,
            &CreateAtomRequest {
                content: "# Target Atom\n\nBody".to_string(),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &source_id,
            &CreateAtomRequest {
                content: format!(
                    "Links: [[{}|Target label]], [[{}]], [[future-slug]]",
                    target_id, missing_id
                ),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();

    let links = storage.get_atom_links(&source_id).await.unwrap();
    assert_eq!(links.len(), 3);
    assert_eq!(links[0].target_atom_id.as_deref(), Some(target_id.as_str()));
    assert_eq!(links[0].target_title.as_deref(), Some("Target Atom"));
    assert_eq!(links[0].label.as_deref(), Some("Target label"));
    assert_eq!(links[0].target_kind, "atom_id");
    assert_eq!(links[0].status, "resolved");
    assert_eq!(links[1].raw_target, missing_id);
    assert_eq!(links[1].status, "missing");
    assert_eq!(links[2].raw_target, "future-slug");
    assert_eq!(links[2].target_kind, "text");
    assert_eq!(links[2].status, "unresolved");
}

async fn test_atom_links_replaced_on_update(storage: &dyn AtomStore) {
    let target_id = uuid::Uuid::new_v4().to_string();
    let source_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    storage
        .insert_atom(
            &target_id,
            &CreateAtomRequest {
                content: "# Target".to_string(),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &source_id,
            &CreateAtomRequest {
                content: format!("[[{}]]", target_id),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();

    assert_eq!(storage.get_atom_links(&source_id).await.unwrap().len(), 1);

    storage
        .update_atom_content_only(
            &source_id,
            &UpdateAtomRequest {
                content: "No links now".to_string(),
                source_url: None,
                published_at: None,
                tag_ids: None,
            },
            &chrono::Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();

    assert!(storage.get_atom_links(&source_id).await.unwrap().is_empty());
}

async fn test_atom_links_mark_target_missing_on_delete(storage: &dyn AtomStore) {
    let target_id = uuid::Uuid::new_v4().to_string();
    let source_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    storage
        .insert_atom(
            &target_id,
            &CreateAtomRequest {
                content: "# Target".to_string(),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &source_id,
            &CreateAtomRequest {
                content: format!("[[{}|Target]]", target_id),
                ..Default::default()
            },
            &now,
        )
        .await
        .unwrap();

    storage.delete_atom(&target_id).await.unwrap();

    let links = storage.get_atom_links(&source_id).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].raw_target, target_id);
    assert_eq!(links[0].target_atom_id, None);
    assert_eq!(links[0].status, "missing");
}

async fn test_atom_link_suggestions_recent_and_title_ranked(storage: &dyn AtomStore) {
    let older = "2024-01-01T00:00:00Z";
    let newer = "2024-01-02T00:00:00Z";
    let newest = "2024-01-03T00:00:00Z";

    let exact_id = uuid::Uuid::new_v4().to_string();
    let prefix_id = uuid::Uuid::new_v4().to_string();
    let contains_id = uuid::Uuid::new_v4().to_string();
    let body_only_id = uuid::Uuid::new_v4().to_string();

    storage
        .insert_atom(
            &prefix_id,
            &CreateAtomRequest {
                content: "# Project Atlas Notes\n\nBody".to_string(),
                ..Default::default()
            },
            older,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &contains_id,
            &CreateAtomRequest {
                content: "# Notes for Project Atlas\n\nBody".to_string(),
                ..Default::default()
            },
            newer,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &exact_id,
            &CreateAtomRequest {
                content: "# Project Atlas\n\nBody".to_string(),
                ..Default::default()
            },
            older,
        )
        .await
        .unwrap();
    storage
        .insert_atom(
            &body_only_id,
            &CreateAtomRequest {
                content: "# Unrelated\n\nProject Atlas only appears in body.".to_string(),
                ..Default::default()
            },
            newest,
        )
        .await
        .unwrap();

    let recent = storage.suggest_atom_links("", 2).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].id, body_only_id);
    assert_eq!(recent[1].id, contains_id);

    let matches = storage
        .suggest_atom_links("project atlas", 10)
        .await
        .unwrap();
    let ids: Vec<&str> = matches.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![exact_id, prefix_id, contains_id]);
    assert!(!ids.contains(&body_only_id.as_str()));
}

async fn test_get_all_atoms(storage: &dyn AtomStore) {
    let now = chrono::Utc::now().to_rfc3339();
    for i in 0..3 {
        let request = CreateAtomRequest {
            content: format!("Atom {}", i),
            source_url: None,
            published_at: None,
            tag_ids: vec![],
            ..Default::default()
        };
        let id = uuid::Uuid::new_v4().to_string();
        storage.insert_atom(&id, &request, &now).await.unwrap();
    }

    let all = storage.get_all_atoms().await.unwrap();
    assert!(all.len() >= 3);
}

async fn test_list_atoms_pagination(storage: &dyn AtomStore) {
    let now = chrono::Utc::now().to_rfc3339();
    for i in 0..5 {
        let request = CreateAtomRequest {
            content: format!("Paginated atom {}", i),
            source_url: None,
            published_at: None,
            tag_ids: vec![],
            ..Default::default()
        };
        let id = uuid::Uuid::new_v4().to_string();
        storage.insert_atom(&id, &request, &now).await.unwrap();
    }

    let params = ListAtomsParams {
        tag_id: None,
        limit: 2,
        offset: 0,
        cursor: None,
        cursor_id: None,
        source_filter: SourceFilter::All,
        source_value: None,
        sort_by: SortField::Updated,
        sort_order: SortOrder::Desc,
    };

    let page = storage
        .list_atoms(&params, &atomic_core::models::KindFilter::All)
        .await
        .unwrap();
    assert_eq!(page.atoms.len(), 2);
    assert!(page.total_count >= 5);
}

// ==================== TagStore Tests ====================

async fn test_create_and_get_tags(storage: &dyn TagStore) {
    let tag = storage.create_tag("Test Tag", None).await.unwrap();
    assert_eq!(tag.name, "Test Tag");
    assert!(tag.parent_id.is_none());

    let child = storage
        .create_tag("Child Tag", Some(&tag.id))
        .await
        .unwrap();
    assert_eq!(child.parent_id.as_deref(), Some(tag.id.as_str()));

    // get_all_tags returns a tree — flatten to count
    let all_tags = storage.get_all_tags().await.unwrap();
    fn count_tree(tags: &[TagWithCount]) -> usize {
        tags.iter().map(|t| 1 + count_tree(&t.children)).sum()
    }
    assert!(count_tree(&all_tags) >= 2);
}

async fn test_update_tag(storage: &dyn TagStore) {
    let tag = storage.create_tag("Old Name", None).await.unwrap();
    let updated = storage.update_tag(&tag.id, "New Name", None).await.unwrap();
    assert_eq!(updated.name, "New Name");
}

async fn test_delete_tag(storage: &dyn TagStore) {
    let tag = storage.create_tag("Doomed", None).await.unwrap();
    storage.delete_tag(&tag.id, false).await.unwrap();

    // Tag should be gone from get_all_tags
    let tags = storage.get_all_tags().await.unwrap();
    assert!(!tags.iter().any(|t| t.tag.id == tag.id));
}

async fn test_get_tag(storage: &dyn TagStore) {
    let tag = storage.create_tag("Lookup Tag", None).await.unwrap();
    let fetched = storage.get_tag(&tag.id).await.unwrap();
    assert_eq!(
        fetched.as_ref().map(|t| t.name.as_str()),
        Some("Lookup Tag")
    );
    assert_eq!(fetched.unwrap().id, tag.id);

    let missing = storage.get_tag("nonexistent-tag-id").await.unwrap();
    assert!(missing.is_none());
}

// ==================== TaskRunStore Tests ====================

/// Build a `TaskRun` row with caller-controlled state/timing fields —
/// `insert_task_run`'s contract is that the caller owns every column.
fn task_run_row(
    task_id: &str,
    subject_id: &str,
    state: TaskRunState,
    next_attempt_at: &str,
    lease_until: Option<&str>,
) -> TaskRun {
    let now = chrono::Utc::now().to_rfc3339();
    TaskRun {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        subject_id: Some(subject_id.to_string()),
        state,
        trigger: TaskRunTrigger::Schedule,
        attempts: 1,
        max_attempts: 3,
        lease_until: lease_until.map(String::from),
        next_attempt_at: next_attempt_at.to_string(),
        scope: None,
        result_id: None,
        last_error: None,
        started_at: None,
        finished_at: None,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// The sweep query behind event-triggered retries (wiki regen): every
/// pending row past its `next_attempt_at` plus every running row whose
/// lease expired, across all subjects, earliest `next_attempt_at` first —
/// and nothing else.
async fn test_list_runnable_task_runs(storage: &dyn TaskRunStore) {
    // Unique task id per invocation so reruns against a shared Postgres
    // database can't see a previous run's rows.
    let task_id = format!("sweep_test::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let past = (now - chrono::Duration::minutes(5)).to_rfc3339();
    let earlier_past = (now - chrono::Duration::minutes(10)).to_rfc3339();
    let future = (now + chrono::Duration::minutes(5)).to_rfc3339();

    let due = task_run_row(&task_id, "subject-due", TaskRunState::Pending, &past, None);
    let crashed = task_run_row(
        &task_id,
        "subject-crashed",
        TaskRunState::Running,
        &earlier_past,
        Some(&past), // lease expired — crash-recovery candidate
    );
    let backed_off = task_run_row(
        &task_id,
        "subject-backoff",
        TaskRunState::Pending,
        &future,
        None,
    );
    let in_flight = task_run_row(
        &task_id,
        "subject-live",
        TaskRunState::Running,
        &past,
        Some(&future), // live lease — owned by another worker
    );
    let settled = task_run_row(
        &task_id,
        "subject-done",
        TaskRunState::Succeeded,
        &past,
        None,
    );
    for row in [&due, &crashed, &backed_off, &in_flight, &settled] {
        storage.insert_task_run(row).await.unwrap();
    }

    let runnable = storage
        .list_runnable_task_runs(&task_id, &now.to_rfc3339())
        .await
        .unwrap();
    let ids: Vec<&str> = runnable.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![crashed.id.as_str(), due.id.as_str()],
        "exactly the due-pending and expired-lease rows, earliest next_attempt_at first"
    );

    // Other task ids never bleed into the sweep.
    let other = storage
        .list_runnable_task_runs("some_other_task", &now.to_rfc3339())
        .await
        .unwrap();
    assert!(other.iter().all(|r| r.task_id != task_id));
}

/// Deferral (environmental failures — provider limits/outages): the row
/// returns to `pending` at the caller's horizon with the claim's attempt
/// charge REFUNDED and the lease released. The retry-budget sibling of the
/// reclaim rule pinned in `pg_expired_lease_reclaimed_through_claim_path` /
/// `dispatch_reclaims_running_row_with_expired_lease`: neither a crash nor
/// an environment-caused failure consumes `max_attempts`. Same lease fence
/// as every other settle.
async fn test_defer_task_run_refunds_attempt_and_releases_lease(storage: &dyn TaskRunStore) {
    let task_id = format!("defer::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();
    let lease = (now + chrono::Duration::minutes(2)).to_rfc3339();
    let mut row = task_run_row(
        &task_id,
        "subject",
        TaskRunState::Running,
        &now_str,
        Some(&lease),
    );
    row.started_at = Some(now_str.clone());
    storage.insert_task_run(&row).await.unwrap();

    let horizon = (now + chrono::Duration::hours(1)).to_rfc3339();
    let error = "Embedding error: API error (402): out of credits";

    // A stale lease is fenced out and the row is untouched.
    assert!(
        !storage
            .defer_task_run(&row.id, "someone-elses-lease", error, &now_str, &horizon)
            .await
            .unwrap(),
        "a mismatched lease must not settle the row"
    );
    let untouched = storage.get_task_run(&row.id).await.unwrap().unwrap();
    assert_eq!(untouched.state, TaskRunState::Running);
    assert_eq!(untouched.attempts, 1);

    // The lease holder defers: pending at the horizon, attempt refunded,
    // lease and started_at cleared, failure recorded.
    assert!(storage
        .defer_task_run(&row.id, &lease, error, &now_str, &horizon)
        .await
        .unwrap());
    let deferred = storage.get_task_run(&row.id).await.unwrap().unwrap();
    assert_eq!(
        deferred.state,
        TaskRunState::Pending,
        "re-armed, not failed"
    );
    assert_eq!(
        deferred.attempts, 0,
        "deferral must refund the claim's attempt — environmental failures never consume retry budget"
    );
    assert_eq!(deferred.next_attempt_at, horizon);
    assert!(deferred.lease_until.is_none(), "lease released");
    assert!(deferred.started_at.is_none());
    assert_eq!(deferred.last_error.as_deref(), Some(error));
}

/// `rearm_task_runs` resets `next_attempt_at` on exactly the given ids, and
/// only while they are still `pending` — a row claimed (or settled) since
/// the caller's scan keeps its own horizon. `list_waiting_task_runs` is the
/// scan feeding it: pending rows with future horizons, nothing else.
async fn test_rearm_task_runs_gated_on_pending(storage: &dyn TaskRunStore) {
    let task_id = format!("rearm::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();
    let future = (now + chrono::Duration::hours(1)).to_rfc3339();
    let live_lease = (now + chrono::Duration::minutes(2)).to_rfc3339();

    let waiting = task_run_row(&task_id, "deferred", TaskRunState::Pending, &future, None);
    let running = task_run_row(
        &task_id,
        "claimed",
        TaskRunState::Running,
        &future,
        Some(&live_lease),
    );
    let due = task_run_row(
        &task_id,
        "already-due",
        TaskRunState::Pending,
        &now_str,
        None,
    );
    for row in [&waiting, &running, &due] {
        storage.insert_task_run(row).await.unwrap();
    }

    // The scan sees the future-pending row, not the running or already-due ones.
    let scanned: Vec<String> = storage
        .list_waiting_task_runs(&now_str)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert!(scanned.contains(&waiting.id), "waiting row is scanned");
    assert!(
        !scanned.contains(&running.id),
        "claimed rows are not waiting"
    );
    assert!(
        !scanned.contains(&due.id),
        "already-due rows are not waiting"
    );

    // Re-arm both the waiting and the (stale-scanned) running row: only the
    // pending one moves.
    let rearmed = storage
        .rearm_task_runs(&[waiting.id.clone(), running.id.clone()], &now_str)
        .await
        .unwrap();
    assert_eq!(rearmed, 1, "only the pending row is ours to rewrite");
    let waiting_after = storage.get_task_run(&waiting.id).await.unwrap().unwrap();
    assert_eq!(waiting_after.next_attempt_at, now_str);
    let running_after = storage.get_task_run(&running.id).await.unwrap().unwrap();
    assert_eq!(
        running_after.next_attempt_at, future,
        "a claimed row keeps its own horizon"
    );
}

/// `rearm_pipeline_jobs` resets `not_before` to now on pending jobs stamped
/// with exactly the given reason — the environment-changed escape hatch for
/// provider-backed-off pipeline work. Other reasons keep their horizons.
async fn test_rearm_pipeline_jobs_scoped_to_reason(atoms: &dyn AtomStore, chunks: &dyn ChunkStore) {
    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();
    let future = (now + chrono::Duration::minutes(10)).to_rfc3339();

    let mut atom_ids = Vec::new();
    for _ in 0..2 {
        let id = uuid::Uuid::new_v4().to_string();
        let request = CreateAtomRequest {
            content: "pipeline re-arm fixture".to_string(),
            ..Default::default()
        };
        atoms.insert_atom(&id, &request, &now_str).await.unwrap();
        atom_ids.push(id);
    }
    let job = |atom_id: &str, reason: &str| AtomPipelineJobRequest {
        atom_id: atom_id.to_string(),
        embed_requested: true,
        tag_requested: false,
        not_before: Some(future.clone()),
        reason: reason.to_string(),
        replace_existing: true,
    };
    chunks
        .enqueue_pipeline_jobs(&[
            job(&atom_ids[0], "provider-backoff"),
            job(&atom_ids[1], "save"),
        ])
        .await
        .unwrap();
    assert_eq!(chunks.count_due_pipeline_jobs(&now_str).await.unwrap(), 0);

    let rearmed = chunks
        .rearm_pipeline_jobs("provider-backoff", &now_str)
        .await
        .unwrap();
    assert_eq!(rearmed, 1, "exactly the provider-backoff row re-arms");
    assert_eq!(
        chunks.count_due_pipeline_jobs(&now_str).await.unwrap(),
        1,
        "the re-armed row is immediately due; the 'save' row keeps waiting"
    );
    // Nothing left to re-arm — the call is idempotent on already-due rows.
    assert_eq!(
        chunks
            .rearm_pipeline_jobs("provider-backoff", &now_str)
            .await
            .unwrap(),
        0
    );
}

// ==================== task_runs retention GC ====================
//
// `gc_task_runs` is one bounded delete batch; the policy lives entirely in
// its SQL, so each rule is pinned here against both backends. Each test
// uses a unique task id so reruns against the shared Postgres database
// can't see a previous run's rows.

/// Build a row with caller-controlled state and `created_at` — the GC
/// ranks (per-subject recency) and ages entirely on `created_at`.
fn gc_run_row(
    task_id: &str,
    subject_id: Option<&str>,
    state: TaskRunState,
    created_at: &str,
) -> TaskRun {
    TaskRun {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        subject_id: subject_id.map(String::from),
        state,
        trigger: TaskRunTrigger::Schedule,
        attempts: 1,
        max_attempts: 3,
        lease_until: None,
        next_attempt_at: created_at.to_string(),
        scope: None,
        result_id: None,
        last_error: None,
        started_at: None,
        finished_at: state.is_terminal().then(|| created_at.to_string()),
        created_at: created_at.to_string(),
        updated_at: created_at.to_string(),
    }
}

/// Surviving row ids for a task, any subject, newest-first.
async fn surviving_gc_ids(storage: &dyn TaskRunStore, task_id: &str) -> Vec<String> {
    storage
        .list_recent_task_runs(task_id, None, 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}

/// Rule: non-terminal rows are live execution state — never deleted, no
/// matter how old. `keep = 0` with both cutoffs at `now` makes every
/// terminal row eligible, so the state predicate is the only protection.
async fn test_gc_task_runs_never_deletes_non_terminal(storage: &dyn TaskRunStore) {
    let task_id = format!("gc_live::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let ancient = (now - chrono::Duration::days(400)).to_rfc3339();

    let pending = gc_run_row(
        &task_id,
        Some("backing-off"),
        TaskRunState::Pending,
        &ancient,
    );
    // Crashed run: still `running` with a long-expired lease. Reclaimable,
    // not collectible.
    let mut crashed = gc_run_row(&task_id, Some("crashed"), TaskRunState::Running, &ancient);
    crashed.lease_until = Some(ancient.clone());
    let settled = gc_run_row(&task_id, Some("settled"), TaskRunState::Succeeded, &ancient);
    for row in [&pending, &crashed, &settled] {
        storage.insert_task_run(row).await.unwrap();
    }

    let now_str = now.to_rfc3339();
    let deleted = storage
        .gc_task_runs(0, &now_str, &now_str, 100)
        .await
        .unwrap();

    assert_eq!(deleted, 1, "only the terminal row is collectible");
    assert!(storage.get_task_run(&pending.id).await.unwrap().is_some());
    assert!(storage.get_task_run(&crashed.id).await.unwrap().is_some());
    assert!(storage.get_task_run(&settled.id).await.unwrap().is_none());
}

/// Rule: per `(task_id, subject_id)`, the most recent K terminal rows
/// survive — each subject keeps its own window, and the NULL subject
/// (singleton system tasks) is its own group.
async fn test_gc_task_runs_keeps_most_recent_k_per_subject(storage: &dyn TaskRunStore) {
    let task_id = format!("gc_window::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let at = |mins: i64| (now - chrono::Duration::minutes(mins)).to_rfc3339();

    // Subject "feed-a": 4 recent successes; NULL subject: 3.
    let feed_rows: Vec<TaskRun> = (1..=4)
        .map(|m| gc_run_row(&task_id, Some("feed-a"), TaskRunState::Succeeded, &at(m)))
        .collect();
    let singleton_rows: Vec<TaskRun> = (1..=3)
        .map(|m| gc_run_row(&task_id, None, TaskRunState::Succeeded, &at(m)))
        .collect();
    for row in feed_rows.iter().chain(&singleton_rows) {
        storage.insert_task_run(row).await.unwrap();
    }

    let age_cutoff = (now - chrono::Duration::days(30)).to_rfc3339();
    let failed_cutoff = (now - chrono::Duration::days(90)).to_rfc3339();
    let deleted = storage
        .gc_task_runs(2, &age_cutoff, &failed_cutoff, 100)
        .await
        .unwrap();

    assert_eq!(deleted, 3, "2 trimmed from feed-a, 1 from the NULL group");
    let survivors = surviving_gc_ids(storage, &task_id).await;
    let expected: Vec<&str> = feed_rows[..2]
        .iter()
        .chain(&singleton_rows[..2])
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(survivors.len(), 4);
    assert!(
        expected.iter().all(|id| survivors.iter().any(|s| s == id)),
        "each group keeps exactly its 2 newest rows"
    );
}

/// Rule: the hard age cap deletes terminal rows older than `retain_days`
/// even when they sit comfortably inside the keep-K window.
async fn test_gc_task_runs_age_cap_applies_inside_keep_window(storage: &dyn TaskRunStore) {
    let task_id = format!("gc_age::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();

    let recent = gc_run_row(
        &task_id,
        Some("subject"),
        TaskRunState::Succeeded,
        &(now - chrono::Duration::minutes(1)).to_rfc3339(),
    );
    let expired = gc_run_row(
        &task_id,
        Some("subject"),
        TaskRunState::Succeeded,
        &(now - chrono::Duration::days(31)).to_rfc3339(),
    );
    for row in [&recent, &expired] {
        storage.insert_task_run(row).await.unwrap();
    }

    // keep = 50: both rows are within the recency window — only the age
    // cap can make the old one eligible.
    let age_cutoff = (now - chrono::Duration::days(30)).to_rfc3339();
    let failed_cutoff = (now - chrono::Duration::days(90)).to_rfc3339();
    let deleted = storage
        .gc_task_runs(50, &age_cutoff, &failed_cutoff, 100)
        .await
        .unwrap();

    assert_eq!(deleted, 1);
    assert!(storage.get_task_run(&recent.id).await.unwrap().is_some());
    assert!(storage.get_task_run(&expired.id).await.unwrap().is_none());
}

/// Rule: the most recent terminal failure per group outlives both the
/// keep-K window and the age cap, until it ages past `retain_failed_days`.
/// Older failures and successes in the same group get no such grace.
async fn test_gc_task_runs_retains_most_recent_failure(storage: &dyn TaskRunStore) {
    let task_id = format!("gc_failure::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let days_ago = |d: i64| (now - chrono::Duration::days(d)).to_rfc3339();

    // feed-x: the most recent failure (40d) is past retain_days but inside
    // retain_failed_days — protected. Its older failure (50d) and an old
    // success (45d) are not.
    let protected = gc_run_row(
        &task_id,
        Some("feed-x"),
        TaskRunState::Abandoned,
        &days_ago(40),
    );
    let older_failure = gc_run_row(
        &task_id,
        Some("feed-x"),
        TaskRunState::Failed,
        &days_ago(50),
    );
    let old_success = gc_run_row(
        &task_id,
        Some("feed-x"),
        TaskRunState::Succeeded,
        &days_ago(45),
    );
    // feed-y: its most recent failure is older than retain_failed_days —
    // the exception lapses and it goes too.
    let lapsed_failure = gc_run_row(
        &task_id,
        Some("feed-y"),
        TaskRunState::Failed,
        &days_ago(120),
    );
    for row in [&protected, &older_failure, &old_success, &lapsed_failure] {
        storage.insert_task_run(row).await.unwrap();
    }

    // keep = 0 so every row is rank-eligible: the failure exception is the
    // only thing that can keep a row alive here.
    let age_cutoff = days_ago(30);
    let failed_cutoff = days_ago(90);
    let deleted = storage
        .gc_task_runs(0, &age_cutoff, &failed_cutoff, 100)
        .await
        .unwrap();

    assert_eq!(deleted, 3);
    let survivors = surviving_gc_ids(storage, &task_id).await;
    assert_eq!(survivors, vec![protected.id.clone()]);
}

/// Each call deletes at most `batch_size` rows (oldest first) and reports
/// the count, so the caller's loop can drain a backlog without one giant
/// delete holding the write lock.
async fn test_gc_task_runs_batches_are_bounded(storage: &dyn TaskRunStore) {
    let task_id = format!("gc_batch::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    for i in 0..7 {
        let created =
            (now - chrono::Duration::days(40) - chrono::Duration::minutes(i)).to_rfc3339();
        let row = gc_run_row(&task_id, Some("subject"), TaskRunState::Succeeded, &created);
        storage.insert_task_run(&row).await.unwrap();
    }

    let age_cutoff = (now - chrono::Duration::days(30)).to_rfc3339();
    let failed_cutoff = (now - chrono::Duration::days(90)).to_rfc3339();
    let mut per_call = Vec::new();
    loop {
        let deleted = storage
            .gc_task_runs(0, &age_cutoff, &failed_cutoff, 3)
            .await
            .unwrap();
        per_call.push(deleted);
        if deleted < 3 {
            break;
        }
    }

    assert_eq!(per_call, vec![3, 3, 1], "bounded batches until drained");
    assert!(surviving_gc_ids(storage, &task_id).await.is_empty());
}

/// Force-settling moot rows (subject definition deleted, e.g. a feed):
/// every non-terminal row for the `(task_id, subject_id)` settles
/// `succeeded` — backed-off pending and live-lease running alike, since
/// neither is claimable through the normal path and no sweep will ever
/// revisit them once the definition is gone. Terminal history and other
/// subjects are untouched.
async fn test_settle_task_runs_moot(storage: &dyn TaskRunStore) {
    let task_id = format!("moot_test::{}", uuid::Uuid::new_v4());
    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();
    let future = (now + chrono::Duration::minutes(30)).to_rfc3339();
    let past = (now - chrono::Duration::minutes(5)).to_rfc3339();

    // subject-a: a backed-off pending row (unclaimable until `future`)
    // plus terminal history that must survive the settle untouched.
    let backed_off = task_run_row(&task_id, "subject-a", TaskRunState::Pending, &future, None);
    let history = task_run_row(&task_id, "subject-a", TaskRunState::Abandoned, &past, None);
    // subject-b: a running row with a live lease (a poll in flight).
    let in_flight = task_run_row(
        &task_id,
        "subject-b",
        TaskRunState::Running,
        &past,
        Some(&future),
    );
    for row in [&backed_off, &history, &in_flight] {
        storage.insert_task_run(row).await.unwrap();
    }

    let settled = storage
        .settle_task_runs_moot(&task_id, "subject-a", &now_str)
        .await
        .unwrap();
    assert_eq!(settled, 1, "only subject-a's non-terminal row settles");

    let row = storage.get_task_run(&backed_off.id).await.unwrap().unwrap();
    assert_eq!(row.state, TaskRunState::Succeeded);
    assert!(row.lease_until.is_none());
    assert!(row.finished_at.is_some());
    assert!(
        storage
            .find_active_task_run(&task_id, Some("subject-a"))
            .await
            .unwrap()
            .is_none(),
        "no non-terminal rows remain for the settled subject"
    );
    let untouched = storage.get_task_run(&history.id).await.unwrap().unwrap();
    assert_eq!(
        untouched.state,
        TaskRunState::Abandoned,
        "terminal history keeps its state — settle is not a rewrite"
    );

    // The sibling subject's in-flight row is another definition's business.
    let sibling = storage.get_task_run(&in_flight.id).await.unwrap().unwrap();
    assert_eq!(sibling.state, TaskRunState::Running);

    // A live lease is no shield when its own subject is deleted…
    let settled = storage
        .settle_task_runs_moot(&task_id, "subject-b", &now_str)
        .await
        .unwrap();
    assert_eq!(settled, 1);
    let row = storage.get_task_run(&in_flight.id).await.unwrap().unwrap();
    assert_eq!(row.state, TaskRunState::Succeeded);
    // …and the displaced worker's terminal write loses cleanly on the
    // state predicate instead of resurrecting the row.
    assert!(!storage
        .fail_task_run_retry(&in_flight.id, &future, "boom", &now_str, &future)
        .await
        .unwrap());

    // Idempotent: nothing left to settle.
    assert_eq!(
        storage
            .settle_task_runs_moot(&task_id, "subject-a", &now_str)
            .await
            .unwrap(),
        0
    );
}

// ==================== ChatStore Tests ====================

async fn test_create_conversation(storage: &dyn ChatStore) {
    let conv = storage.create_conversation(&[], None).await.unwrap();
    assert!(!conv.conversation.id.is_empty());

    let fetched = storage
        .get_conversation(&conv.conversation.id)
        .await
        .unwrap();
    assert!(fetched.is_some());
}

async fn test_save_and_get_messages(storage: &dyn ChatStore) {
    let conv = storage
        .create_conversation(&[], Some("Test Chat"))
        .await
        .unwrap();

    let msg = storage
        .save_message(&conv.conversation.id, "user", "Hello!")
        .await
        .unwrap();
    assert_eq!(msg.role, "user");
    assert_eq!(msg.content, "Hello!");

    let full = storage
        .get_conversation(&conv.conversation.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(full.messages.len(), 1);
}

async fn test_delete_conversation(storage: &dyn ChatStore) {
    let conv = storage.create_conversation(&[], None).await.unwrap();
    storage
        .delete_conversation(&conv.conversation.id)
        .await
        .unwrap();

    let fetched = storage
        .get_conversation(&conv.conversation.id)
        .await
        .unwrap();
    assert!(fetched.is_none());
}

// ==================== WikiStore Tests ====================

async fn test_save_and_get_wiki(tag_store: &dyn TagStore, wiki_store: &dyn WikiStore) {
    // Wiki articles reference tags via FK, so create a tag first
    let tag = tag_store.create_tag("Wiki Test Tag", None).await.unwrap();
    let article = wiki_store
        .save_wiki(&tag.id, "# Wiki Article\n\nContent here.", &[], 5)
        .await
        .unwrap();
    assert_eq!(article.article.tag_id, tag.id);

    let fetched = wiki_store.get_wiki(&tag.id).await.unwrap();
    assert!(fetched.is_some());
    assert_eq!(
        fetched.unwrap().article.content,
        "# Wiki Article\n\nContent here."
    );
}

async fn test_wiki_dangling_link_persists(tag_store: &dyn TagStore, wiki_store: &dyn WikiStore) {
    let source = tag_store.create_tag("Wiki Link Source", None).await.unwrap();
    let target = tag_store.create_tag("Wiki Link Target", None).await.unwrap();
    let saved = wiki_store
        .save_wiki(&source.id, "See [[Wiki Link Target]] and [[Ghost]].", &[], 1)
        .await
        .unwrap();

    let run = uuid::Uuid::new_v4();
    let links = vec![
        WikiLink {
            id: format!("wl-resolved-{run}"),
            source_article_id: saved.article.id.clone(),
            target_tag_name: "Wiki Link Target".to_string(),
            target_tag_id: Some(target.id.clone()),
            has_article: false,
        },
        // A dangling link — its tag no longer exists (deleted after the
        // article was written). Both backends must persist it; Postgres
        // used to silently drop it (NOT NULL column, fixed in migration 023).
        WikiLink {
            id: format!("wl-ghost-{run}"),
            source_article_id: saved.article.id.clone(),
            target_tag_name: "Ghost".to_string(),
            target_tag_id: None,
            has_article: false,
        },
    ];
    wiki_store
        .save_wiki_with_links(&saved.article, &[], &links)
        .await
        .unwrap();

    let mut fetched = wiki_store.get_wiki_links(&source.id).await.unwrap();
    fetched.sort_by(|a, b| a.target_tag_name.cmp(&b.target_tag_name));
    assert_eq!(fetched.len(), 2, "dangling link persisted alongside resolved");
    assert_eq!(fetched[0].target_tag_name, "Ghost");
    assert_eq!(fetched[0].target_tag_id, None);
    assert_eq!(fetched[1].target_tag_id.as_deref(), Some(target.id.as_str()));
}

async fn test_delete_wiki(tag_store: &dyn TagStore, wiki_store: &dyn WikiStore) {
    let tag = tag_store.create_tag("Wiki Delete Tag", None).await.unwrap();
    wiki_store.save_wiki(&tag.id, "temp", &[], 1).await.unwrap();
    wiki_store.delete_wiki(&tag.id).await.unwrap();

    let fetched = wiki_store.get_wiki(&tag.id).await.unwrap();
    assert!(fetched.is_none());
}

async fn test_wiki_update_chunks_pending_atom_errors(
    atom_store: &dyn AtomStore,
    tag_store: &dyn TagStore,
    wiki_store: &dyn WikiStore,
) {
    let tag = tag_store
        .create_tag("Wiki Pending Tag", None)
        .await
        .unwrap();
    let request = CreateAtomRequest {
        content: "This atom has not been chunked yet.".to_string(),
        source_url: None,
        published_at: None,
        tag_ids: vec![tag.id.clone()],
        ..Default::default()
    };
    let atom_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    atom_store
        .insert_atom(&atom_id, &request, &now)
        .await
        .unwrap();

    let result = wiki_store
        .get_wiki_update_chunks(&tag.id, "1970-01-01T00:00:00Z", 1024)
        .await;

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("not ready for wiki update"));
}

// ==================== ChunkStore Tests ====================

// Embedding status tests need both AtomStore and ChunkStore together,
// which is tested through AtomicCore integration tests.

// ==================== SQLite Test Runners ====================

#[tokio::test]
async fn sqlite_create_and_get_atom() {
    let (s, _dir) = sqlite_storage().await;
    test_create_and_get_atom(&s).await;
}

#[tokio::test]
async fn sqlite_get_atom_not_found() {
    let (s, _dir) = sqlite_storage().await;
    test_get_atom_not_found(&s).await;
}

#[tokio::test]
async fn sqlite_delete_atom() {
    let (s, _dir) = sqlite_storage().await;
    test_delete_atom(&s).await;
}

#[tokio::test]
async fn sqlite_update_atom() {
    let (s, _dir) = sqlite_storage().await;
    test_update_atom(&s).await;
}

#[tokio::test]
async fn sqlite_update_atom_if_unchanged_rejects_stale_write() {
    let (s, _dir) = sqlite_storage().await;
    test_update_atom_if_unchanged_rejects_stale_write(&s).await;
}

#[tokio::test]
async fn sqlite_atom_links_materialized() {
    let (s, _dir) = sqlite_storage().await;
    test_atom_links_materialized(&s).await;
}

#[tokio::test]
async fn sqlite_atom_links_replaced_on_update() {
    let (s, _dir) = sqlite_storage().await;
    test_atom_links_replaced_on_update(&s).await;
}

#[tokio::test]
async fn sqlite_atom_links_mark_target_missing_on_delete() {
    let (s, _dir) = sqlite_storage().await;
    test_atom_links_mark_target_missing_on_delete(&s).await;
}

#[tokio::test]
async fn sqlite_atom_link_suggestions_recent_and_title_ranked() {
    let (s, _dir) = sqlite_storage().await;
    test_atom_link_suggestions_recent_and_title_ranked(&s).await;
}

#[tokio::test]
async fn sqlite_get_all_atoms() {
    let (s, _dir) = sqlite_storage().await;
    test_get_all_atoms(&s).await;
}

#[tokio::test]
async fn sqlite_list_atoms_pagination() {
    let (s, _dir) = sqlite_storage().await;
    test_list_atoms_pagination(&s).await;
}

#[tokio::test]
async fn sqlite_create_and_get_tags() {
    let (s, _dir) = sqlite_storage().await;
    test_create_and_get_tags(&s).await;
}

#[tokio::test]
async fn sqlite_update_tag() {
    let (s, _dir) = sqlite_storage().await;
    test_update_tag(&s).await;
}

#[tokio::test]
async fn sqlite_delete_tag() {
    let (s, _dir) = sqlite_storage().await;
    test_delete_tag(&s).await;
}

#[tokio::test]
async fn sqlite_get_tag() {
    let (s, _dir) = sqlite_storage().await;
    test_get_tag(&s).await;
}

#[tokio::test]
async fn sqlite_list_runnable_task_runs() {
    let (s, _dir) = sqlite_storage().await;
    test_list_runnable_task_runs(&s).await;
}

#[tokio::test]
async fn sqlite_gc_task_runs_never_deletes_non_terminal() {
    let (s, _dir) = sqlite_storage().await;
    test_gc_task_runs_never_deletes_non_terminal(&s).await;
}

#[tokio::test]
async fn sqlite_gc_task_runs_keeps_most_recent_k_per_subject() {
    let (s, _dir) = sqlite_storage().await;
    test_gc_task_runs_keeps_most_recent_k_per_subject(&s).await;
}

#[tokio::test]
async fn sqlite_gc_task_runs_age_cap_applies_inside_keep_window() {
    let (s, _dir) = sqlite_storage().await;
    test_gc_task_runs_age_cap_applies_inside_keep_window(&s).await;
}

#[tokio::test]
async fn sqlite_gc_task_runs_retains_most_recent_failure() {
    let (s, _dir) = sqlite_storage().await;
    test_gc_task_runs_retains_most_recent_failure(&s).await;
}

#[tokio::test]
async fn sqlite_gc_task_runs_batches_are_bounded() {
    let (s, _dir) = sqlite_storage().await;
    test_gc_task_runs_batches_are_bounded(&s).await;
}

#[tokio::test]
async fn sqlite_settle_task_runs_moot() {
    let (s, _dir) = sqlite_storage().await;
    test_settle_task_runs_moot(&s).await;
}

#[tokio::test]
async fn sqlite_defer_task_run_refunds_attempt_and_releases_lease() {
    let (s, _dir) = sqlite_storage().await;
    test_defer_task_run_refunds_attempt_and_releases_lease(&s).await;
}

#[tokio::test]
async fn sqlite_rearm_task_runs_gated_on_pending() {
    let (s, _dir) = sqlite_storage().await;
    test_rearm_task_runs_gated_on_pending(&s).await;
}

#[tokio::test]
async fn sqlite_rearm_pipeline_jobs_scoped_to_reason() {
    let (s, _dir) = sqlite_storage().await;
    test_rearm_pipeline_jobs_scoped_to_reason(&s, &s).await;
}

#[tokio::test]
async fn sqlite_create_conversation() {
    let (s, _dir) = sqlite_storage().await;
    test_create_conversation(&s).await;
}

#[tokio::test]
async fn sqlite_save_and_get_messages() {
    let (s, _dir) = sqlite_storage().await;
    test_save_and_get_messages(&s).await;
}

#[tokio::test]
async fn sqlite_delete_conversation() {
    let (s, _dir) = sqlite_storage().await;
    test_delete_conversation(&s).await;
}

#[tokio::test]
async fn sqlite_save_and_get_wiki() {
    let (s, _dir) = sqlite_storage().await;
    test_save_and_get_wiki(&s, &s).await;
}

#[tokio::test]
async fn sqlite_wiki_dangling_link_persists() {
    let (s, _dir) = sqlite_storage().await;
    test_wiki_dangling_link_persists(&s, &s).await;
}

#[tokio::test]
async fn sqlite_delete_wiki() {
    let (s, _dir) = sqlite_storage().await;
    test_delete_wiki(&s, &s).await;
}

#[tokio::test]
async fn sqlite_wiki_update_chunks_pending_atom_errors() {
    let (s, _dir) = sqlite_storage().await;
    test_wiki_update_chunks_pending_atom_errors(&s, &s, &s).await;
}

// ==================== SettingsStore Tests ====================

/// On SQLite the global accessors must resolve to the same physical table
/// as the scoped ones: each data DB file owns its only settings table, and
/// the registry plays the global role one layer up (in `AtomicCore`), so
/// the trait's delegate-to-scoped defaults are exactly right here.
#[tokio::test]
async fn sqlite_global_settings_delegate_to_scoped_table() {
    let (s, _dir) = sqlite_storage().await;

    s.set_global_setting("settings_probe_global", "from-global")
        .await
        .unwrap();
    assert_eq!(
        s.get_setting("settings_probe_global").await.unwrap(),
        Some("from-global".to_string()),
        "a global write must be visible through the scoped read"
    );

    s.set_setting("settings_probe_scoped", "from-scoped")
        .await
        .unwrap();
    assert_eq!(
        s.get_global_settings()
            .await
            .unwrap()
            .get("settings_probe_scoped")
            .map(String::as_str),
        Some("from-scoped"),
        "a scoped write must be visible through the global read"
    );

    s.delete_global_setting("settings_probe_global")
        .await
        .unwrap();
    assert!(
        s.get_setting("settings_probe_global")
            .await
            .unwrap()
            .is_none(),
        "a global delete must remove the scoped row"
    );
}

// ==================== Postgres Test Runners ====================

#[cfg(feature = "postgres")]
mod postgres_tests {
    use super::*;

    /// Helper macro: skip test if ATOMIC_TEST_DATABASE_URL is not set
    macro_rules! pg_test {
        ($name:ident, $body:expr) => {
            #[tokio::test]
            async fn $name() {
                let Some(ref s) = postgres_storage().await else {
                    eprintln!(
                        "Skipping {} (ATOMIC_TEST_DATABASE_URL not set)",
                        stringify!($name)
                    );
                    return;
                };
                $body(s).await;
            }
        };
    }

    pg_test!(pg_create_and_get_atom, test_create_and_get_atom);
    pg_test!(pg_get_atom_not_found, test_get_atom_not_found);
    pg_test!(pg_delete_atom, test_delete_atom);
    pg_test!(pg_update_atom, test_update_atom);
    pg_test!(
        pg_update_atom_if_unchanged_rejects_stale_write,
        test_update_atom_if_unchanged_rejects_stale_write
    );
    pg_test!(pg_atom_links_materialized, test_atom_links_materialized);
    pg_test!(
        pg_atom_links_replaced_on_update,
        test_atom_links_replaced_on_update
    );
    pg_test!(
        pg_atom_links_mark_target_missing_on_delete,
        test_atom_links_mark_target_missing_on_delete
    );
    pg_test!(
        pg_atom_link_suggestions_recent_and_title_ranked,
        test_atom_link_suggestions_recent_and_title_ranked
    );
    pg_test!(pg_get_all_atoms, test_get_all_atoms);
    pg_test!(pg_list_atoms_pagination, test_list_atoms_pagination);
    pg_test!(pg_create_and_get_tags, test_create_and_get_tags);
    pg_test!(pg_update_tag, test_update_tag);
    pg_test!(pg_delete_tag, test_delete_tag);
    pg_test!(pg_get_tag, test_get_tag);
    pg_test!(pg_list_runnable_task_runs, test_list_runnable_task_runs);
    pg_test!(
        pg_gc_task_runs_never_deletes_non_terminal,
        test_gc_task_runs_never_deletes_non_terminal
    );
    pg_test!(
        pg_gc_task_runs_keeps_most_recent_k_per_subject,
        test_gc_task_runs_keeps_most_recent_k_per_subject
    );
    pg_test!(
        pg_gc_task_runs_age_cap_applies_inside_keep_window,
        test_gc_task_runs_age_cap_applies_inside_keep_window
    );
    pg_test!(
        pg_gc_task_runs_retains_most_recent_failure,
        test_gc_task_runs_retains_most_recent_failure
    );
    pg_test!(
        pg_gc_task_runs_batches_are_bounded,
        test_gc_task_runs_batches_are_bounded
    );
    pg_test!(pg_settle_task_runs_moot, test_settle_task_runs_moot);
    pg_test!(
        pg_defer_task_run_refunds_attempt_and_releases_lease,
        test_defer_task_run_refunds_attempt_and_releases_lease
    );
    pg_test!(
        pg_rearm_task_runs_gated_on_pending,
        test_rearm_task_runs_gated_on_pending
    );

    #[tokio::test]
    async fn pg_rearm_pipeline_jobs_scoped_to_reason() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        test_rearm_pipeline_jobs_scoped_to_reason(s, s).await;
    }

    pg_test!(pg_create_conversation, test_create_conversation);
    pg_test!(pg_save_and_get_messages, test_save_and_get_messages);
    pg_test!(pg_delete_conversation, test_delete_conversation);

    /// Re-initializing an already-migrated database must be a no-op. A
    /// version-read failure (or type-mismatched decode) that silently
    /// defaults to 0 re-runs every migration from 1, which appends duplicate
    /// schema_version rows on every open — exactly what this guards against.
    #[tokio::test]
    async fn pg_initialize_reopen_runs_no_migrations() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        let url = std::env::var("ATOMIC_TEST_DATABASE_URL").unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let count = |pool: sqlx::PgPool| async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schema_version")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let before = count(pool.clone()).await;
        s.initialize().await.unwrap();
        let after = count(pool).await;
        assert_eq!(
            before, after,
            "initialize() on a migrated database re-ran migrations \
             ({before} schema_version rows -> {after})"
        );
    }

    #[tokio::test]
    async fn pg_save_and_get_wiki() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        test_save_and_get_wiki(s, s).await;
    }

    #[tokio::test]
    async fn pg_wiki_dangling_link_persists() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        test_wiki_dangling_link_persists(s, s).await;
    }

    #[tokio::test]
    async fn pg_delete_wiki() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        test_delete_wiki(s, s).await;
    }

    #[tokio::test]
    async fn pg_wiki_update_chunks_pending_atom_errors() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        test_wiki_update_chunks_pending_atom_errors(s, s, s).await;
    }

    /// The SQL-side `AVG(embedding)` must reproduce the per-atom chunk
    /// averaging the canvas compute relies on: multi-chunk atoms get the
    /// element-wise mean, single-chunk atoms pass through unchanged, and
    /// atoms without embeddings are absent.
    #[tokio::test]
    async fn pg_embedding_pairs_average_chunks_per_atom() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };

        let now = chrono::Utc::now().to_rfc3339();
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = uuid::Uuid::new_v4().to_string();
            let request = CreateAtomRequest {
                content: format!("Atom {i}"),
                ..Default::default()
            };
            s.insert_atom(&id, &request, &now).await.unwrap();
            ids.push(id);
        }

        s.save_chunks_and_embeddings(
            &ids[0],
            &[
                ("chunk a".to_string(), vec![1.0, 0.0, 0.0]),
                ("chunk b".to_string(), vec![0.0, 1.0, 0.0]),
            ],
        )
        .await
        .unwrap();
        s.save_chunks_and_embeddings(&ids[1], &[("solo".to_string(), vec![0.0, 0.0, 1.0])])
            .await
            .unwrap();
        // ids[2] has no chunks and must not appear.

        let pairs: std::collections::HashMap<String, Vec<f32>> =
            s.get_all_embedding_pairs().await.unwrap().into_iter().collect();

        assert_eq!(pairs.len(), 2);
        let averaged = &pairs[&ids[0]];
        assert_eq!(averaged.len(), 3);
        for (got, want) in averaged.iter().zip([0.5, 0.5, 0.0]) {
            assert!((got - want).abs() < 1e-6, "averaged: {averaged:?}");
        }
        assert_eq!(pairs[&ids[1]], vec![0.0, 0.0, 1.0]);
        assert!(!pairs.contains_key(&ids[2]));
    }

    /// The centroid-ranked wiki chunk selectors must run against the real
    /// Postgres schema — embeddings live ON `atom_chunks`, and a stale
    /// `atom_chunk_embeddings` join here shipped to production unexercised
    /// (cloud wiki generation failed on FIRST use, 2026-07-14). This pins
    /// both ranked paths: full-source selection and the incremental-update
    /// selection, ordered by similarity to the tag centroid.
    #[tokio::test]
    async fn pg_wiki_ranked_chunk_selection_uses_real_schema() {
        use atomic_core::storage::{TagStore, WikiStore};

        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };

        let tag = s.create_tag("Wiki Ranked Tag", None).await.unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut ids = Vec::new();
        for i in 0..2 {
            let id = uuid::Uuid::new_v4().to_string();
            let request = CreateAtomRequest {
                content: format!("Ranked atom {i}"),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            };
            s.insert_atom(&id, &request, &now).await.unwrap();
            ids.push(id);
        }
        // Chunk embeddings: ids[0] aligned with the centroid, ids[1]
        // orthogonal. The cloud schema pins vector(1536), so build
        // full-width basis vectors.
        let axis = |i: usize| {
            let mut v = vec![0.0f32; 1536];
            v[i] = 1.0;
            v
        };
        s.save_chunks_and_embeddings(&ids[0], &[("near".to_string(), axis(0))])
            .await
            .unwrap();
        s.save_chunks_and_embeddings(&ids[1], &[("far".to_string(), axis(1))])
            .await
            .unwrap();
        s.save_tag_centroid(&tag.id, &axis(0)).await.unwrap();

        // Ranked full-source path: both chunks, similarity-ordered.
        let (chunks, atom_count) = s
            .get_wiki_source_chunks(&tag.id, 10_000)
            .await
            .expect("ranked source selection must run on the Postgres schema");
        assert_eq!(atom_count, 2);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].content, "near", "centroid-nearest chunk first");
        assert!(chunks[0].similarity_score > chunks[1].similarity_score);

        // Ranked incremental path: only atoms created after the watermark.
        let epoch = "1970-01-01T00:00:00Z";
        let update = s
            .get_wiki_update_chunks(&tag.id, epoch, 10_000)
            .await
            .expect("ranked update selection must run on the Postgres schema")
            .expect("all atoms are newer than the epoch watermark");
        assert_eq!(update.1, 2);
        assert_eq!(update.0[0].content, "near");
    }

    /// Cross-`db_id` fencing: two storage handles sharing one pool but
    /// scoped to different logical databases must not see each other's
    /// settings rows — the exact leak that used to share `task.{id}.*`
    /// scheduler state between databases on Postgres.
    #[tokio::test]
    async fn pg_settings_scoped_by_db_id() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        let alpha = s.with_db_id("settings_fence_alpha");
        let beta = s.with_db_id("settings_fence_beta");

        alpha
            .set_setting("task.probe.last_run", "alpha-run")
            .await
            .unwrap();
        beta.set_setting("task.probe.last_run", "beta-run")
            .await
            .unwrap();

        assert_eq!(
            alpha.get_setting("task.probe.last_run").await.unwrap(),
            Some("alpha-run".to_string())
        );
        assert_eq!(
            beta.get_setting("task.probe.last_run").await.unwrap(),
            Some("beta-run".to_string()),
            "beta must keep its own value — alpha's write must not clobber it"
        );

        let alpha_all = alpha.get_all_settings().await.unwrap();
        assert_eq!(
            alpha_all.get("task.probe.last_run").map(String::as_str),
            Some("alpha-run")
        );
        assert_eq!(alpha_all.len(), 1, "alpha sees only its own row");

        // Deleting in one database must leave the sibling untouched.
        alpha.delete_setting("task.probe.last_run").await.unwrap();
        assert!(alpha
            .get_setting("task.probe.last_run")
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            beta.get_setting("task.probe.last_run").await.unwrap(),
            Some("beta-run".to_string()),
            "alpha's delete must not bleed into beta"
        );
    }

    /// The global tier is its own scope: a global row and a per-DB row
    /// under the same key name coexist without shadowing each other in
    /// either direction.
    #[tokio::test]
    async fn pg_global_settings_isolated_from_scoped_rows() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };

        s.set_global_setting("settings_probe", "global-value")
            .await
            .unwrap();
        s.set_setting("settings_probe", "scoped-value")
            .await
            .unwrap();

        assert_eq!(
            s.get_global_setting("settings_probe").await.unwrap(),
            Some("global-value".to_string())
        );
        assert_eq!(
            s.get_setting("settings_probe").await.unwrap(),
            Some("scoped-value".to_string())
        );

        let global_all = s.get_global_settings().await.unwrap();
        assert_eq!(
            global_all.get("settings_probe").map(String::as_str),
            Some("global-value"),
            "global map reads the global row, not the scoped one"
        );
        let scoped_all = s.get_all_settings().await.unwrap();
        assert_eq!(
            scoped_all.get("settings_probe").map(String::as_str),
            Some("scoped-value"),
            "scoped map reads the scoped row, not the global one"
        );

        // Deletes are tier-local too.
        s.delete_global_setting("settings_probe").await.unwrap();
        assert!(s
            .get_global_setting("settings_probe")
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            s.get_setting("settings_probe").await.unwrap(),
            Some("scoped-value".to_string()),
            "deleting the global row must not touch the scoped row"
        );
    }

    /// Migration 021 on a pre-021 shaped table: existing single-tier rows
    /// must land in the `'_global'` scope and the new composite primary key
    /// must hold. Rewinds the shared test schema to the old shape (legal
    /// because the suite runs with `--test-threads=1`), seeds legacy rows,
    /// and re-runs `initialize()` — which replays 021 *and* 022, so the
    /// per-DB-role task key is lifted back out of `'_global'` by the
    /// backfill (see `pg_settings_backfill_replicates_orphaned_per_db_keys`
    /// for the fan-out itself).
    #[tokio::test]
    async fn pg_settings_migration_lands_existing_rows_in_global() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };

        // Rewind: drop the 021 marker and restore the pre-021 table shape
        // (key PRIMARY KEY, no db_id). The settings table is already empty
        // thanks to the truncate in `postgres_storage()`.
        sqlx::raw_sql(
            "DELETE FROM schema_version WHERE version >= 21;
             ALTER TABLE settings DROP CONSTRAINT IF EXISTS settings_pkey;
             ALTER TABLE settings DROP COLUMN IF EXISTS db_id;
             ALTER TABLE settings ADD PRIMARY KEY (key);",
        )
        .execute(s.pool())
        .await
        .unwrap();

        // Pre-021 rows: registry-role config plus a per-DB-role task key
        // that used to collide across logical databases.
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES
                 ('provider', 'ollama'),
                 ('task.draft_pipeline.last_run', '2026-01-01T00:00:00Z')",
        )
        .execute(s.pool())
        .await
        .unwrap();

        s.initialize().await.unwrap();

        // The registry-role row landed in the global tier; the per-DB-role
        // task key passed through it but was lifted out again by 022's
        // backfill, so '_global' holds exactly the registry config.
        let global_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT db_id, key FROM settings WHERE db_id = '_global' ORDER BY key")
                .fetch_all(s.pool())
                .await
                .unwrap();
        assert_eq!(
            global_rows,
            vec![("_global".to_string(), "provider".to_string())]
        );

        // …visible through the global accessors and invisible to scoped
        // reads.
        assert_eq!(
            s.get_global_setting("provider").await.unwrap(),
            Some("ollama".to_string())
        );
        assert!(s.get_setting("provider").await.unwrap().is_none());

        // The new PK is (db_id, key): the same key coexists across scopes…
        s.set_setting("provider", "scoped-value").await.unwrap();
        // …but a duplicate (db_id, key) pair violates the constraint.
        let dup = sqlx::query(
            "INSERT INTO settings (db_id, key, value) VALUES ('_global', 'provider', 'dup')",
        )
        .execute(s.pool())
        .await;
        assert!(dup.is_err(), "duplicate (db_id, key) must violate the PK");
    }

    /// Migration 022 on the post-021 orphaned state: per-DB-role keys
    /// stranded in `'_global'` by 021's landing must be replicated into
    /// every logical database — preserving the pre-021 "one shared table
    /// applies everywhere" behavior, so no duplicate Daily Briefing
    /// re-seed and no reverted operator overrides — and removed from
    /// `'_global'`; genuinely-global registry keys stay put. Rewinds only
    /// the 022 marker (the table already has the 021 shape), seeds the
    /// orphans, and re-runs `initialize()`.
    #[tokio::test]
    async fn pg_settings_backfill_replicates_orphaned_per_db_keys() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };

        // Rewind the 022 marker. Settings are already empty (truncated by
        // `postgres_storage()`); re-create this test's two logical
        // databases so the fan-out has at least two targets.
        sqlx::raw_sql(
            "DELETE FROM schema_version WHERE version >= 22;
             DELETE FROM databases WHERE id IN ('backfill_alpha', 'backfill_beta');",
        )
        .execute(s.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO databases (id, name, is_default, created_at) VALUES
                 ('backfill_alpha', 'Backfill Alpha', 0, $1),
                 ('backfill_beta', 'Backfill Beta', 0, $1)",
        )
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(s.pool())
        .await
        .unwrap();

        // The post-021 orphaned state: per-DB-role keys stranded in
        // '_global' alongside a genuinely-global registry key.
        sqlx::query(
            "INSERT INTO settings (db_id, key, value) VALUES
                 ('_global', 'task.task_runs_gc.retain_days', '7'),
                 ('_global', 'reports.default_briefing_seeded', 'true'),
                 ('_global', 'dashboard.featured_report_id', 'report-1'),
                 ('_global', 'ai_provider', 'openrouter')",
        )
        .execute(s.pool())
        .await
        .unwrap();

        s.initialize().await.unwrap();

        let db_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM databases ORDER BY id")
            .fetch_all(s.pool())
            .await
            .unwrap();
        assert!(db_ids.len() >= 2, "the fan-out needs at least two targets");

        for key in [
            "task.task_runs_gc.retain_days",
            "reports.default_briefing_seeded",
            "dashboard.featured_report_id",
        ] {
            let scoped: Vec<String> = sqlx::query_scalar(
                "SELECT db_id FROM settings WHERE key = $1 AND db_id <> '_global' ORDER BY db_id",
            )
            .bind(key)
            .fetch_all(s.pool())
            .await
            .unwrap();
            assert_eq!(
                scoped, db_ids,
                "'{key}' must be replicated under every database id"
            );
            assert!(
                s.get_global_setting(key).await.unwrap().is_none(),
                "'{key}' must be gone from the '_global' tier"
            );
        }

        // Values survive the move and surface through the scoped accessors
        // — exactly what keeps the seed guard and operator overrides live.
        assert_eq!(
            s.with_db_id("backfill_alpha")
                .get_setting("task.task_runs_gc.retain_days")
                .await
                .unwrap(),
            Some("7".to_string())
        );
        assert_eq!(
            s.with_db_id("backfill_beta")
                .get_setting("reports.default_briefing_seeded")
                .await
                .unwrap(),
            Some("true".to_string())
        );

        // The genuinely-global key is untouched: still global, never scoped.
        assert_eq!(
            s.get_global_setting("ai_provider").await.unwrap(),
            Some("openrouter".to_string())
        );
        let scoped_provider: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM settings WHERE key = 'ai_provider' AND db_id <> '_global'",
        )
        .fetch_one(s.pool())
        .await
        .unwrap();
        assert_eq!(scoped_provider, 0, "'ai_provider' must not be replicated");
    }

    /// `purge_database_data` must clear the purged database's ledger too:
    /// a deleted DB's GC sweep never runs again, so surviving `task_runs`
    /// rows would leak forever on a shared cluster. The sibling database's
    /// history is untouched.
    #[tokio::test]
    async fn pg_purge_database_data_deletes_task_runs() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        let doomed = s.with_db_id("purge_runs_doomed");
        let survivor = s.with_db_id("purge_runs_survivor");

        let task_id = format!("purge::{}", uuid::Uuid::new_v4());
        let past = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let doomed_row = task_run_row(&task_id, "subject", TaskRunState::Succeeded, &past, None);
        let survivor_row = task_run_row(&task_id, "subject", TaskRunState::Succeeded, &past, None);
        doomed.insert_task_run(&doomed_row).await.unwrap();
        survivor.insert_task_run(&survivor_row).await.unwrap();

        s.purge_database_data("purge_runs_doomed").await.unwrap();

        assert!(
            doomed.get_task_run(&doomed_row.id).await.unwrap().is_none(),
            "the purged database's ledger rows must be deleted"
        );
        assert!(
            survivor
                .get_task_run(&survivor_row.id)
                .await
                .unwrap()
                .is_some(),
            "the sibling database's ledger must survive the purge"
        );
    }

    /// Cross-`db_id` fencing for the ledger sweep: runnable rows are scoped
    /// to the logical database, so one database's sweeper (wiki regen,
    /// crash recovery) can never see — let alone claim — a sibling's
    /// backlog on a shared Postgres cluster.
    #[tokio::test]
    async fn pg_list_runnable_task_runs_scoped_by_db_id() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        let alpha = s.with_db_id("task_runs_fence_alpha");
        let beta = s.with_db_id("task_runs_fence_beta");

        let task_id = format!("fence_sweep::{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now();
        let past = (now - chrono::Duration::minutes(5)).to_rfc3339();

        // Same (task_id, subject) in both databases — legal, because the
        // active-row unique index is keyed on db_id too.
        let alpha_row = task_run_row(&task_id, "subject", TaskRunState::Pending, &past, None);
        let beta_row = task_run_row(&task_id, "subject", TaskRunState::Pending, &past, None);
        alpha.insert_task_run(&alpha_row).await.unwrap();
        beta.insert_task_run(&beta_row).await.unwrap();

        let now_str = now.to_rfc3339();
        let alpha_runnable = alpha
            .list_runnable_task_runs(&task_id, &now_str)
            .await
            .unwrap();
        let ids: Vec<&str> = alpha_runnable.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![alpha_row.id.as_str()],
            "alpha's sweep surfaces only alpha's row"
        );
        let beta_runnable = beta
            .list_runnable_task_runs(&task_id, &now_str)
            .await
            .unwrap();
        let ids: Vec<&str> = beta_runnable.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![beta_row.id.as_str()],
            "beta's sweep surfaces only beta's row"
        );
    }

    /// Cross-`db_id` fencing for retention GC: a sweep in one logical
    /// database must rank and delete only its own history — the sibling's
    /// rows are invisible to both the eligibility CTEs and the DELETE.
    #[tokio::test]
    async fn pg_gc_task_runs_scoped_by_db_id() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        let alpha = s.with_db_id("task_runs_gc_alpha");
        let beta = s.with_db_id("task_runs_gc_beta");

        let task_id = format!("fence_gc::{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now();
        let ancient = (now - chrono::Duration::days(400)).to_rfc3339();

        let alpha_row = gc_run_row(&task_id, Some("subject"), TaskRunState::Succeeded, &ancient);
        let beta_row = gc_run_row(&task_id, Some("subject"), TaskRunState::Succeeded, &ancient);
        alpha.insert_task_run(&alpha_row).await.unwrap();
        beta.insert_task_run(&beta_row).await.unwrap();

        // keep = 0 with both cutoffs at `now`: every terminal row visible
        // to the sweep is eligible — db_id scoping is the only protection.
        let now_str = now.to_rfc3339();
        let deleted = alpha
            .gc_task_runs(0, &now_str, &now_str, 100)
            .await
            .unwrap();
        assert_eq!(deleted, 1, "alpha collects exactly its own history");
        assert!(alpha.get_task_run(&alpha_row.id).await.unwrap().is_none());
        assert!(
            beta.get_task_run(&beta_row.id).await.unwrap().is_some(),
            "the sibling database's history must survive alpha's GC"
        );
    }

    /// Crash recovery against Postgres through the real claim path: a
    /// `running` row whose lease expired (the process died mid-run) is
    /// reclaimed by `ledger::claim_or_create` — same row, no new insert,
    /// no attempt bump — and settles normally. Mirrors
    /// `dispatch_reclaims_running_row_with_expired_lease` in lib.rs.
    #[tokio::test]
    async fn pg_expired_lease_reclaimed_through_claim_path() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        let storage = s.with_db_id("task_runs_reclaim");
        let core = atomic_core::AtomicCore::from_postgres_storage(storage.clone());

        let task_id = format!("crash::{}", uuid::Uuid::new_v4());
        let now = chrono::Utc::now();
        let expired_lease = (now - chrono::Duration::minutes(5)).to_rfc3339();
        let mut crashed = task_run_row(
            &task_id,
            "subject",
            TaskRunState::Running,
            &now.to_rfc3339(),
            Some(&expired_lease),
        );
        crashed.started_at = Some((now - chrono::Duration::minutes(20)).to_rfc3339());
        storage.insert_task_run(&crashed).await.unwrap();

        let handle = atomic_core::scheduler::ledger::claim_or_create(
            &core,
            &task_id,
            Some("subject"),
            TaskRunTrigger::Schedule,
            3,
        )
        .await
        .unwrap()
        .expect("a running row with an expired lease must be reclaimable");
        assert_eq!(
            handle.run().id,
            crashed.id,
            "reclaimed the crashed row — no new row inserted"
        );
        assert_eq!(
            handle.run().attempts,
            1,
            "reclaim must NOT bump attempts — a crash isn't a logic failure"
        );
        assert!(
            handle.run().lease_until.as_deref() > Some(expired_lease.as_str()),
            "reclaim granted a fresh lease"
        );

        assert!(
            handle.complete(None).await.unwrap(),
            "the reclaimer owns the lease and settles the row"
        );
        let row = storage.get_task_run(&crashed.id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskRunState::Succeeded);
        assert_eq!(row.attempts, 1);
    }

    /// Deferral against Postgres through the real fail path: with a
    /// failure-disposition policy installed, `RunHandle::fail` on a
    /// provider-classified error routes to `defer_until` — the row re-arms
    /// `pending` at the policy's horizon with the claim's attempt refunded,
    /// while a logic failure through the same policy still consumes budget.
    /// The PG companion of
    /// `provider_classified_failure_defers_without_consuming_attempts` in
    /// lib.rs, mirroring the reclaim test above.
    #[tokio::test]
    async fn pg_provider_failure_defers_through_fail_path() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        use atomic_core::providers::{classify_provider_failure, ProviderFailureClass};
        use atomic_core::scheduler::ledger::{self, FailureDisposition};

        let storage = s.with_db_id("task_runs_defer");
        let core = atomic_core::AtomicCore::from_postgres_storage(storage.clone());
        core.set_failure_disposition_policy(Some(Arc::new(|error: &str| {
            match classify_provider_failure(error) {
                ProviderFailureClass::Other => FailureDisposition::Fail,
                _ => {
                    FailureDisposition::DeferUntil(chrono::Utc::now() + chrono::Duration::hours(1))
                }
            }
        })));

        let task_id = format!("defer::{}", uuid::Uuid::new_v4());
        let handle = ledger::claim_or_create(
            &core,
            &task_id,
            Some("subject"),
            TaskRunTrigger::Schedule,
            3,
        )
        .await
        .unwrap()
        .expect("fresh claim");
        let run_id = handle.run().id.clone();
        assert_eq!(handle.run().attempts, 1, "the claim charges an attempt");

        // Environmental failure → deferred, attempt refunded.
        assert!(handle
            .fail("Rate limited, retry after 300 seconds".to_string())
            .await
            .unwrap());
        let row = storage.get_task_run(&run_id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskRunState::Pending);
        assert_eq!(
            row.attempts, 0,
            "a provider-classified failure must not consume retry budget"
        );
        assert!(row.lease_until.is_none(), "the deferral released the lease");
        let horizon_mins = (chrono::DateTime::parse_from_rfc3339(&row.next_attempt_at)
            .unwrap()
            .with_timezone(&chrono::Utc)
            - chrono::Utc::now())
        .num_minutes();
        assert!(
            (55..=65).contains(&horizon_mins),
            "deferred to the policy's horizon, got {horizon_mins}m"
        );
        assert_eq!(
            row.last_error.as_deref(),
            Some("Rate limited, retry after 300 seconds")
        );

        // A logic failure through the same policy still consumes budget.
        sqlx::query("UPDATE task_runs SET next_attempt_at = $2 WHERE id = $1")
            .bind(&run_id)
            .bind((chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339())
            .execute(storage.pool())
            .await
            .unwrap();
        let handle = ledger::claim_or_create(
            &core,
            &task_id,
            Some("subject"),
            TaskRunTrigger::Schedule,
            3,
        )
        .await
        .unwrap()
        .expect("re-claim after deferral");
        assert!(handle
            .fail("Parse error: bad JSON".to_string())
            .await
            .unwrap());
        let row = storage.get_task_run(&run_id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskRunState::Pending, "retry scheduled");
        assert_eq!(row.attempts, 1, "logic failures keep consuming the budget");
    }

    /// Deleting a feed settles its stranded ledger rows on Postgres too:
    /// the core path (`delete_feed` → `settle_task_runs_moot`) runs fenced
    /// by `db_id`. Companion to `delete_feed_settles_nonterminal_ledger_rows`
    /// in lib.rs, which drives the same path on SQLite.
    #[tokio::test]
    async fn pg_delete_feed_settles_nonterminal_ledger_rows() {
        let Some(ref s) = postgres_storage().await else {
            eprintln!("Skipping (ATOMIC_TEST_DATABASE_URL not set)");
            return;
        };
        let storage = s.with_db_id("feed_delete_settle");
        let core = atomic_core::AtomicCore::from_postgres_storage(storage.clone());

        let feed = storage
            .create_feed(
                "https://example.com/doomed.xml",
                Some("Doomed"),
                None,
                60,
                &[],
            )
            .await
            .unwrap();
        // A failed poll's backed-off retry: pending with `next_attempt_at`
        // in the future — unclaimable, and unreachable by the poll sweep
        // once the definition row is gone.
        let future = (chrono::Utc::now() + chrono::Duration::minutes(30)).to_rfc3339();
        let row = task_run_row(
            atomic_core::ingest::poller::FEED_POLL_TASK_ID,
            &feed.id,
            TaskRunState::Pending,
            &future,
            None,
        );
        storage.insert_task_run(&row).await.unwrap();

        core.delete_feed(&feed.id).await.unwrap();

        assert!(
            storage
                .find_active_task_run(
                    atomic_core::ingest::poller::FEED_POLL_TASK_ID,
                    Some(&feed.id)
                )
                .await
                .unwrap()
                .is_none(),
            "no non-terminal rows survive feed deletion"
        );
        let settled = storage.get_task_run(&row.id).await.unwrap().unwrap();
        assert_eq!(
            settled.state,
            TaskRunState::Succeeded,
            "settled as moot success — history preserved, not deleted"
        );
        assert!(settled.finished_at.is_some());
    }
}
