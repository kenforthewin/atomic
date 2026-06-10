//! Shared infrastructure for atomic-cloud integration tests.
//!
//! Postgres-gated, mirroring the workspace convention: every test that needs
//! a cluster skips with a message when `ATOMIC_TEST_DATABASE_URL` is unset.
//! Run single-threaded against the test cluster:
//!
//! ```sh
//! ATOMIC_TEST_DATABASE_URL=postgres://atomic:atomic_test@localhost:5433/atomic_test \
//!     cargo test -p atomic-cloud -- --test-threads=1
//! ```
//!
//! Each test creates a uniquely named control-plane database under the
//! `atomic_cloud_test_` prefix and drops it afterwards even when the test
//! body panics ([`with_control_db`] catches the unwind, cleans up, then
//! resumes it). Tenant databases provisioned during a test are discovered
//! through that control database (its `account_databases` rows plus names
//! derived from `accounts.id`) and dropped in the same guard. A
//! once-per-process sweep removes leftovers stranded by prior crashed runs —
//! it matches only the dedicated test prefix, then chases each leftover
//! control database's tenant references, so it can never touch real data.

#![allow(dead_code)] // Helpers are per-test; not every test binary uses every helper.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use atomic_cloud::provision::is_tenant_db_name;
use atomic_cloud::tenant_db_name;
use futures::FutureExt;
use sqlx::{Connection, PgConnection};

/// Dedicated prefix for control-plane databases created by this suite — the
/// startup sweep matches it and nothing else.
pub const TEST_DB_PREFIX: &str = "atomic_cloud_test_";

/// Swap the database name in the test-cluster URL. The conventional test URL
/// (`postgres://atomic:atomic_test@localhost:5433/atomic_test`) always ends
/// in `/<database>` with no query string, so a path swap is a string splice.
pub fn with_db_name(base_url: &str, db_name: &str) -> String {
    let (prefix, _) = base_url
        .rsplit_once('/')
        .expect("test database URL ends in /<database>");
    format!("{prefix}/{db_name}")
}

/// Tenant databases referenced by a control-plane database: explicit
/// `account_databases.db_name` rows plus names derived from `accounts.id`
/// (covering provisions that crashed before the mapping row was written).
/// Best-effort — a missing database or absent tables yields an empty list.
async fn referenced_tenant_dbs(control_url: &str) -> Vec<String> {
    let Ok(mut conn) = PgConnection::connect(control_url).await else {
        return Vec::new();
    };
    let mut names: Vec<String> = sqlx::query_scalar("SELECT db_name FROM account_databases")
        .fetch_all(&mut conn)
        .await
        .unwrap_or_default();
    let account_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM accounts")
        .fetch_all(&mut conn)
        .await
        .unwrap_or_default();
    let _ = conn.close().await;

    for id in account_ids {
        if let Ok(uuid) = uuid::Uuid::parse_str(&id) {
            let derived = tenant_db_name(uuid);
            if !names.contains(&derived) {
                names.push(derived);
            }
        }
    }
    // Belt and braces: only ever drop names with the exact generated shape.
    names.retain(|name| is_tenant_db_name(name));
    names
}

/// Best-effort drop of leftover `atomic_cloud_test_*` databases — and the
/// tenant databases they reference — from prior crashed runs. Runs once per
/// test process, before the first database is created, so it cannot race a
/// live test under `--test-threads=1` (or any schedule — every creation
/// happens after the sweep completes).
pub async fn sweep_leftovers(base_url: &str) {
    static SWEEP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SWEEP
        .get_or_init(|| async {
            let Ok(mut conn) = PgConnection::connect(base_url).await else {
                return;
            };
            let pattern = format!("{}%", TEST_DB_PREFIX);
            let leftovers: Vec<String> =
                sqlx::query_scalar("SELECT datname FROM pg_database WHERE datname LIKE $1")
                    .bind(&pattern)
                    .fetch_all(&mut conn)
                    .await
                    .unwrap_or_default();
            let _ = conn.close().await;

            for db_name in leftovers {
                eprintln!("sweeping leftover test database {db_name}");
                for tenant in referenced_tenant_dbs(&with_db_name(base_url, &db_name)).await {
                    eprintln!("sweeping leftover tenant database {tenant}");
                    try_drop_database(base_url, &tenant).await;
                }
                try_drop_database(base_url, &db_name).await;
            }
        })
        .await;
}

/// Create a database on the test cluster (plain `CREATE DATABASE`; the name
/// comes from test code, not user input). Pair with [`with_db_guard`] so it
/// is dropped even when the test panics.
pub async fn create_database(base_url: &str, db_name: &str) {
    let mut conn = PgConnection::connect(base_url)
        .await
        .expect("connect for test-database creation");
    sqlx::raw_sql(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&mut conn)
        .await
        .expect("create test database");
    let _ = conn.close().await;
}

pub async fn drop_database(base_url: &str, db_name: &str) {
    let mut conn = PgConnection::connect(base_url)
        .await
        .expect("connect for test-database cleanup");
    // WITH (FORCE) terminates any straggler pool connections; sqlx pool drop
    // is asynchronous, so some may still be open when cleanup runs.
    sqlx::raw_sql(&format!(
        "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
    ))
    .execute(&mut conn)
    .await
    .expect("drop test database");
    let _ = conn.close().await;
}

/// Non-panicking [`drop_database`], for sweep paths where a failed drop
/// should not mask the test result.
async fn try_drop_database(base_url: &str, db_name: &str) {
    let Ok(mut conn) = PgConnection::connect(base_url).await else {
        return;
    };
    let _ = sqlx::raw_sql(&format!(
        "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
    ))
    .execute(&mut conn)
    .await;
    let _ = conn.close().await;
}

/// Run `test` against a fresh, uniquely named control-plane database URL,
/// dropping that database — and every tenant database it references —
/// afterwards, panic or not. Skips (with a message) when
/// `ATOMIC_TEST_DATABASE_URL` is unset.
pub async fn with_control_db<F, Fut>(test_name: &str, test: F)
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = ()>,
{
    let Ok(base_url) = std::env::var("ATOMIC_TEST_DATABASE_URL") else {
        eprintln!("{test_name}: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    };
    sweep_leftovers(&base_url).await;

    let db_name = format!("{TEST_DB_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let control_url = with_db_name(&base_url, &db_name);

    let result = AssertUnwindSafe(test(control_url.clone()))
        .catch_unwind()
        .await;

    // Tenant databases first (their names live in the control database),
    // then the control database itself.
    for tenant in referenced_tenant_dbs(&control_url).await {
        try_drop_database(&base_url, &tenant).await;
    }
    drop_database(&base_url, &db_name).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Run `body`, then drop `db_name` from the cluster even if `body` panics.
/// For tests that create an extra database (e.g. a reference schema) outside
/// [`with_control_db`]'s bookkeeping.
pub async fn with_db_guard<F, Fut>(base_url: &str, db_name: &str, body: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let result = AssertUnwindSafe(body()).catch_unwind().await;
    drop_database(base_url, db_name).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
