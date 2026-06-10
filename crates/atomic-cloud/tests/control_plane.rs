//! Control-plane integration tests.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use atomic_cloud::reserved_subdomains::is_reserved;
use atomic_cloud::ControlPlane;
use support::with_control_db;

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
