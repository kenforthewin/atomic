//! Multi-database isolation tests.
//!
//! CLAUDE.md flags this as the area most prone to silent cross-contamination
//! bugs. The settings routing has two flavors: workspace-only keys (theme,
//! font, credentials, machine URLs — see `settings::WORKSPACE_ONLY_KEYS`)
//! always live in `registry.db` and are shared across every database;
//! everything else is *overridable*, meaning a per-DB row in that database's
//! settings table wins over the registry-side workspace default.
//!
//! These tests open a real `DatabaseManager`, create two data databases
//! inside it, and assert:
//!
//!   1. Atoms created in DB1 are invisible from DB2 (and vice versa).
//!   2. Tags are isolated per-database.
//!   3. Workspace-only keys (e.g. `theme`) are shared — setting one on DB1
//!      is visible from DB2.
//!   4. Overridable keys (e.g. `provider`) routed via `set_setting` with a
//!      registry attached and N≥2 land in the active DB's per-DB table —
//!      DB2 keeps seeing the workspace default until it sets its own override.
//!      Postgres mode has no registry, so `set_setting` writes the storage
//!      layer's global tier (`db_id = '_global'`) — deliberately shared
//!      across logical databases, with no override layer. The SQLite-only
//!      assertion below pins the override behavior where it exists.

mod support;

use std::collections::HashSet;

use atomic_core::{CreateAtomRequest, DatabaseManager};
use tempfile::TempDir;

#[tokio::test]
async fn isolation_sqlite() {
    let dir = TempDir::new().expect("tempdir");
    let manager = DatabaseManager::new(dir.path()).expect("open manager");
    run_isolation(&manager).await;

    // ---------- Overridable settings are per-DB (SQLite + registry only) ----------
    //
    // Postgres mode has no registry, so the Postgres `set_setting` path
    // writes the global settings tier — there's no override layer to test.
    // SQLite always has a registry, so this is the deployment where the
    // override semantics actually take effect.
    let dbs = manager.list_databases().await.expect("list").0;
    let alpha = dbs
        .iter()
        .find(|d| d.name == "isolation_alpha")
        .expect("alpha exists");
    let beta = dbs
        .iter()
        .find(|d| d.name == "isolation_beta")
        .expect("beta exists");
    let core1 = manager.get_core(&alpha.id).await.expect("get_core alpha");
    let core2 = manager.get_core(&beta.id).await.expect("get_core beta");

    // Set an overridable key on alpha. With N≥2 this writes to alpha's per-DB
    // settings table — beta should keep seeing the workspace default.
    core1
        .set_setting("provider", "ollama")
        .await
        .expect("set provider override on alpha");

    let s1 = core1.get_settings().await.expect("get_settings alpha");
    let s2 = core2.get_settings().await.expect("get_settings beta");
    assert_eq!(
        s1.get("provider").map(String::as_str),
        Some("ollama"),
        "alpha sees its own override"
    );
    assert_ne!(
        s2.get("provider").map(String::as_str),
        Some("ollama"),
        "beta MUST NOT see alpha's per-DB override — that would leak the active \
         DB's choice into every other DB and break the inheritance model."
    );

    // Clearing the override on alpha makes it fall back to whatever beta sees
    // (the workspace default in registry.db).
    core1
        .clear_override("provider")
        .await
        .expect("clear override on alpha");
    let s1_after = core1.get_settings().await.expect("get_settings alpha");
    assert_eq!(
        s1_after.get("provider").map(String::as_str),
        s2.get("provider").map(String::as_str),
        "after clear_override, alpha resolves to the same value beta sees"
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn isolation_postgres() {
    let Ok(url) = std::env::var("ATOMIC_TEST_DATABASE_URL") else {
        eprintln!("isolation_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    };
    // Shared Postgres deployment — start clean so leftover rows from earlier
    // suites don't make "DB2 sees DB1's data" look like a leak when it's
    // actually prior test residue.
    support::truncate_postgres_for_test(&url).await;
    let dir = TempDir::new().expect("tempdir");
    let manager = DatabaseManager::new_postgres(dir.path(), &url)
        .await
        .expect("open postgres manager");
    run_isolation(&manager).await;
}

async fn run_isolation(manager: &DatabaseManager) {
    // Create two named databases. Using explicit names (rather than the
    // seeded default) means the test is robust against reordering and
    // survives in a shared Postgres where another suite may have created
    // different defaults earlier.
    let db1 = manager
        .create_database("isolation_alpha")
        .await
        .expect("create db alpha");
    let db2 = manager
        .create_database("isolation_beta")
        .await
        .expect("create db beta");
    assert_ne!(db1.id, db2.id, "two databases must have distinct ids");

    let core1 = manager.get_core(&db1.id).await.expect("get_core alpha");
    let core2 = manager.get_core(&db2.id).await.expect("get_core beta");

    // ---------- Atom isolation ----------
    let a1 = core1
        .create_atom(
            CreateAtomRequest {
                content: "alpha-only content".to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .expect("create_atom alpha")
        .expect("alpha atom inserted");
    let a2 = core2
        .create_atom(
            CreateAtomRequest {
                content: "beta-only content".to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .expect("create_atom beta")
        .expect("beta atom inserted");

    assert!(
        core1.get_atom(&a1.atom.id).await.unwrap().is_some(),
        "alpha should see its own atom"
    );
    assert!(
        core1.get_atom(&a2.atom.id).await.unwrap().is_none(),
        "alpha MUST NOT see beta's atom (leak!)"
    );
    assert!(
        core2.get_atom(&a2.atom.id).await.unwrap().is_some(),
        "beta should see its own atom"
    );
    assert!(
        core2.get_atom(&a1.atom.id).await.unwrap().is_none(),
        "beta MUST NOT see alpha's atom (leak!)"
    );

    // ---------- Tag isolation ----------
    core1
        .create_tag("AlphaOnlyTag", None)
        .await
        .expect("create tag in alpha");
    core2
        .create_tag("BetaOnlyTag", None)
        .await
        .expect("create tag in beta");

    let names1: HashSet<String> = core1
        .get_all_tags()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.tag.name)
        .collect();
    let names2: HashSet<String> = core2
        .get_all_tags()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.tag.name)
        .collect();

    assert!(
        names1.contains("AlphaOnlyTag"),
        "alpha should see its own tag; got {:?}",
        names1
    );
    assert!(
        !names1.contains("BetaOnlyTag"),
        "alpha MUST NOT see beta's tag; got {:?}",
        names1
    );
    assert!(
        names2.contains("BetaOnlyTag"),
        "beta should see its own tag; got {:?}",
        names2
    );
    assert!(
        !names2.contains("AlphaOnlyTag"),
        "beta MUST NOT see alpha's tag; got {:?}",
        names2
    );

    // ---------- Workspace-only keys are shared across DBs ----------
    //
    // Workspace-only keys (theme, font, credentials, machine URLs) live in
    // the registry and are intentionally global — setting `theme` on alpha
    // must show up on beta. This is the contract we *want* for these keys;
    // overridable keys behave differently and are pinned per-deployment in
    // each test entry point.
    core1
        .set_setting("theme", "dracula")
        .await
        .expect("set theme on alpha");
    let s2 = core2.get_settings().await.expect("get_settings on beta");
    assert_eq!(
        s2.get("theme").map(String::as_str),
        Some("dracula"),
        "workspace-only keys (here: theme) MUST be visible across all \
         databases — they live in registry.db and the resolver short-circuits \
         the per-DB layer for them."
    );
}
