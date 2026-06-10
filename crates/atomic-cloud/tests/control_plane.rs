//! Control-plane integration tests.
//!
//! Postgres-gated, mirroring the workspace convention: every test that needs
//! a cluster skips with a message when `ATOMIC_TEST_DATABASE_URL` is unset.
//! Run them single-threaded against the test cluster:
//!
//! ```sh
//! ATOMIC_TEST_DATABASE_URL=postgres://atomic:atomic_test@localhost:5433/atomic_test \
//!     cargo test -p atomic-cloud -- --test-threads=1
//! ```
//!
//! Each test creates a uniquely named control-plane database under the
//! `atomic_cloud_test_` prefix and drops it afterwards even when the test
//! body panics ([`with_control_db`] catches the unwind, cleans up, then
//! resumes it). A once-per-process sweep drops leftovers stranded by prior
//! crashed runs.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use atomic_cloud::reserved_subdomains::is_reserved;
use atomic_cloud::ControlPlane;
use futures::FutureExt;
use sqlx::{Connection, PgConnection};

/// Dedicated prefix for databases created by this suite — the startup sweep
/// matches it and nothing else, so a sweep can never touch real data.
const TEST_DB_PREFIX: &str = "atomic_cloud_test_";

/// Swap the database name in the test-cluster URL. The conventional test URL
/// (`postgres://atomic:atomic_test@localhost:5433/atomic_test`) always ends
/// in `/<database>` with no query string, so a path swap is a string splice.
fn with_db_name(base_url: &str, db_name: &str) -> String {
    let (prefix, _) = base_url
        .rsplit_once('/')
        .expect("test database URL ends in /<database>");
    format!("{prefix}/{db_name}")
}

/// Best-effort drop of leftover `atomic_cloud_test_*` databases from prior
/// crashed runs. Runs once per test process, before the first database is
/// created, so it cannot race a live test under `--test-threads=1` (or any
/// schedule — every creation happens after the sweep completes).
async fn sweep_leftovers(base_url: &str) {
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
            for db_name in leftovers {
                eprintln!("sweeping leftover test database {db_name}");
                let _ = sqlx::raw_sql(&format!(
                    "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
                ))
                .execute(&mut conn)
                .await;
            }
            let _ = conn.close().await;
        })
        .await;
}

async fn drop_database(base_url: &str, db_name: &str) {
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

/// Run `test` against a fresh, uniquely named control-plane database URL,
/// dropping the database afterwards — panic or not. Skips (with a message)
/// when `ATOMIC_TEST_DATABASE_URL` is unset.
async fn with_control_db<F, Fut>(test_name: &str, test: F)
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

    let result = AssertUnwindSafe(test(control_url)).catch_unwind().await;

    drop_database(&base_url, &db_name).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

const SLICE_1_TABLES: &[&str] = &[
    "accounts",
    "account_databases",
    "cloud_tokens",
    "sessions",
    "subdomains_reserved",
];

async fn table_exists(control: &ControlPlane, table: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
    )
    .bind(table)
    .fetch_one(control.pool())
    .await
    .expect("query information_schema")
}

async fn schema_version_rows(control: &ControlPlane) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM schema_version")
        .fetch_one(control.pool())
        .await
        .expect("count schema_version rows")
}

#[tokio::test]
async fn fresh_initialize_applies_all_migrations() {
    with_control_db(
        "fresh_initialize_applies_all_migrations",
        |url| async move {
            // `connect` must create the database — the name is freshly minted.
            let control = ControlPlane::connect(&url)
                .await
                .expect("connect-or-create");
            let applied = control.initialize().await.expect("run migrations");

            assert!(applied >= 1, "fresh database must apply migrations");
            assert_eq!(
                schema_version_rows(&control).await,
                applied as i64,
                "each applied migration records exactly one schema_version row"
            );
            for table in SLICE_1_TABLES {
                assert!(
                    table_exists(&control, table).await,
                    "slice-1 table {table:?} should exist after initialize"
                );
            }

            // Schema sanity: the accounts.subdomain UNIQUE constraint is what
            // makes subdomain claiming race-free at signup — pin it.
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
             VALUES ('acct-1', 'kenny', 'k@example.com', 'active', 'free')",
            )
            .execute(control.pool())
            .await
            .expect("insert account");
            let duplicate = sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
             VALUES ('acct-2', 'kenny', 'other@example.com', 'active', 'free')",
            )
            .execute(control.pool())
            .await;
            assert!(
                duplicate.is_err(),
                "duplicate subdomain must violate the UNIQUE constraint"
            );
        },
    )
    .await;
}

#[tokio::test]
async fn reopen_applies_zero_migrations() {
    with_control_db("reopen_applies_zero_migrations", |url| async move {
        let first = ControlPlane::connect(&url).await.expect("first connect");
        let applied_first = first.initialize().await.expect("first initialize");
        assert!(applied_first >= 1);
        let rows_after_first = schema_version_rows(&first).await;
        drop(first);

        // Reopen: `connect` takes the database-already-exists path and
        // `initialize` must be a no-op.
        let second = ControlPlane::connect(&url).await.expect("reopen connect");
        let applied_second = second.initialize().await.expect("reopen initialize");
        assert_eq!(applied_second, 0, "reopen must apply zero migrations");
        assert_eq!(
            schema_version_rows(&second).await,
            rows_after_first,
            "reopen must not add schema_version rows"
        );
    })
    .await;
}

#[tokio::test]
async fn concurrent_initialize_is_serialized_by_advisory_lock() {
    with_control_db(
        "concurrent_initialize_is_serialized_by_advisory_lock",
        |url| async move {
            // Two independent handles (separate pools) against the same
            // fresh database. The advisory lock serializes them: exactly one
            // applies each migration, the other observes the recorded
            // version and applies nothing.
            let a = ControlPlane::connect(&url).await.expect("connect a");
            let b = ControlPlane::connect(&url).await.expect("connect b");

            let (applied_a, applied_b) = tokio::join!(a.initialize(), b.initialize());
            let applied_a = applied_a.expect("initialize a succeeds");
            let applied_b = applied_b.expect("initialize b succeeds");

            let total_rows = schema_version_rows(&a).await;
            assert_eq!(
                (applied_a + applied_b) as i64,
                total_rows,
                "between them the two racers apply each migration exactly once"
            );
            for table in SLICE_1_TABLES {
                assert!(table_exists(&a, table).await, "{table:?} should exist");
            }
        },
    )
    .await;
}

#[test]
fn reserved_subdomain_lookup() {
    // (candidate, expected_reserved)
    let cases: &[(&str, bool)] = &[
        // The names the plan calls out explicitly.
        ("www", true),
        ("app", true),
        ("api", true),
        ("mcp", true),
        ("admin", true),
        ("support", true),
        ("status", true),
        ("docs", true),
        ("blog", true),
        ("auth", true),
        ("login", true),
        ("signup", true),
        // Usual suspects from the wider list.
        ("mail", true),
        ("postmaster", true),
        ("staging", true),
        // Case-insensitive defense in depth.
        ("WWW", true),
        ("Admin", true),
        // Legitimate vanity slugs.
        ("kenny", false),
        ("my-notes", false),
        ("atomic-fan", false),
        // Near-misses must not match.
        ("wwww", false),
        ("api2", false),
        ("docss", false),
        ("", false),
    ];
    for &(candidate, expected) in cases {
        assert_eq!(
            is_reserved(candidate),
            expected,
            "is_reserved({candidate:?}) should be {expected}"
        );
    }
}
