//! Database management for atomic-core

use crate::error::AtomicCoreError;
use rusqlite::ffi::sqlite3_auto_extension;
use rusqlite::Connection;
use sqlite_vec::sqlite3_vec_init;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const READ_POOL_SIZE: usize = 4;
const SERVER_READ_POOL_SIZE: usize = 16;

/// Statement cache capacity per connection (default is 16, too small for our query variety)
const STMT_CACHE_CAPACITY: usize = 64;

/// Base PRAGMAs applied to every connection
const BASE_PRAGMAS: &str = "\
    PRAGMA journal_mode=WAL; \
    PRAGMA synchronous=NORMAL; \
    PRAGMA busy_timeout=5000; \
    PRAGMA cache_size=-64000; \
    PRAGMA mmap_size=2147483648; \
    PRAGMA temp_store=MEMORY; \
";

/// A read-only connection handle — either borrowed from the pool or a temporary connection.
pub enum ReadConn<'a> {
    Pooled(std::sync::MutexGuard<'a, Connection>),
    Temp(Connection),
}

impl std::ops::Deref for ReadConn<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            ReadConn::Pooled(guard) => guard,
            ReadConn::Temp(conn) => conn,
        }
    }
}

/// Database handle with connection management
pub struct Database {
    pub conn: Mutex<Connection>,
    /// Pool of read-only connections for query-heavy paths.
    /// Avoids contention with the main write connection.
    read_pool: Vec<Mutex<Connection>>,
    pub db_path: PathBuf,
}

impl Database {
    /// Open an existing database
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        Self::open_internal(path.as_ref(), false)
    }

    /// Open an existing database or create a new one
    pub fn open_or_create(path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        Self::open_internal(path.as_ref(), true)
    }

    fn open_internal(path: &Path, create: bool) -> Result<Self, AtomicCoreError> {
        Self::open_with_pool_size(path, create, READ_POOL_SIZE)
    }

    fn open_with_pool_size(
        path: &Path,
        create: bool,
        pool_size: usize,
    ) -> Result<Self, AtomicCoreError> {
        Self::open_with_pool_size_and_settings_cleanup(path, create, pool_size, false)
    }

    /// Acquire a read-only connection from the pool.
    /// Tries each pooled connection via try_lock; if all are busy, creates a fresh one.
    pub fn read_conn(&self) -> Result<ReadConn<'_>, AtomicCoreError> {
        for slot in &self.read_pool {
            if let Ok(guard) = slot.try_lock() {
                return Ok(ReadConn::Pooled(guard));
            }
        }
        // All pool slots busy — create a temporary connection
        let conn = Connection::open(&self.db_path)?;
        conn.set_prepared_statement_cache_capacity(STMT_CACHE_CAPACITY);
        conn.execute_batch(&format!("{} PRAGMA query_only=ON;", BASE_PRAGMAS))?;
        Ok(ReadConn::Temp(conn))
    }

    /// Create a new connection to the same database.
    /// Registers sqlite-vec so the connection can query vec_chunks.
    pub fn new_connection(&self) -> Result<Connection, AtomicCoreError> {
        // sqlite-vec is registered via sqlite3_auto_extension in open_internal,
        // which applies to all connections opened after that call.
        let conn = Connection::open(&self.db_path)?;
        conn.set_prepared_statement_cache_capacity(STMT_CACHE_CAPACITY);
        conn.execute_batch(BASE_PRAGMAS)?;
        Ok(conn)
    }

    /// Open with a larger read pool sized for server workloads.
    /// Creates the DB and parent directories if they don't exist.
    pub fn open_for_server(path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        Self::open_with_pool_size(path.as_ref(), true, SERVER_READ_POOL_SIZE)
    }

    /// Open a registry-backed data database with a larger read pool.
    ///
    /// This is the same as `open_for_server`, but runs the one migration step
    /// that is only valid when this SQLite file is a per-database store whose
    /// defaults live in registry.db.
    pub fn open_for_server_with_registry(path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        Self::open_with_pool_size_and_settings_cleanup(
            path.as_ref(),
            true,
            SERVER_READ_POOL_SIZE,
            true,
        )
    }

    fn open_with_pool_size_and_settings_cleanup(
        path: &Path,
        create: bool,
        pool_size: usize,
        cleanup_legacy_seed_settings: bool,
    ) -> Result<Self, AtomicCoreError> {
        // Register sqlite-vec extension
        unsafe {
            #[allow(clippy::missing_transmute_annotations)]
            sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())));
        }

        if create {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let conn = Connection::open(path)?;
        conn.set_prepared_statement_cache_capacity(STMT_CACHE_CAPACITY);

        conn.execute_batch(&format!(
            "{} PRAGMA journal_size_limit=67108864;",
            BASE_PRAGMAS
        ))?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

        Self::run_migrations_internal(&conn, cleanup_legacy_seed_settings)?;

        conn.execute_batch("PRAGMA optimize=0x10002;")?;
        Self::warm_cache(&conn);

        let db_path = path.to_path_buf();
        let mut read_pool = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            let rc = Connection::open(&db_path)?;
            rc.set_prepared_statement_cache_capacity(STMT_CACHE_CAPACITY);
            rc.execute_batch(&format!("{} PRAGMA query_only=ON;", BASE_PRAGMAS))?;
            read_pool.push(Mutex::new(rc));
        }

        Ok(Database {
            conn: Mutex::new(conn),
            read_pool,
            db_path,
        })
    }

    /// Walk the hot indexes and table pages into the OS + SQLite page caches.
    /// Called once at startup so the first real queries don't pay cold-cache costs.
    fn warm_cache(conn: &Connection) {
        let _ = conn.execute_batch(
            "SELECT COUNT(*) FROM atoms;
             SELECT COUNT(*) FROM atom_tags;
             SELECT COUNT(*) FROM tags;
             SELECT 1 FROM atoms ORDER BY updated_at DESC, id DESC LIMIT 1;
             SELECT tag_id, COUNT(*) FROM atom_tags GROUP BY tag_id LIMIT 1;
             SELECT id, parent_id, atom_count FROM tags WHERE parent_id IS NOT NULL LIMIT 1;",
        );

        // Warm the vec_chunks vector index by running a dummy similarity search.
        // This forces sqlite-vec to scan the full vector data into the OS page cache.
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT embedding FROM atom_chunks WHERE embedding IS NOT NULL LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();
        if let Some(query_blob) = blob {
            let _ = conn.query_row(
                "SELECT chunk_id FROM vec_chunks WHERE embedding MATCH ?1 ORDER BY distance LIMIT 1",
                rusqlite::params![&query_blob],
                |row| row.get::<_, String>(0),
            );
        }
    }

    /// Run PRAGMA optimize to update query planner statistics.
    /// Call this on graceful shutdown for best effect.
    pub fn optimize(&self) {
        if let Ok(conn) = self.conn.lock() {
            // 0x10002 = analyze tables that haven't been analyzed + merge FTS
            let _ = conn.execute_batch("PRAGMA optimize=0x10002;");
        }
    }

    /// Schema version tracked via PRAGMA user_version.
    /// Each migration runs exactly once; new databases get all of them sequentially.
    ///
    /// To add a migration:
    ///   1. Add a new `if version < N` block at the end (before the virtual-table section)
    ///   2. End the block with `PRAGMA user_version = N;`
    ///   3. Bump LATEST_VERSION
    const LATEST_VERSION: i32 = 19;

    pub fn run_migrations(conn: &Connection) -> Result<(), AtomicCoreError> {
        Self::run_migrations_internal(conn, false)
    }

    pub fn run_migrations_for_registry(conn: &Connection) -> Result<(), AtomicCoreError> {
        Self::run_migrations_internal(conn, true)
    }

    fn run_migrations_internal(
        conn: &Connection,
        cleanup_legacy_seed_settings: bool,
    ) -> Result<(), AtomicCoreError> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        // --- V0 → V1: Baseline schema (tables + indexes) ---
        if version < 1 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS atoms (
                    id TEXT PRIMARY KEY,
                    content TEXT NOT NULL,
                    source_url TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    embedding_status TEXT DEFAULT 'pending',
                    tagging_status TEXT DEFAULT 'pending'
                );

                CREATE TABLE IF NOT EXISTS tags (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL COLLATE NOCASE,
                    parent_id TEXT REFERENCES tags(id) ON DELETE SET NULL,
                    created_at TEXT NOT NULL,
                    UNIQUE(name COLLATE NOCASE)
                );

                CREATE TABLE IF NOT EXISTS atom_tags (
                    atom_id TEXT REFERENCES atoms(id) ON DELETE CASCADE,
                    tag_id TEXT REFERENCES tags(id) ON DELETE CASCADE,
                    PRIMARY KEY (atom_id, tag_id)
                );

                CREATE TABLE IF NOT EXISTS atom_chunks (
                    id TEXT PRIMARY KEY,
                    atom_id TEXT REFERENCES atoms(id) ON DELETE CASCADE,
                    chunk_index INTEGER NOT NULL,
                    content TEXT NOT NULL,
                    embedding BLOB
                );

                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS wiki_articles (
                    id TEXT PRIMARY KEY,
                    tag_id TEXT UNIQUE REFERENCES tags(id) ON DELETE CASCADE,
                    content TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    atom_count INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS wiki_citations (
                    id TEXT PRIMARY KEY,
                    wiki_article_id TEXT REFERENCES wiki_articles(id) ON DELETE CASCADE,
                    citation_index INTEGER NOT NULL,
                    atom_id TEXT REFERENCES atoms(id) ON DELETE CASCADE,
                    chunk_index INTEGER,
                    excerpt TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS atom_positions (
                    atom_id TEXT PRIMARY KEY REFERENCES atoms(id) ON DELETE CASCADE,
                    x REAL NOT NULL,
                    y REAL NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS semantic_edges (
                    id TEXT PRIMARY KEY,
                    source_atom_id TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE,
                    target_atom_id TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE,
                    similarity_score REAL NOT NULL,
                    source_chunk_index INTEGER,
                    target_chunk_index INTEGER,
                    created_at TEXT NOT NULL,
                    UNIQUE(source_atom_id, target_atom_id)
                );

                CREATE TABLE IF NOT EXISTS atom_clusters (
                    atom_id TEXT PRIMARY KEY REFERENCES atoms(id) ON DELETE CASCADE,
                    cluster_id INTEGER NOT NULL,
                    computed_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS conversations (
                    id TEXT PRIMARY KEY,
                    title TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    is_archived INTEGER DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS conversation_tags (
                    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                    tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                    PRIMARY KEY (conversation_id, tag_id)
                );

                CREATE TABLE IF NOT EXISTS chat_messages (
                    id TEXT PRIMARY KEY,
                    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    message_index INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS chat_tool_calls (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL REFERENCES chat_messages(id) ON DELETE CASCADE,
                    tool_name TEXT NOT NULL,
                    tool_input TEXT NOT NULL,
                    tool_output TEXT,
                    status TEXT NOT NULL DEFAULT 'pending',
                    created_at TEXT NOT NULL,
                    completed_at TEXT
                );

                CREATE TABLE IF NOT EXISTS chat_citations (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL REFERENCES chat_messages(id) ON DELETE CASCADE,
                    citation_index INTEGER NOT NULL,
                    atom_id TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE,
                    chunk_index INTEGER,
                    excerpt TEXT NOT NULL,
                    relevance_score REAL
                );

                CREATE TABLE IF NOT EXISTS api_tokens (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    token_hash TEXT NOT NULL,
                    token_prefix TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    last_used_at TEXT,
                    is_revoked INTEGER DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS oauth_clients (
                    id TEXT PRIMARY KEY,
                    client_id TEXT UNIQUE NOT NULL,
                    client_secret_hash TEXT NOT NULL,
                    client_name TEXT NOT NULL,
                    redirect_uris TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS oauth_codes (
                    code_hash TEXT PRIMARY KEY,
                    client_id TEXT NOT NULL,
                    code_challenge TEXT NOT NULL,
                    code_challenge_method TEXT NOT NULL DEFAULT 'S256',
                    redirect_uri TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    used INTEGER NOT NULL DEFAULT 0,
                    token_id TEXT
                );

                CREATE TABLE IF NOT EXISTS wiki_links (
                    id TEXT PRIMARY KEY,
                    source_article_id TEXT NOT NULL REFERENCES wiki_articles(id) ON DELETE CASCADE,
                    target_tag_name TEXT NOT NULL COLLATE NOCASE,
                    target_tag_id TEXT REFERENCES tags(id) ON DELETE SET NULL,
                    created_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS tag_embeddings (
                    tag_id TEXT PRIMARY KEY REFERENCES tags(id) ON DELETE CASCADE,
                    embedding BLOB NOT NULL,
                    atom_count INTEGER NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_atoms_updated_id ON atoms(updated_at DESC, id DESC);
                CREATE INDEX IF NOT EXISTS idx_atoms_source_url ON atoms(source_url) WHERE source_url IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_atoms_embedding_status ON atoms(embedding_status);
                CREATE INDEX IF NOT EXISTS idx_atoms_tagging_status ON atoms(tagging_status);
                CREATE INDEX IF NOT EXISTS idx_atom_tags_tag_atom ON atom_tags(tag_id, atom_id);
                CREATE INDEX IF NOT EXISTS idx_atom_chunks_atom_id ON atom_chunks(atom_id);
                CREATE INDEX IF NOT EXISTS idx_semantic_edges_source ON semantic_edges(source_atom_id);
                CREATE INDEX IF NOT EXISTS idx_semantic_edges_target ON semantic_edges(target_atom_id);
                CREATE INDEX IF NOT EXISTS idx_semantic_edges_similarity ON semantic_edges(similarity_score DESC);
                CREATE INDEX IF NOT EXISTS idx_tags_parent_id ON tags(parent_id);
                CREATE INDEX IF NOT EXISTS idx_wiki_citations_article ON wiki_citations(wiki_article_id);
                CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations(updated_at DESC);
                CREATE INDEX IF NOT EXISTS idx_conversation_tags_conv ON conversation_tags(conversation_id);
                CREATE INDEX IF NOT EXISTS idx_conversation_tags_tag ON conversation_tags(tag_id);
                CREATE INDEX IF NOT EXISTS idx_chat_messages_conversation ON chat_messages(conversation_id, message_index);
                CREATE INDEX IF NOT EXISTS idx_chat_tool_calls_message ON chat_tool_calls(message_id);
                CREATE INDEX IF NOT EXISTS idx_chat_citations_message ON chat_citations(message_id);
                CREATE INDEX IF NOT EXISTS idx_chat_citations_atom ON chat_citations(atom_id);
                CREATE INDEX IF NOT EXISTS idx_api_tokens_hash ON api_tokens(token_hash);
                CREATE INDEX IF NOT EXISTS idx_wiki_links_source ON wiki_links(source_article_id);
                CREATE INDEX IF NOT EXISTS idx_wiki_links_target_tag ON wiki_links(target_tag_id);

                DROP INDEX IF EXISTS idx_atoms_updated_at;
                DROP INDEX IF EXISTS idx_atom_tags_atom_id;

                PRAGMA user_version = 1;
                "#,
            )?;
        }

        // --- V1 → V2: Denormalize atom_count onto tags ---
        if version < 2 {
            let has_col: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('tags') WHERE name='atom_count'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);

            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE tags ADD COLUMN atom_count INTEGER NOT NULL DEFAULT 0;",
                )?;
            }

            conn.execute_batch(
                "UPDATE tags SET atom_count = (
                     SELECT COUNT(*) FROM atom_tags WHERE tag_id = tags.id
                 );
                 CREATE INDEX IF NOT EXISTS idx_tags_parent_count ON tags(parent_id, atom_count DESC);
                 PRAGMA user_version = 2;",
            )?;
        }

        // --- V2 → V3: Add title and snippet columns to atoms ---
        if version < 3 {
            conn.execute_batch(
                "ALTER TABLE atoms ADD COLUMN title TEXT NOT NULL DEFAULT '';
                 ALTER TABLE atoms ADD COLUMN snippet TEXT NOT NULL DEFAULT '';",
            )?;

            // Backfill title and snippet from existing content
            {
                let mut read_stmt = conn.prepare("SELECT id, content FROM atoms")?;
                let atoms: Vec<(String, String)> = read_stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;

                let mut update_stmt =
                    conn.prepare("UPDATE atoms SET title = ?1, snippet = ?2 WHERE id = ?3")?;
                for (id, content) in &atoms {
                    let (title, snippet) = crate::extract_title_and_snippet(content, 300);
                    update_stmt.execute(rusqlite::params![title, snippet, id])?;
                }
            }

            conn.execute_batch("PRAGMA user_version = 3;")?;
        }

        // --- V3 → V4: Add published_at column to atoms ---
        if version < 4 {
            conn.execute_batch(
                "ALTER TABLE atoms ADD COLUMN published_at TEXT;
                 PRAGMA user_version = 4;",
            )?;
        }

        // --- V4 → V5: Add source column (parsed from source_url) ---
        if version < 5 {
            conn.execute_batch(
                "ALTER TABLE atoms ADD COLUMN source TEXT;
                 CREATE INDEX IF NOT EXISTS idx_atoms_source ON atoms(source) WHERE source IS NOT NULL;
                 CREATE INDEX IF NOT EXISTS idx_atoms_created_id ON atoms(created_at DESC, id DESC);",
            )?;

            // Backfill: extract domain from existing source_url values.
            // For http(s) URLs: strip scheme + 'www.' prefix to get hostname.
            // For other schemes (kindle://, obsidian://): use scheme name.
            // This is a best-effort SQL approximation; new atoms use the Rust parser.
            conn.execute_batch(
                "UPDATE atoms SET source =
                    CASE
                        -- http(s) URLs: extract hostname, strip www.
                        WHEN source_url LIKE 'http://%' OR source_url LIKE 'https://%' THEN
                            REPLACE(
                                SUBSTR(
                                    REPLACE(REPLACE(source_url, 'https://', ''), 'http://', ''),
                                    1,
                                    CASE
                                        WHEN INSTR(REPLACE(REPLACE(source_url, 'https://', ''), 'http://', ''), '/') > 0
                                        THEN INSTR(REPLACE(REPLACE(source_url, 'https://', ''), 'http://', ''), '/') - 1
                                        ELSE LENGTH(REPLACE(REPLACE(source_url, 'https://', ''), 'http://', ''))
                                    END
                                ),
                                'www.', ''
                            )
                        -- Other scheme:// URIs: use scheme as source
                        WHEN INSTR(source_url, '://') > 0 THEN
                            SUBSTR(source_url, 1, INSTR(source_url, '://') - 1)
                        ELSE source_url
                    END
                 WHERE source_url IS NOT NULL AND source IS NULL;",
            )?;

            conn.execute_batch("PRAGMA user_version = 5;")?;
        }

        // --- V5 → V6: Feed management tables ---
        if version < 6 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS feeds (
                    id TEXT PRIMARY KEY,
                    url TEXT NOT NULL UNIQUE,
                    title TEXT,
                    site_url TEXT,
                    poll_interval INTEGER NOT NULL DEFAULT 60,
                    last_polled_at TEXT,
                    last_error TEXT,
                    created_at TEXT NOT NULL,
                    is_paused INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS feed_tags (
                    feed_id TEXT NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                    tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                    PRIMARY KEY (feed_id, tag_id)
                );

                CREATE TABLE IF NOT EXISTS feed_items (
                    feed_id TEXT NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
                    guid TEXT NOT NULL,
                    atom_id TEXT REFERENCES atoms(id) ON DELETE SET NULL,
                    skipped INTEGER NOT NULL DEFAULT 0,
                    skip_reason TEXT,
                    seen_at TEXT NOT NULL,
                    PRIMARY KEY (feed_id, guid)
                );

                CREATE INDEX IF NOT EXISTS idx_feeds_last_polled ON feeds(is_paused, last_polled_at);
                CREATE INDEX IF NOT EXISTS idx_feed_items_feed ON feed_items(feed_id);

                PRAGMA user_version = 6;
                "#,
            )?;
        }

        // --- V6 → V7: Wiki article version history ---
        if version < 7 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS wiki_article_versions (
                    id TEXT PRIMARY KEY,
                    tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                    content TEXT NOT NULL,
                    citations_json TEXT NOT NULL,
                    atom_count INTEGER NOT NULL,
                    version_number INTEGER NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_wiki_versions_tag ON wiki_article_versions(tag_id, version_number);

                PRAGMA user_version = 7;
                "#,
            )?;
        }

        // --- V7 → V8: Store error reasons for failed embeddings/tagging ---
        if version < 8 {
            conn.execute_batch(
                "ALTER TABLE atoms ADD COLUMN embedding_error TEXT;
                 ALTER TABLE atoms ADD COLUMN tagging_error TEXT;
                 PRAGMA user_version = 8;",
            )?;
        }

        // --- V8 → V9: Wiki proposals (human-in-the-loop update review) ---
        if version < 9 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS wiki_proposals (
                    id              TEXT PRIMARY KEY,
                    tag_id          TEXT UNIQUE NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                    base_article_id TEXT NOT NULL,
                    base_updated_at TEXT NOT NULL,
                    content         TEXT NOT NULL,
                    citations_json  TEXT NOT NULL,
                    ops_json        TEXT NOT NULL,
                    new_atom_count  INTEGER NOT NULL,
                    created_at      TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_wiki_proposals_tag_id ON wiki_proposals(tag_id);

                PRAGMA user_version = 9;
                "#,
            )?;
        }

        // --- V9 → V10: Track semantic edge computation status per atom ---
        if version < 10 {
            conn.execute_batch(
                r#"
                ALTER TABLE atoms ADD COLUMN edges_status TEXT DEFAULT 'pending';
                CREATE INDEX IF NOT EXISTS idx_atoms_edges_status ON atoms(edges_status);
                "#,
            )?;
            // Atoms that already have embeddings need edges computed
            // (they may already have edges from before, but we treat them as pending
            // so the batched pipeline can process them cleanly)
            conn.execute(
                "UPDATE atoms SET edges_status = 'pending' WHERE embedding_status = 'complete'",
                [],
            )?;
            // Atoms without embeddings don't need edges yet
            conn.execute(
                "UPDATE atoms SET edges_status = 'none' WHERE embedding_status != 'complete'",
                [],
            )?;
            conn.execute_batch("PRAGMA user_version = 10;")?;
        }

        // --- V10 → V11: Mark which top-level tags the auto-tagger may extend ---
        if version < 11 {
            let has_col: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('tags') WHERE name='is_autotag_target'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);

            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE tags ADD COLUMN is_autotag_target INTEGER NOT NULL DEFAULT 0;",
                )?;
            }

            // Backfill: the five seeded categories are auto-tag targets by default.
            conn.execute_batch(
                "UPDATE tags SET is_autotag_target = 1
                   WHERE parent_id IS NULL
                     AND name IN ('Topics', 'People', 'Locations', 'Organizations', 'Events');
                 PRAGMA user_version = 11;",
            )?;
        }

        // --- V11 → V12: Daily briefings + briefing citations ---
        if version < 12 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS briefings (
                    id TEXT PRIMARY KEY,
                    content TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    atom_count INTEGER NOT NULL,
                    last_run_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS briefing_citations (
                    id TEXT PRIMARY KEY,
                    briefing_id TEXT NOT NULL REFERENCES briefings(id) ON DELETE CASCADE,
                    citation_index INTEGER NOT NULL,
                    atom_id TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE,
                    excerpt TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_briefings_created ON briefings(created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_briefing_citations_briefing
                    ON briefing_citations(briefing_id);

                PRAGMA user_version = 12;
                "#,
            )?;
        }

        // --- V12 → V13: Materialized markdown atom links ---
        if version < 13 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS atom_links (
                    id TEXT PRIMARY KEY,
                    source_atom_id TEXT NOT NULL,
                    target_atom_id TEXT,
                    raw_target TEXT NOT NULL,
                    label TEXT,
                    target_kind TEXT NOT NULL,
                    status TEXT NOT NULL,
                    start_offset INTEGER,
                    end_offset INTEGER,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_atom_links_source
                    ON atom_links(source_atom_id, start_offset);
                CREATE INDEX IF NOT EXISTS idx_atom_links_target
                    ON atom_links(target_atom_id)
                    WHERE target_atom_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS idx_atom_links_status
                    ON atom_links(status);
                "#,
            )?;

            conn.execute_batch("PRAGMA user_version = 13;")?;
        }

        // --- V13 → V14: Durable atom pipeline queue ---
        if version < 14 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS atom_pipeline_jobs (
                    atom_id TEXT PRIMARY KEY REFERENCES atoms(id) ON DELETE CASCADE,
                    embed_requested INTEGER NOT NULL DEFAULT 0,
                    tag_requested INTEGER NOT NULL DEFAULT 0,
                    reason TEXT NOT NULL,
                    not_before TEXT NOT NULL,
                    state TEXT NOT NULL DEFAULT 'pending',
                    lease_until TEXT,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    atom_updated_at TEXT NOT NULL,
                    last_error TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_atom_pipeline_jobs_claim
                    ON atom_pipeline_jobs(state, not_before, updated_at);
                CREATE INDEX IF NOT EXISTS idx_atom_pipeline_jobs_lease
                    ON atom_pipeline_jobs(state, lease_until);

                PRAGMA user_version = 14;
                "#,
            )?;
        }

        // --- V14 → V15: Finish settings resolver migration ---
        //
        // Pre-this-version, every per-DB settings table was seeded with the
        // entire `DEFAULT_SETTINGS` map at open time. The override-aware
        // resolver in `AtomicCore::get_settings_with_source` treats any
        // per-DB row as an override on top of the registry default. As long
        // as those legacy rows match the registry value they look harmless
        // — but the moment the user edits a setting at N=1 (which writes
        // only to the registry), the registry diverges from the stale
        // per-DB row and the row starts shadowing the new value, making
        // edits appear to do nothing. Wipe the seeds so future resolver
        // reads fall through to the registry cleanly.
        //
        // This cleanup is only valid for registry-backed data DBs. Standalone
        // SQLite `AtomicCore::open_or_create` DBs store their canonical
        // settings in this table, so plain Database opens preserve rows and
        // only advance the schema version.
        //
        // In registry-backed deployments before this version, settings writes
        // routed to the registry, so a per-DB row for one of these keys was
        // definitionally a seed (never user-set). Per CLAUDE.md pre-alpha
        // policy we accept erasing any in-flight per-DB overrides written by
        // builds of this branch before the resolver landed.
        if version < 15 {
            if cleanup_legacy_seed_settings {
                let keys: Vec<&str> = crate::settings::DEFAULT_SETTINGS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect();
                if !keys.is_empty() {
                    let placeholders = std::iter::repeat("?")
                        .take(keys.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!("DELETE FROM settings WHERE key IN ({})", placeholders);
                    conn.execute(&sql, rusqlite::params_from_iter(keys.iter()))?;
                }
            }
            conn.execute_batch("PRAGMA user_version = 15;")?;
        }

        // --- V15 → V16: Per-target auto-tag guidance ---
        if version < 16 {
            let has_col: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('tags') WHERE name='autotag_description'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);

            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE tags ADD COLUMN autotag_description TEXT NOT NULL DEFAULT '';",
                )?;
            }

            conn.execute_batch("PRAGMA user_version = 16;")?;
        }

        // --- V16 → V17: Knowledge health tables ---
        if version < 17 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS health_reports (
                    id TEXT PRIMARY KEY,
                    computed_at TEXT NOT NULL,
                    overall_score INTEGER NOT NULL,
                    check_scores TEXT NOT NULL,
                    atom_count INTEGER NOT NULL,
                    auto_fixes_applied INTEGER NOT NULL DEFAULT 0,
                    report_json TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_health_reports_computed
                    ON health_reports(computed_at DESC);

                CREATE TABLE IF NOT EXISTS health_fix_log (
                    id TEXT PRIMARY KEY,
                    check_name TEXT NOT NULL,
                    action TEXT NOT NULL,
                    tier TEXT NOT NULL,
                    atom_ids TEXT,
                    tag_ids TEXT,
                    before_state TEXT NOT NULL DEFAULT '{}',
                    after_state TEXT NOT NULL DEFAULT '{}',
                    llm_prompt TEXT,
                    llm_response TEXT,
                    executed_at TEXT NOT NULL,
                    undone_at TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_health_fix_log_executed
                    ON health_fix_log(executed_at DESC);
                CREATE INDEX IF NOT EXISTS idx_health_fix_log_check
                    ON health_fix_log(check_name);

                PRAGMA user_version = 17;
                "#,
            )?;
        }

        // --- V16 → V17: content_hash column on atom_chunks for boilerplate detection ---
        if version < 17 {
            // ALTER TABLE ADD COLUMN has no IF NOT EXISTS in SQLite; ignore the error
            // if the column was already added (e.g. during a test migration re-run).
            let _ = conn.execute(
                "ALTER TABLE atom_chunks ADD COLUMN content_hash TEXT",
                [],
            );
            conn.execute_batch(
                r#"
                CREATE INDEX IF NOT EXISTS idx_atom_chunks_content_hash
                    ON atom_chunks(content_hash);
                PRAGMA user_version = 17;
                "#,
            )?;
        }

        // --- V17 → V18: persistent dismissals for the review queue ---
        if version < 18 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS health_dismissals (
                    id TEXT PRIMARY KEY,
                    check_name TEXT NOT NULL,
                    item_key TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    dismissed_at TEXT NOT NULL,
                    expires_at TEXT
                );
                CREATE UNIQUE INDEX IF NOT EXISTS idx_health_dismissals_lookup
                    ON health_dismissals(check_name, item_key);
                PRAGMA user_version = 18;
                "#,
            )?;
        }

        // --- V18 → V19: tag_proposals table ---
        if version < 19 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS tag_proposals (
                    id TEXT PRIMARY KEY,
                    summary TEXT NOT NULL,
                    actions_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    applied_at TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_tag_proposals_created
                    ON tag_proposals(created_at DESC);
                PRAGMA user_version = 19;
                "#,
            )?;
        }

        // --- V19 → V20: atoms.is_locked flag ---
        //
        // Locked atoms are protected from automated mutation by health fixes
        // (strip-boilerplate, auto-merge-duplicate, auto-resolve-contradiction,
        // relink-broken-link). They remain readable and editable through the
        // normal UI. Use for source-of-truth material (books, studies, primary
        // research) where automated "correction" would do more harm than good.
        if version < 20 {
            // ALTER TABLE ADD COLUMN has no IF NOT EXISTS in SQLite. Ignore the
            // "duplicate column" error so migration stays idempotent when a
            // test resets user_version to a pre-V20 value on a DB whose table
            // was already migrated by the initial open.
            let _ = conn.execute(
                "ALTER TABLE atoms ADD COLUMN is_locked INTEGER NOT NULL DEFAULT 0",
                [],
            );
            conn.execute_batch("PRAGMA user_version = 20;")?;
        }

        // --- Triggers (recreated every startup to stay current) ---
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS atom_tags_insert_count;
             DROP TRIGGER IF EXISTS atom_tags_delete_count;

             CREATE TRIGGER atom_tags_insert_count
             AFTER INSERT ON atom_tags
             BEGIN
                 UPDATE tags SET atom_count = atom_count + 1 WHERE id = NEW.tag_id;
             END;

             CREATE TRIGGER atom_tags_delete_count
             AFTER DELETE ON atom_tags
             BEGIN
                 UPDATE tags SET atom_count = atom_count - 1 WHERE id = OLD.tag_id;
             END;",
        )?;

        // --- Virtual tables (idempotent checks, recreated if wrong) ---

        let has_vec_chunks: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_chunks'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_vec_chunks {
            conn.execute(
                "CREATE VIRTUAL TABLE vec_chunks USING vec0(chunk_id TEXT PRIMARY KEY, embedding float[1536])",
                [],
            )?;
        }

        // vec_tags must match vec_chunks dimension
        let vec_chunks_dim: usize = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='vec_chunks'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|sql| {
                let start = sql.find("float[")?;
                let after = &sql[start + 6..];
                let end = after.find(']')?;
                after[..end].parse::<usize>().ok()
            })
            .unwrap_or(1536);

        let vec_tags_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='vec_tags'",
                [],
                |row| row.get(0),
            )
            .unwrap_or_default();

        if vec_tags_sql.is_empty() || !vec_tags_sql.contains(&format!("float[{}]", vec_chunks_dim))
        {
            conn.execute("DROP TABLE IF EXISTS vec_tags", []).ok();
            conn.execute("DELETE FROM tag_embeddings", []).ok();
            conn.execute(
                &format!(
                    "CREATE VIRTUAL TABLE vec_tags USING vec0(tag_id TEXT PRIMARY KEY, embedding float[{}])",
                    vec_chunks_dim
                ),
                [],
            )?;
        }

        // FTS5 for keyword search (external content backed by atom_chunks)
        let fts_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='atom_chunks_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap_or_default();

        let has_correct_fts = fts_sql.contains("content='atom_chunks'")
            && fts_sql.contains("atom_id")
            && fts_sql.contains("chunk_index");

        if !has_correct_fts {
            conn.execute_batch("DROP TABLE IF EXISTS atom_chunks_fts")?;
            conn.execute_batch(
                r#"
                CREATE VIRTUAL TABLE atom_chunks_fts USING fts5(
                    id,
                    atom_id,
                    chunk_index,
                    content,
                    content='atom_chunks',
                    content_rowid='rowid'
                );
                "#,
            )?;
            conn.execute(
                "INSERT INTO atom_chunks_fts(atom_chunks_fts) VALUES('rebuild')",
                [],
            )?;
        }

        // FTS5 for atom-level keyword search (powers the search palette).
        // External content backed by the atoms table; kept in sync manually
        // from `storage::sqlite::atoms` on insert/update/delete.
        let atoms_fts_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='atoms_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap_or_default();

        let has_atoms_fts = atoms_fts_sql.contains("content='atoms'");

        if !has_atoms_fts {
            conn.execute_batch("DROP TABLE IF EXISTS atoms_fts")?;
            conn.execute_batch(
                r#"
                CREATE VIRTUAL TABLE atoms_fts USING fts5(
                    id UNINDEXED,
                    content,
                    content='atoms',
                    content_rowid='rowid'
                );
                "#,
            )?;
            conn.execute("INSERT INTO atoms_fts(atoms_fts) VALUES('rebuild')", [])?;
        }

        let wiki_fts_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='wiki_articles_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap_or_default();

        if wiki_fts_sql.is_empty() {
            conn.execute_batch(
                r#"
                CREATE VIRTUAL TABLE wiki_articles_fts USING fts5(
                    id UNINDEXED,
                    tag_id UNINDEXED,
                    tag_name,
                    content
                );
                "#,
            )?;
            conn.execute("DELETE FROM wiki_articles_fts", [])?;
            conn.execute(
                "INSERT INTO wiki_articles_fts(id, tag_id, tag_name, content)
                 SELECT w.id, w.tag_id, t.name, w.content
                 FROM wiki_articles w
                 JOIN tags t ON t.id = w.tag_id",
                [],
            )?;
        }

        let chat_fts_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='chat_messages_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap_or_default();

        if chat_fts_sql.is_empty() {
            conn.execute_batch(
                r#"
                CREATE VIRTUAL TABLE chat_messages_fts USING fts5(
                    id UNINDEXED,
                    conversation_id UNINDEXED,
                    content
                );
                "#,
            )?;
            conn.execute("DELETE FROM chat_messages_fts", [])?;
            conn.execute(
                "INSERT INTO chat_messages_fts(id, conversation_id, content)
                 SELECT id, conversation_id, content FROM chat_messages",
                [],
            )?;
        }

        // Per-DB settings tables intentionally start empty under the
        // registry-defaults + per-DB-overrides model. Defaults live in
        // `registry.db` (seeded by `Registry::open` via `migrate_settings_to`);
        // per-DB rows only exist when the user explicitly overrides a value.
        // Registry-backed opens run the V14→V15 cleanup above to purge any
        // legacy seed rows so the resolver's "any per-DB row is an override"
        // rule stays correct.

        Ok(())
    }
}

// ==================== Dimension Change Helpers ====================

/// Get embedding dimension based on current settings
pub fn get_current_embedding_dimension(conn: &Connection) -> usize {
    use crate::providers::ProviderConfig;

    let settings_map = crate::settings::get_all_settings(conn).unwrap_or_default();
    let config = ProviderConfig::from_settings(&settings_map);
    config.embedding_dimension()
}

/// Check if dimension will change with new settings
pub fn will_dimension_change(conn: &Connection, key: &str, new_value: &str) -> (bool, usize) {
    use crate::providers::ProviderConfig;

    let current_dim = get_current_embedding_dimension(conn);

    // Get current settings and apply the change
    let mut settings_map = crate::settings::get_all_settings(conn).unwrap_or_default();
    settings_map.insert(key.to_string(), new_value.to_string());

    let new_config = ProviderConfig::from_settings(&settings_map);
    let new_dim = new_config.embedding_dimension();

    (current_dim != new_dim, new_dim)
}

/// Recreate vec_chunks table with a new dimension and reset embedding status.
/// Chunk rows are preserved so embed-only re-embedding can reuse the existing
/// markdown-aware chunk boundaries.
pub fn recreate_vec_chunks_with_dimension(
    conn: &Connection,
    dimension: usize,
) -> Result<(), AtomicCoreError> {
    conn.execute("DROP TABLE IF EXISTS vec_chunks", [])?;

    let create_sql = format!(
        "CREATE VIRTUAL TABLE vec_chunks USING vec0(chunk_id TEXT PRIMARY KEY, embedding float[{}])",
        dimension
    );
    conn.execute(&create_sql, [])?;

    // Reset ONLY embedding status to pending
    conn.execute("UPDATE atoms SET embedding_status = 'pending'", [])?;

    // Set tagging_status to 'skipped' - existing tags are preserved
    conn.execute("UPDATE atoms SET tagging_status = 'skipped'", [])?;

    // Clear old chunk vectors while preserving chunk ids/content.
    conn.execute("UPDATE atom_chunks SET embedding = NULL", [])?;

    // Clear semantic edges
    conn.execute("DELETE FROM semantic_edges", [])?;

    // Clear tag embeddings and recreate vec_tags with new dimension
    conn.execute("DELETE FROM tag_embeddings", []).ok();
    conn.execute("DROP TABLE IF EXISTS vec_tags", [])?;
    let vec_tags_sql = format!(
        "CREATE VIRTUAL TABLE vec_tags USING vec0(tag_id TEXT PRIMARY KEY, embedding float[{}])",
        dimension
    );
    conn.execute(&vec_tags_sql, [])?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_v15_wipes_legacy_per_db_seed_rows() {
        // Pre-V15, every per-DB connection got the entire DEFAULT_SETTINGS
        // map seeded into its `settings` table. The resolver treats per-DB
        // rows as overrides on top of registry defaults — the moment the
        // user edits a setting at N=1 (writes to registry only), the
        // registry diverges from the stale per-DB row and the row starts
        // shadowing the new value, making edits look broken. V15 deletes
        // those rows.
        //
        // Simulate the pre-V15 state by opening a fresh DB (which now runs
        // *all* migrations including V15), forcibly downgrading the
        // user_version, re-inserting seed rows, then re-running the
        // migration.
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();

        {
            let conn = db.conn.lock().unwrap();
            // Force the schema back to V14 and re-seed the legacy rows.
            conn.execute_batch("PRAGMA user_version = 14;").unwrap();
            crate::settings::migrate_settings(&conn).unwrap();
            let pre_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM settings", [], |row| row.get(0))
                .unwrap();
            assert!(
                pre_count > 0,
                "test setup: legacy rows should be present before V15"
            );

            // Run migrations again — V15 should fire and wipe the seed rows.
            Database::run_migrations_for_registry(&conn).unwrap();

            let version: i32 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert!(
                version >= 15,
                "user_version should be ≥15 after migration, got {version}"
            );

            // Every key in DEFAULT_SETTINGS must be gone — they were seeds.
            for (key, _) in crate::settings::DEFAULT_SETTINGS {
                let exists: bool = conn
                    .query_row("SELECT 1 FROM settings WHERE key = ?1", [key], |_| Ok(true))
                    .unwrap_or(false);
                assert!(
                    !exists,
                    "V15 should have removed legacy seed row for '{key}'"
                );
            }
        }
    }

    #[test]
    fn test_v15_preserves_standalone_settings_rows() {
        // Registry-less SQLite cores use the data DB's settings table as the
        // canonical settings store. The V15 seed cleanup is only valid when a
        // registry is attached, so the plain migration path must preserve
        // those rows.
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();

        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("PRAGMA user_version = 14;").unwrap();
            crate::settings::set_setting(&conn, "chat_model", "custom/model").unwrap();

            Database::run_migrations(&conn).unwrap();

            let value = crate::settings::get_setting(&conn, "chat_model").unwrap();
            assert_eq!(value, "custom/model");
        }
    }

    #[test]
    fn test_create_database() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();

        // Verify we got a valid database
        let conn = db.conn.lock().unwrap();
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // Should have at least our core tables (16 regular + 2 virtual)
        assert!(count >= 16, "Expected at least 16 tables, got {}", count);
    }

    #[test]
    fn test_tables_created() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();
        let conn = db.conn.lock().unwrap();

        let expected_tables = vec![
            "atoms",
            "tags",
            "atom_tags",
            "atom_chunks",
            "settings",
            "wiki_articles",
            "wiki_citations",
            "atom_positions",
            "semantic_edges",
            "atom_clusters",
            "conversations",
            "conversation_tags",
            "chat_messages",
            "chat_tool_calls",
            "chat_citations",
            "api_tokens",
        ];

        for table in expected_tables {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(exists, "Table '{}' should exist", table);
        }
    }

    #[test]
    fn test_vec_chunks_virtual_table() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();
        let conn = db.conn.lock().unwrap();

        // Verify vec_chunks virtual table exists
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_chunks'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "vec_chunks virtual table should exist");
    }

    #[test]
    fn test_fts_virtual_table() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();
        let conn = db.conn.lock().unwrap();

        // Verify atom_chunks_fts virtual table exists
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='atom_chunks_fts'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "atom_chunks_fts FTS5 table should exist");
    }

    #[test]
    fn test_new_connection() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open_or_create(temp_file.path()).unwrap();

        // Create a new connection - should work without errors
        let conn2 = db.new_connection().unwrap();

        // Verify we can query the new connection
        let count: i32 = conn2
            .query_row("SELECT COUNT(*) FROM atoms", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "New database should have 0 atoms");
    }
}
