//! SQLite → Postgres migration tests.
//!
//! All tests require the `postgres` feature plus `ATOMIC_TEST_DATABASE_URL`
//! pointing at a Postgres 16+ instance with pgvector; they skip gracefully
//! when the env var is unset. Tests share one Postgres database and truncate
//! it, so they serialize on a file-level lock rather than relying on
//! `--test-threads=1`.
#![cfg(feature = "postgres")]

use atomic_core::db::Database;
use atomic_core::migrate::{
    migrate_sqlite_to_postgres, MigrationEvent, MigrationOptions, MigrationReport,
};
use atomic_core::storage::PostgresStorage;
use atomic_core::AtomicCoreError;
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Mutex;
use tempfile::TempDir;

static PG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn pg_storage() -> Option<PostgresStorage> {
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL").ok()?;
    let pg = PostgresStorage::connect(&url, "migrate-test")
        .await
        .expect("connect postgres");
    pg.initialize().await.expect("run postgres migrations");
    truncate_all(pg.pool()).await;
    Some(pg)
}

async fn truncate_all(pool: &PgPool) {
    sqlx::raw_sql(
        "TRUNCATE atoms, tags, atom_tags, atom_chunks, atom_positions, atom_links, \
         atom_pipeline_jobs, semantic_edges, atom_clusters, tag_embeddings, \
         wiki_articles, wiki_citations, wiki_links, wiki_article_versions, wiki_proposals, \
         conversations, conversation_tags, chat_messages, chat_tool_calls, chat_citations, \
         feeds, feed_tags, feed_items, reports, report_findings, report_finding_citations, \
         task_runs, databases CASCADE",
    )
    .execute(pool)
    .await
    .expect("truncate postgres test tables");

    // Reseed the row `DatabaseManager::new_postgres` bootstraps on an empty
    // instance. Other suites sharing this Postgres database truncate content
    // tables but not `databases`, and expect a default row to exist.
    sqlx::query(
        "INSERT INTO databases (id, name, is_default, created_at) VALUES ('default', 'Default', 1, $1)",
    )
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .expect("reseed default database row");
}

/// Databases created by a migration (everything but the seeded default).
async fn migrated_database_count(pool: &PgPool) -> i64 {
    count(pool, "SELECT COUNT(*) FROM databases WHERE is_default = 0").await
}

/// Build a SQLite fixture exercising every migrated table, including dirty
/// data a real user database can contain: an orphaned atom_tags row and
/// chunk (SQLite FK enforcement is off by default) and a feed item pointing
/// at a deleted atom.
fn seed_source_db(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("source.db");
    // Create the schema at the current version, then seed over a plain
    // connection (FK pragma off, like most real-world writers).
    drop(Database::open_or_create(&path).expect("create source schema"));
    let conn = rusqlite::Connection::open(&path).expect("open source for seeding");

    conn.execute_batch(
        r#"
        -- The bundled SQLite enforces foreign keys by default; turn them off
        -- so the fixture can contain the orphaned rows a database edited by
        -- external tools (where the pragma defaults off) may hold.
        PRAGMA foreign_keys = OFF;

        INSERT INTO tags (id, name, parent_id, created_at, atom_count, is_autotag_target, autotag_description)
        VALUES ('tag-root', 'Topics', NULL, '2026-01-01T00:00:00Z', 1, 1, 'general topics'),
               ('tag-child', 'Rust', 'tag-root', '2026-01-02T00:00:00Z', 2, 0, '');

        INSERT INTO atoms (id, content, title, snippet, source_url, source, published_at,
                           created_at, updated_at, embedding_status, tagging_status,
                           edges_status, embedding_error, tagging_error, kind)
        VALUES ('atom-1', '# Rust note' || char(10) || char(10) || 'A note about ownership and borrowing.',
                'Rust note', 'A note about ownership…', 'https://example.com/rust', 'web', NULL,
                '2026-01-03T00:00:00Z', '2026-01-03T01:00:00Z', 'complete', 'complete',
                'complete', NULL, NULL, 'captured'),
               ('atom-2', 'hello world', 'hello', 'hello world', NULL, NULL, NULL,
                '2026-01-04T00:00:00Z', '2026-01-04T00:00:00Z', 'failed', 'pending',
                'none', 'embed boom', 'tag boom', 'captured'),
               ('atom-3', 'finding body', 'Daily finding', 'finding body', NULL, NULL, NULL,
                '2026-01-05T00:00:00Z', '2026-01-05T00:00:00Z', 'complete', 'complete',
                'complete', NULL, NULL, 'finding');

        INSERT INTO atom_tags (atom_id, tag_id, source)
        VALUES ('atom-1', 'tag-child', 'manual'),
               ('atom-1', 'tag-root', 'auto'),
               ('atom-2', 'tag-child', 'auto'),
               ('ghost-atom', 'tag-child', 'auto'); -- orphan: must be skipped

        INSERT INTO atom_chunks (id, atom_id, chunk_index, content, embedding)
        VALUES ('chunk-1', 'atom-1', 0, 'A note about ownership and borrowing.', x'00112233'),
               ('chunk-2', 'atom-2', 0, 'hello world', NULL),
               ('chunk-ghost', 'ghost-atom', 0, 'orphan', NULL); -- orphan: must be skipped

        INSERT INTO atom_positions (atom_id, x, y, updated_at)
        VALUES ('atom-1', 1.5, -2.5, '2026-01-06T00:00:00Z');

        INSERT INTO atom_links (id, source_atom_id, target_atom_id, raw_target, label,
                                target_kind, status, start_offset, end_offset, created_at, updated_at)
        VALUES ('link-1', 'atom-1', 'atom-2', 'hello', NULL, 'atom', 'resolved', 2, 9,
                '2026-01-06T00:00:00Z', '2026-01-06T00:00:00Z');

        INSERT INTO wiki_articles (id, tag_id, content, created_at, updated_at, atom_count)
        VALUES ('wa-1', 'tag-child', 'Rust is [1] great. See [[Topics]].',
                '2026-01-07T00:00:00Z', '2026-01-07T00:00:00Z', 2);

        INSERT INTO wiki_citations (id, wiki_article_id, citation_index, atom_id, chunk_index, excerpt)
        VALUES ('wc-1', 'wa-1', 1, 'atom-1', 0, 'ownership and borrowing');

        INSERT INTO wiki_links (id, source_article_id, target_tag_name, target_tag_id, created_at)
        VALUES ('wl-1', 'wa-1', 'Topics', 'tag-root', '2026-01-07T00:00:00Z'),
               -- Dangling link: its tag was deleted (SQLite's ON DELETE SET
               -- NULL). Must migrate as a NULL target, not fail or be dropped.
               ('wl-dangling', 'wa-1', 'Deleted Topic', NULL, '2026-01-07T00:00:00Z');

        INSERT INTO wiki_article_versions (id, tag_id, content, citations_json, atom_count, version_number, created_at)
        VALUES ('wv-1', 'tag-child', 'old content', '[]', 1, 1, '2026-01-07T00:00:00Z');

        INSERT INTO wiki_proposals (id, tag_id, base_article_id, base_updated_at, content,
                                    citations_json, ops_json, new_atom_count, created_at)
        VALUES ('wp-1', 'tag-child', 'wa-1', '2026-01-07T00:00:00Z', 'proposed content',
                '[]', '[]', 1, '2026-01-08T00:00:00Z');

        INSERT INTO conversations (id, title, created_at, updated_at, is_archived)
        VALUES ('conv-1', 'About Rust', '2026-01-09T00:00:00Z', '2026-01-09T01:00:00Z', 0);

        INSERT INTO conversation_tags (conversation_id, tag_id)
        VALUES ('conv-1', 'tag-child');

        INSERT INTO chat_messages (id, conversation_id, role, content, created_at, message_index)
        VALUES ('msg-1', 'conv-1', 'user', 'what is ownership?', '2026-01-09T00:00:00Z', 0),
               ('msg-2', 'conv-1', 'assistant', 'Ownership is… [1]', '2026-01-09T00:01:00Z', 1);

        INSERT INTO chat_tool_calls (id, message_id, tool_name, tool_input, tool_output, status, created_at, completed_at)
        VALUES ('tc-1', 'msg-2', 'search_atoms', '{"query":"ownership"}', '{"results":1}',
                'complete', '2026-01-09T00:00:30Z', '2026-01-09T00:00:31Z');

        INSERT INTO chat_citations (id, message_id, citation_index, atom_id, chunk_index, excerpt, relevance_score)
        VALUES ('cc-1', 'msg-2', 1, 'atom-1', 0, 'ownership and borrowing', 0.9);

        INSERT INTO feeds (id, url, title, site_url, poll_interval, last_polled_at, last_error, created_at, is_paused)
        VALUES ('feed-1', 'https://example.com/rss', 'Example', 'https://example.com', 3600,
                '2026-01-10T00:00:00Z', NULL, '2026-01-10T00:00:00Z', 0),
               ('feed-2', 'https://conflict.example/rss', 'Conflict', NULL, 3600,
                NULL, NULL, '2026-01-10T00:00:00Z', 0);

        INSERT INTO feed_tags (feed_id, tag_id)
        VALUES ('feed-1', 'tag-root');

        INSERT INTO feed_items (feed_id, guid, atom_id, skipped, skip_reason, seen_at)
        VALUES ('feed-1', 'guid-1', 'atom-1', 0, NULL, '2026-01-10T01:00:00Z'),
               ('feed-1', 'guid-2', 'missing-atom', 0, NULL, '2026-01-10T02:00:00Z'),
               ('feed-2', 'guid-3', NULL, 1, 'duplicate', '2026-01-10T03:00:00Z');

        INSERT INTO reports (id, name, research_prompt, schedule, created_at, updated_at)
        VALUES ('rep-1', 'Daily digest', 'summarize new atoms', '0 7 * * *',
                '2026-01-11T00:00:00Z', '2026-01-11T00:00:00Z');

        INSERT INTO report_findings (finding_atom_id, report_id, run_id, report_name_snapshot, created_at)
        VALUES ('atom-3', 'rep-1', 'run-1', 'Daily digest', '2026-01-12T00:00:00Z');

        INSERT INTO report_finding_citations (finding_atom_id, cited_atom_id, position, excerpt)
        VALUES ('atom-3', 'atom-1', 0, 'ownership and borrowing');
        "#,
    )
    .expect("seed source db");
    path
}

fn options(name: &str) -> MigrationOptions {
    MigrationOptions {
        db_name: name.to_string(),
        dry_run: false,
        pause_feeds: false,
    }
}

async fn run_migration(
    source: &std::path::Path,
    pg: &PostgresStorage,
    opts: MigrationOptions,
) -> Result<MigrationReport, AtomicCoreError> {
    migrate_sqlite_to_postgres(source, pg, opts, |_| {}, || false).await
}

fn copied(report: &MigrationReport, table: &str) -> i64 {
    report
        .tables
        .iter()
        .find(|t| t.table == table)
        .unwrap_or_else(|| panic!("no report entry for {table}"))
        .copied_rows
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("count query failed ({sql}): {e}"))
}

#[tokio::test]
async fn migrate_full_fidelity_roundtrip() {
    let _guard = PG_LOCK.lock().await;
    let Some(pg) = pg_storage().await else {
        eprintln!("skipping: ATOMIC_TEST_DATABASE_URL not set");
        return;
    };
    let dir = TempDir::new().unwrap();
    let source = seed_source_db(&dir);

    let events: Mutex<Vec<MigrationEvent>> = Mutex::new(Vec::new());
    let report = migrate_sqlite_to_postgres(
        &source,
        &pg,
        MigrationOptions {
            db_name: "Migrated KB".to_string(),
            dry_run: false,
            pause_feeds: true,
        },
        |e| events.lock().unwrap().push(e),
        || false,
    )
    .await
    .expect("migration succeeds");

    let db_id = report.db_id.clone().expect("db_id assigned");
    let pool = pg.pool();

    // Per-table copy counts, orphans excluded.
    assert_eq!(copied(&report, "tags"), 2);
    assert_eq!(copied(&report, "atoms"), 3);
    assert_eq!(
        copied(&report, "atom_tags"),
        3,
        "orphan atom_tags row skipped"
    );
    assert_eq!(copied(&report, "atom_chunks"), 2, "orphan chunk skipped");
    assert_eq!(copied(&report, "atom_positions"), 1);
    assert_eq!(copied(&report, "atom_links"), 1);
    assert_eq!(copied(&report, "wiki_articles"), 1);
    assert_eq!(copied(&report, "wiki_citations"), 1);
    assert_eq!(copied(&report, "wiki_links"), 2, "dangling link included");
    assert_eq!(copied(&report, "wiki_article_versions"), 1);
    assert_eq!(copied(&report, "wiki_proposals"), 1);
    assert_eq!(copied(&report, "conversations"), 1);
    assert_eq!(copied(&report, "conversation_tags"), 1);
    assert_eq!(copied(&report, "chat_messages"), 2);
    assert_eq!(copied(&report, "chat_tool_calls"), 1);
    assert_eq!(copied(&report, "chat_citations"), 1);
    assert_eq!(copied(&report, "feeds"), 2);
    assert_eq!(copied(&report, "feed_tags"), 1);
    assert_eq!(copied(&report, "feed_items"), 3);
    assert_eq!(copied(&report, "reports"), 1);
    assert_eq!(copied(&report, "report_findings"), 1);
    assert_eq!(copied(&report, "report_finding_citations"), 1);
    assert!(report.skipped_feed_urls.is_empty());

    // Orphan skips are visible as source vs copied deltas.
    let atom_tags = report
        .tables
        .iter()
        .find(|t| t.table == "atom_tags")
        .unwrap();
    assert_eq!(atom_tags.source_rows, 4);

    // Statuses: embedding/edges forced pending, errors cleared; tagging preserved.
    let statuses: Vec<(String, String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT embedding_status, edges_status, tagging_status, embedding_error, tagging_error
             FROM atoms WHERE db_id = $1 ORDER BY id",
    )
    .bind(&db_id)
    .fetch_all(pool)
    .await
    .unwrap();
    for (embedding, edges, _, embedding_error, _) in &statuses {
        assert_eq!(embedding, "pending");
        assert_eq!(edges, "pending");
        assert!(embedding_error.is_none());
    }
    assert_eq!(statuses[0].2, "complete", "atom-1 tagging preserved");
    assert_eq!(statuses[1].2, "pending", "atom-2 tagging preserved");
    assert_eq!(statuses[1].4.as_deref(), Some("tag boom"));

    // Atom kind survives.
    let finding_kind: String =
        sqlx::query_scalar("SELECT kind FROM atoms WHERE id = 'atom-3' AND db_id = $1")
            .bind(&db_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(finding_kind, "finding");

    // Chunks: text present, embeddings NULL, keyword search live immediately.
    let null_embeddings: i64 = count(
        pool,
        &format!("SELECT COUNT(*) FROM atom_chunks WHERE db_id = '{db_id}' AND embedding IS NULL"),
    )
    .await;
    assert_eq!(null_embeddings, 2);
    let keyword_hits: i64 = count(
        pool,
        &format!(
            "SELECT COUNT(*) FROM atom_chunks \
             WHERE db_id = '{db_id}' AND content_tsv @@ plainto_tsquery('english', 'ownership')"
        ),
    )
    .await;
    assert_eq!(keyword_hits, 1, "keyword search works before re-embedding");

    // Tag hierarchy relinked; counts rebuilt by triggers (not copied).
    let (parent, atom_count, autotag): (Option<String>, i32, bool) = sqlx::query_as(
        "SELECT parent_id, atom_count, is_autotag_target FROM tags WHERE id = 'tag-child' AND db_id = $1",
    )
    .bind(&db_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(parent.as_deref(), Some("tag-root"));
    assert_eq!(atom_count, 2, "trigger-rebuilt from atom_tags");
    assert!(!autotag);
    let root_autotag: bool = sqlx::query_scalar(
        "SELECT is_autotag_target FROM tags WHERE id = 'tag-root' AND db_id = $1",
    )
    .bind(&db_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(root_autotag, "INTEGER 1 lands as BOOLEAN true");

    // Column-mapped drift: wiki target_tag_name → link_text, tool_output → tool_result.
    let (link_text, target_tag): (String, Option<String>) = sqlx::query_as(
        "SELECT link_text, target_tag_id FROM wiki_links WHERE id = 'wl-1' AND db_id = $1",
    )
    .bind(&db_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(link_text, "Topics");
    assert_eq!(target_tag.as_deref(), Some("tag-root"));
    let (dangling_text, dangling_target): (String, Option<String>) = sqlx::query_as(
        "SELECT link_text, target_tag_id FROM wiki_links WHERE id = 'wl-dangling' AND db_id = $1",
    )
    .bind(&db_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(dangling_text, "Deleted Topic");
    assert_eq!(dangling_target, None, "dangling link keeps its NULL target");
    let tool_result: String = sqlx::query_scalar(
        "SELECT tool_result FROM chat_tool_calls WHERE id = 'tc-1' AND db_id = $1",
    )
    .bind(&db_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(tool_result, "{\"results\":1}");

    // Feed items: dangling atom reference nulled, live one kept; feeds paused.
    let dangling: Option<String> =
        sqlx::query_scalar("SELECT atom_id FROM feed_items WHERE guid = 'guid-2' AND db_id = $1")
            .bind(&db_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(dangling.is_none(), "reference to deleted atom nulled out");
    let paused: i64 = count(
        pool,
        &format!("SELECT COUNT(*) FROM feeds WHERE db_id = '{db_id}' AND is_paused = 1"),
    )
    .await;
    assert_eq!(paused, 2, "pause_feeds lands every feed paused");

    // Commit marker exists with the requested name.
    let name: String = sqlx::query_scalar("SELECT name FROM databases WHERE id = $1")
        .bind(&db_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(name, "Migrated KB");

    // Progress events covered every table.
    let events = events.into_inner().unwrap();
    let completed = events
        .iter()
        .filter(|e| matches!(e, MigrationEvent::TableCompleted { .. }))
        .count();
    assert_eq!(completed, report.tables.len());
    assert!(events
        .iter()
        .any(|e| matches!(e, MigrationEvent::Started { .. })));
}

#[tokio::test]
async fn migrate_twice_aborts_on_collision() {
    let _guard = PG_LOCK.lock().await;
    let Some(pg) = pg_storage().await else {
        eprintln!("skipping: ATOMIC_TEST_DATABASE_URL not set");
        return;
    };
    let dir = TempDir::new().unwrap();
    let source = seed_source_db(&dir);

    run_migration(&source, &pg, options("First"))
        .await
        .expect("first migration");
    let err = run_migration(&source, &pg, options("Second"))
        .await
        .expect_err("second migration must abort");
    assert!(
        matches!(err, AtomicCoreError::Conflict(_)),
        "expected Conflict, got: {err}"
    );

    let pool = pg.pool();
    assert_eq!(migrated_database_count(pool).await, 1);
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM atoms").await,
        3,
        "first copy untouched"
    );
}

#[tokio::test]
async fn migrate_purges_partial_copy_on_failure() {
    let _guard = PG_LOCK.lock().await;
    let Some(pg) = pg_storage().await else {
        eprintln!("skipping: ATOMIC_TEST_DATABASE_URL not set");
        return;
    };
    let dir = TempDir::new().unwrap();
    let source = seed_source_db(&dir);
    let pool = pg.pool();

    // A chunk under another database that collides with a source chunk PK.
    // Chunk ids aren't collision-probed (only root entities are), so the
    // migration fails mid-copy at atom_chunks — after tags/atoms landed.
    sqlx::raw_sql(
        "INSERT INTO atoms (id, content, created_at, updated_at, db_id)
         VALUES ('other-atom', 'x', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'other-db');
         INSERT INTO atom_chunks (id, atom_id, chunk_index, content, db_id)
         VALUES ('chunk-1', 'other-atom', 0, 'x', 'other-db');",
    )
    .execute(pool)
    .await
    .unwrap();

    let err = run_migration(&source, &pg, options("Doomed"))
        .await
        .expect_err("chunk PK collision must fail the migration");
    assert!(
        err.to_string().contains("atom_chunks"),
        "error should name the failing table: {err}"
    );

    // Partial copy fully purged; the pre-existing database untouched;
    // no commit marker.
    assert_eq!(count(pool, "SELECT COUNT(*) FROM atoms").await, 1);
    assert_eq!(count(pool, "SELECT COUNT(*) FROM tags").await, 0);
    assert_eq!(count(pool, "SELECT COUNT(*) FROM atom_chunks").await, 1);
    assert_eq!(migrated_database_count(pool).await, 0, "no commit marker");
}

#[tokio::test]
async fn dry_run_reports_without_writing() {
    let _guard = PG_LOCK.lock().await;
    let Some(pg) = pg_storage().await else {
        eprintln!("skipping: ATOMIC_TEST_DATABASE_URL not set");
        return;
    };
    let dir = TempDir::new().unwrap();
    let source = seed_source_db(&dir);

    let report = migrate_sqlite_to_postgres(
        &source,
        &pg,
        MigrationOptions {
            db_name: "Dry".to_string(),
            dry_run: true,
            pause_feeds: false,
        },
        |_| {},
        || false,
    )
    .await
    .expect("dry run succeeds");

    assert!(report.dry_run);
    assert!(report.db_id.is_none());
    let atoms = report.tables.iter().find(|t| t.table == "atoms").unwrap();
    assert_eq!(atoms.source_rows, 3);
    assert_eq!(atoms.copied_rows, 0);

    let pool = pg.pool();
    assert_eq!(count(pool, "SELECT COUNT(*) FROM atoms").await, 0);
    assert_eq!(migrated_database_count(pool).await, 0);
}

#[tokio::test]
async fn migrate_skips_feeds_with_conflicting_urls() {
    let _guard = PG_LOCK.lock().await;
    let Some(pg) = pg_storage().await else {
        eprintln!("skipping: ATOMIC_TEST_DATABASE_URL not set");
        return;
    };
    let dir = TempDir::new().unwrap();
    let source = seed_source_db(&dir);
    let pool = pg.pool();

    // Another database on the instance already subscribes to this URL —
    // feeds.url is unique across the whole instance.
    sqlx::raw_sql(
        "INSERT INTO feeds (id, url, created_at, db_id)
         VALUES ('other-feed', 'https://conflict.example/rss', '2026-01-01T00:00:00Z', 'other-db')",
    )
    .execute(pool)
    .await
    .unwrap();

    let report = run_migration(&source, &pg, options("Feeds"))
        .await
        .expect("migration succeeds");
    let db_id = report.db_id.clone().unwrap();

    assert_eq!(
        report.skipped_feed_urls,
        vec!["https://conflict.example/rss".to_string()]
    );
    assert_eq!(copied(&report, "feeds"), 1);
    assert_eq!(
        copied(&report, "feed_items"),
        2,
        "skipped feed's items dropped"
    );
    let kept: i64 = count(
        pool,
        &format!("SELECT COUNT(*) FROM feeds WHERE db_id = '{db_id}' AND id = 'feed-1'"),
    )
    .await;
    assert_eq!(kept, 1);
    let skipped: i64 = count(
        pool,
        &format!("SELECT COUNT(*) FROM feeds WHERE db_id = '{db_id}' AND id = 'feed-2'"),
    )
    .await;
    assert_eq!(skipped, 0);
}
