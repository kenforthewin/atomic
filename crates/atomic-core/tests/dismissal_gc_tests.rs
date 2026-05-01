//! Unit tests for the health_dismissals GC storage method.

use atomic_core::{AtomicCore, CreateAtomRequest};
use tempfile::TempDir;

async fn setup() -> (AtomicCore, TempDir) {
    let dir = TempDir::new().expect("create tempdir");
    let core = AtomicCore::open_or_create(dir.path().join("test.db")).expect("open sqlite");
    (core, dir)
}

#[tokio::test]
async fn test_gc_dismissals_removes_expired_and_orphaned() {
    let (core, _dir) = setup().await;

    // Create two real atoms A and B.
    let atom_a = core
        .create_atom(
            CreateAtomRequest {
                content: "Atom A content".to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .expect("create A");
    let atom_b = core
        .create_atom(
            CreateAtomRequest {
                content: "Atom B content".to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .expect("create B");

    let id_a = atom_a.unwrap().atom.id.clone();
    let id_b = atom_b.unwrap().atom.id.clone();

    // Dismissal for a non-existent atom C (should be GC'd).
    let fake_c = "00000000-0000-0000-0000-000000000099";
    core.dismiss_health_item("boilerplate_pollution", fake_c, "orphan", None)
        .await
        .expect("dismiss fake C");

    // Pair dismissal A__B — both atoms exist (should survive).
    let pair_ab = format!("{}__{}", id_a, id_b);
    core.dismiss_health_item("content_overlap", &pair_ab, "reviewed", None)
        .await
        .expect("dismiss pair A__B");

    // Expired deferred dismissal B__A.
    let pair_ba = format!("{}__{}", id_b, id_a);
    let past = "2000-01-01T00:00:00+00:00";
    core.dismiss_health_item("content_overlap", &pair_ba, "deferred", Some(past))
        .await
        .expect("dismiss expired pair B__A");

    // GC should remove 2 rows: orphan C + expired B__A.
    let removed = core
        .gc_health_dismissals()
        .await
        .expect("gc_dismissals");
    assert_eq!(removed, 2, "expected 2 rows deleted, got {removed}");

    // Only A__B survives.
    let remaining = core
        .list_dismissed_keys("content_overlap")
        .await
        .expect("list dismissed");
    let keys: Vec<String> = remaining.into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec![pair_ab], "only pair A__B should remain");
}
