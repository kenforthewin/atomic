//! Control-plane integration tests.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use atomic_cloud::reserved_subdomains::is_reserved;
use atomic_cloud::ControlPlane;
use support::with_control_db;

/// Every table the control-plane migrations create (001-008; 005, 007 and
/// 008 only add columns).
const CONTROL_TABLES: &[&str] = &[
    "accounts",
    "account_databases",
    "cloud_tokens",
    "sessions",
    "subdomains_reserved",
    "magic_links",
    "provider_credentials",
    "dispatch_hints",
];

/// The migration-tracking columns 008 adds to `account_databases` (plan:
/// "Schema migration on deploy").
const MIGRATION_TRACKING_COLUMNS: &[&str] = &[
    "last_migrated_version",
    "last_migrated_at",
    "migration_failed_at",
    "last_migration_error",
    "migration_retry_after",
    "migration_retry_count",
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

async fn column_exists(control: &ControlPlane, table: &str, column: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.columns \
         WHERE table_name = $1 AND column_name = $2)",
    )
    .bind(table)
    .bind(column)
    .fetch_one(control.pool())
    .await
    .expect("query information_schema columns")
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
            for table in CONTROL_TABLES {
                assert!(
                    table_exists(&control, table).await,
                    "table {table:?} should exist after initialize"
                );
            }
            for column in MIGRATION_TRACKING_COLUMNS {
                assert!(
                    column_exists(&control, "account_databases", column).await,
                    "column account_databases.{column} should exist after initialize"
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
            for table in CONTROL_TABLES {
                assert!(table_exists(&a, table).await, "{table:?} should exist");
            }
        },
    )
    .await;
}

/// Migration 008's backfill: rows that exist before the migration-tracking
/// columns arrive (i.e. every tenant provisioned by a pre-008 binary) are
/// stamped as current, because provision_account always ran the full tenant
/// migration set before writing them — see 008's header comment for the
/// full invariant. Drives the world as it was at version 7, inserts rows
/// the way pre-008 provisioning wrote them, then applies 008 directly.
#[tokio::test]
async fn migration_008_backfills_preexisting_active_rows() {
    with_control_db(
        "migration_008_backfills_preexisting_active_rows",
        |url| async move {
            let control = ControlPlane::connect(&url)
                .await
                .expect("connect-or-create");

            // Bring the schema to exactly version 7 (the embedded files are
            // the source of truth; 008 is deliberately NOT applied yet).
            for sql in [
                include_str!("../migrations/001_control_plane.sql"),
                include_str!("../migrations/002_magic_links.sql"),
                include_str!("../migrations/003_subdomain_reservation_age.sql"),
                include_str!("../migrations/004_provider_credentials.sql"),
                include_str!("../migrations/005_provider_generation.sql"),
                include_str!("../migrations/006_dispatch_hints.sql"),
                include_str!("../migrations/007_provider_backpressure.sql"),
            ] {
                sqlx::raw_sql(sql)
                    .execute(control.pool())
                    .await
                    .expect("apply pre-008 migration");
            }

            // Rows as pre-008 provisioning wrote them: no tracking columns.
            // (The db_names reference no real databases — nothing here opens
            // a tenant; the support guard's cleanup tolerates that.)
            for (id, subdomain, db_name_char, db_status) in [
                ("acct-current", "current", "b", "active"),
                ("acct-parked", "parked", "c", "retired"),
            ] {
                sqlx::query(
                    "INSERT INTO accounts (id, subdomain, email, status, plan) \
                     VALUES ($1, $2, 'k@example.com', 'active', 'free')",
                )
                .bind(id)
                .bind(subdomain)
                .execute(control.pool())
                .await
                .expect("insert account");
                sqlx::query(
                    "INSERT INTO account_databases (account_id, cluster_id, db_name, status) \
                     VALUES ($1, 'c1', $2, $3)",
                )
                .bind(id)
                .bind(format!("acct_{}", db_name_char.repeat(26)))
                .bind(db_status)
                .execute(control.pool())
                .await
                .expect("insert mapping row");
            }

            sqlx::raw_sql(include_str!("../migrations/008_migration_tracking.sql"))
                .execute(control.pool())
                .await
                .expect("apply migration 008");

            // The active row is stamped with 008's frozen literal (22, the
            // compiled tenant target at authoring time — at-or-below the
            // current target by the additive-only discipline, pinned in
            // src/fleet_migration.rs) and a fresh last_migrated_at; the
            // non-active row keeps the column defaults.
            type Tracking = (i32, Option<chrono::DateTime<chrono::Utc>>, i32);
            let (version, at, retries): Tracking = sqlx::query_as(
                "SELECT last_migrated_version, last_migrated_at, migration_retry_count \
                 FROM account_databases WHERE account_id = 'acct-current'",
            )
            .fetch_one(control.pool())
            .await
            .expect("read active row");
            assert_eq!(
                version, 22,
                "active rows are backfilled to the frozen stamp"
            );
            assert!(at.is_some(), "backfill records last_migrated_at");
            assert_eq!(retries, 0);

            let (version, at, _): Tracking = sqlx::query_as(
                "SELECT last_migrated_version, last_migrated_at, migration_retry_count \
                 FROM account_databases WHERE account_id = 'acct-parked'",
            )
            .fetch_one(control.pool())
            .await
            .expect("read non-active row");
            assert_eq!(version, 0, "non-active rows keep the column default");
            assert!(at.is_none());

            // The runner sees the manual application as already-current.
            let applied = control.initialize().await.expect("initialize");
            assert_eq!(applied, 0, "schema is current after manual 008");
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
