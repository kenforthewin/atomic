//! Integration tests for health batch dismissal.
//!
//! Tests that multiple dismiss operations all succeed — analogous to what
//! the batch endpoint does per-item.

use atomic_core::AtomicCore;
use tempfile::TempDir;

async fn setup() -> (AtomicCore, TempDir) {
    let dir = TempDir::new().expect("create tempdir");
    let core = AtomicCore::open_or_create(dir.path().join("test.db"))
        .expect("open sqlite");
    (core, dir)
}

#[tokio::test]
async fn test_batch_dismiss_records_all_items() {
    let (core, _dir) = setup().await;

    // Simulate what the batch endpoint does: dismiss multiple items in sequence.
    core.dismiss_health_item("content_overlap", "a__b", "ignored_pair", None)
        .await
        .expect("dismiss a__b");
    core.dismiss_health_item("content_overlap", "c__d", "ignored_pair", None)
        .await
        .expect("dismiss c__d");

    // Upsert semantics: re-dismissing with a different reason should not error.
    core.dismiss_health_item("content_overlap", "a__b", "resolved_other", None)
        .await
        .expect("re-dismiss a__b");

    // Undismiss succeeds.
    core.undismiss_health_item("content_overlap", "a__b")
        .await
        .expect("undismiss a__b");

    // Undismissing a non-existent key is idempotent.
    core.undismiss_health_item("content_overlap", "does_not_exist")
        .await
        .expect("undismiss missing key is idempotent");
}
