//! Ownership-scope isolation for the migration job registry.
//!
//! The registry is process-global, so under a multi-tenant composition every
//! lookup must present the scope the job was created with (see
//! `db_extractor::RequestJobScope`). A mismatch — including a missing scope —
//! reads as not-found so a foreign principal can't confirm the job exists.
//! The cloud e2e suite covers the HTTP plumbing; this pins the registry
//! semantics both layers rely on.

use atomic_core::AtomicCore;
use atomic_server::migration_jobs::{MigrationJobManager, PushRequest};
use tempfile::TempDir;

fn push_request() -> PushRequest {
    PushRequest {
        // Unroutable port: the spawned job fails fast, which is fine — scope
        // checks apply to the registry entry regardless of job outcome.
        target_url: "http://127.0.0.1:9".to_string(),
        target_token: "irrelevant".to_string(),
        database_id: "default".to_string(),
        name: None,
        pause_feeds: None,
    }
}

#[tokio::test]
async fn job_lookups_require_matching_scope() {
    let dir = TempDir::new().unwrap();
    let core = AtomicCore::open_or_create(dir.path().join("scope.db")).expect("open core");
    let jobs = MigrationJobManager::for_tests(dir.path().join("jobs"));

    let job = jobs
        .start_push(
            core,
            push_request(),
            "Default".to_string(),
            Some("acct-a".to_string()),
        )
        .expect("start push");

    assert!(
        jobs.status(&job.id, Some("acct-a")).is_ok(),
        "owner sees it"
    );
    assert!(
        jobs.status(&job.id, Some("acct-b")).is_err(),
        "foreign scope reads as not-found"
    );
    assert!(
        jobs.status(&job.id, None).is_err(),
        "missing scope reads as not-found"
    );
    assert!(jobs.cancel_or_delete(&job.id, Some("acct-b")).is_err());
    assert!(jobs.cancel_or_delete(&job.id, None).is_err());
    assert!(jobs.cancel_or_delete(&job.id, Some("acct-a")).is_ok());
}

#[tokio::test]
async fn unscoped_jobs_match_only_unscoped_lookups() {
    let dir = TempDir::new().unwrap();
    let core = AtomicCore::open_or_create(dir.path().join("unscoped.db")).expect("open core");
    let jobs = MigrationJobManager::for_tests(dir.path().join("jobs"));

    let job = jobs
        .start_push(core, push_request(), "Default".to_string(), None)
        .expect("start push");

    assert!(
        jobs.status(&job.id, None).is_ok(),
        "standalone server: no scope, plain lookup works"
    );
    assert!(
        jobs.status(&job.id, Some("acct-a")).is_err(),
        "a scoped lookup must not match an unscoped job"
    );
}
