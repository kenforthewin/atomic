//! atomic-core: Knowledge base library for Atomic
//!
//! This library provides the core RAG pipeline for the Atomic knowledge base:
//! - Atom CRUD operations
//! - Embedding generation with callback-based events
//! - Unified search (semantic, keyword, hybrid)
//! - Wiki article synthesis
//! - Tag extraction and compaction
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::{AtomicCore, CreateAtomRequest, EmbeddingEvent};
//!
//! let core = AtomicCore::open_or_create("/path/to/db")?;
//!
//! // Create an atom with embedding callback
//! let atom = core.create_atom(
//!     CreateAtomRequest {
//!         content: "My note content".to_string(),
//!         ..Default::default()
//!     },
//!     |event| match event {
//!         EmbeddingEvent::EmbeddingComplete { atom_id } => println!("Done: {}", atom_id),
//!         _ => {}
//!     },
//! )?;
//! ```

pub mod agent;
pub mod atom_edit;
pub(crate) mod atom_links;
pub mod canvas_level;
pub mod chat;
pub mod chunking;
pub mod clustering;
pub mod compaction;
pub mod db;
pub mod embedding;
pub mod error;
pub mod executor;
pub mod export;
pub mod extraction;
pub mod graph_maintenance;
pub mod import;
pub mod ingest;
pub mod knowledge_signals;
pub mod manager;
pub mod models;
pub mod pipeline_task;
pub mod projection;
pub mod providers;
pub mod registry;
pub mod reports;
pub mod scheduler;
pub mod search;
pub mod settings;
pub mod storage;
pub mod tokens;
pub mod wiki;

// Re-exports for convenience
pub use agent::{CanvasClusterSummary, CanvasContext, ChatEvent, PageContext};
pub use atom_edit::{apply_atom_edits, AtomEditOperation};
pub use db::Database;
pub use embedding::{EmbeddingEvent, EmbeddingStrategy, TaggingStrategy};
pub use error::AtomicCoreError;
pub use export::{MarkdownArchiveFormat, MarkdownExportProgress, MarkdownExportResult};
pub use import::{ImportProgress, ImportResult};
pub use ingest::{FeedPollResult, IngestionEvent, IngestionRequest, IngestionResult};
pub use knowledge_signals::{
    KnowledgeSignal, KnowledgeSignalAction, KnowledgeSignalFilter, KnowledgeSignalProviderConfig,
    KnowledgeSignalReason, KnowledgeSignalSeverity, KnowledgeSignalTarget, WikiCandidateEvidence,
};
pub use manager::DatabaseManager;
pub use models::*;
pub use providers::{ProviderConfig, ProviderType};
pub use registry::{DatabaseInfo, OAuthCodeInfo, Registry};
pub use search::{SearchMode, SearchOptions};
pub use tokens::ApiTokenInfo;

use chrono::Utc;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// Request to create a new atom
#[derive(Debug, Clone, Default)]
pub struct CreateAtomRequest {
    pub content: String,
    pub source_url: Option<String>,
    pub published_at: Option<String>,
    pub tag_ids: Vec<String>,
    /// When true, silently skip creation if an atom with the same source_url already exists.
    pub skip_if_source_exists: bool,
}

/// Request to update an existing atom
#[derive(Debug, Clone)]
pub struct UpdateAtomRequest {
    pub content: String,
    pub source_url: Option<String>,
    pub published_at: Option<String>,
    pub tag_ids: Option<Vec<String>>,
}

/// Rebuilder closure registered by `AtomicCore` so the cache can recompute
/// itself in the background during debounced invalidations. Captures
/// `StorageBackend` only (not `AtomicCore`) to avoid a reference cycle.
type CanvasRebuilder =
    Box<dyn Fn() -> Result<Arc<GlobalCanvasData>, AtomicCoreError> + Send + Sync + 'static>;

/// How long `invalidate_debounced` waits after the last invalidation before
/// kicking off a background rebuild. Sized so that bulk-event storms (e.g.
/// a 100-atom embedding batch) collapse into a single rebuild.
const CANVAS_CACHE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// Per-DB settings key holding the dashboard's featured-report id. The
/// seed helper stamps this on every DB; the dashboard widget reads it; a
/// report deletion clears it when the pointer would otherwise dangle.
const FEATURED_REPORT_SETTING: &str = "dashboard.featured_report_id";

/// In-memory cache for the global canvas payload.
///
/// `compute_and_get_canvas_data` is expensive: it runs PCA on every atom
/// embedding, materializes metadata + edges, and re-clusters from scratch.
/// The cache holds the last computed `GlobalCanvasData` behind an `Arc` so
/// subsequent reads are a lock-free-ish pointer clone.
///
/// Two invalidation flavors:
/// - `invalidate()` — eager clear for direct user mutations. Next read pays
///   full compute cost.
/// - `invalidate_debounced()` — keeps the stale payload visible and spawns
///   a background rebuild after [`CANVAS_CACHE_DEBOUNCE`]. Only the latest
///   request wins (via a generation counter), so bursts of background events
///   coalesce into a single rebuild. Used by the batch embedding and edge
///   pipelines so streaming updates don't thrash the cache.
#[derive(Clone, Default)]
pub struct CanvasCache {
    inner: Arc<CanvasCacheInner>,
}

#[derive(Default)]
struct CanvasCacheInner {
    data: std::sync::RwLock<Option<Arc<GlobalCanvasData>>>,
    rebuild_gen: std::sync::atomic::AtomicU64,
    rebuilder: std::sync::OnceLock<CanvasRebuilder>,
    /// Serializes concurrent cold-cache computes so N simultaneous misses
    /// don't all pay the full PCA + edge-load cost. Paired with a
    /// double-checked read of `data` on either side of the lock acquire.
    ///
    /// Uses `tokio::sync::Mutex` so the guard is Send across `.await`,
    /// letting `compute_and_get_canvas_data` be `Send` when spawned.
    compute_lock: tokio::sync::Mutex<()>,
}

impl CanvasCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached payload if present.
    pub fn get(&self) -> Option<Arc<GlobalCanvasData>> {
        self.inner.data.read().ok().and_then(|g| g.clone())
    }

    /// Store a freshly computed payload.
    pub fn set(&self, data: Arc<GlobalCanvasData>) {
        if let Ok(mut g) = self.inner.data.write() {
            *g = Some(data);
        }
    }

    /// Eager invalidation: drop the cached payload immediately and bump the
    /// rebuild generation so any in-flight debounced rebuild no-ops on
    /// completion. Next read pays full compute cost.
    pub fn invalidate(&self) {
        self.inner
            .rebuild_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut g) = self.inner.data.write() {
            *g = None;
        }
    }

    /// Register the rebuilder closure used by debounced invalidations.
    /// First call wins (backed by `OnceLock`); subsequent calls are no-ops.
    pub(crate) fn set_rebuilder(&self, rebuilder: CanvasRebuilder) {
        let _ = self.inner.rebuilder.set(rebuilder);
    }

    /// Acquire the compute guard used by [`AtomicCore::compute_and_get_canvas_data`]
    /// to serialize cold-cache rebuilds. The caller double-checks the cache
    /// after acquiring so only the first waiter pays the compute cost.
    pub(crate) async fn compute_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.compute_lock.lock().await
    }

    /// Debounced invalidation: schedule a background rebuild after
    /// [`CANVAS_CACHE_DEBOUNCE`], keeping the current (stale) payload visible
    /// to readers in the meantime. Only the latest scheduled rebuild wins —
    /// rapid successive calls collapse into a single rebuild. No-op if no
    /// rebuilder has been registered.
    pub fn invalidate_debounced(&self) {
        use std::sync::atomic::Ordering;
        let my_gen = self.inner.rebuild_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let cache = self.clone();
        crate::executor::spawn(async move {
            tokio::time::sleep(CANVAS_CACHE_DEBOUNCE).await;
            if cache.inner.rebuild_gen.load(Ordering::SeqCst) != my_gen {
                return;
            }
            let cache_compute = cache.clone();
            let result = tokio::task::spawn_blocking(move || {
                cache_compute.inner.rebuilder.get().map(|f| f())
            })
            .await;
            match result {
                Ok(Some(Ok(fresh))) => {
                    if cache.inner.rebuild_gen.load(Ordering::SeqCst) == my_gen {
                        cache.set(fresh);
                    }
                }
                Ok(Some(Err(e))) => {
                    tracing::warn!(error = %e, "Debounced canvas rebuild failed");
                }
                Ok(None) => {
                    tracing::debug!("Debounced canvas rebuild skipped: no rebuilder registered");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Debounced canvas rebuild panicked");
                }
            }
        });
    }
}

/// Main library facade providing high-level operations
#[derive(Clone)]
pub struct AtomicCore {
    /// Storage abstraction layer supporting SQLite and Postgres at runtime.
    /// All DB operations flow through this. For SQLite, the underlying
    /// `Arc<Database>` is accessible via `storage.as_sqlite().db` when
    /// needed by modules not yet fully migrated (search, agent, wiki).
    storage: storage::StorageBackend,
    /// When present, settings and token operations delegate to the shared registry.
    /// When absent (standalone use, tests), uses per-db tables as before.
    registry: Option<Arc<registry::Registry>>,
    /// Per-tag locks to serialize wiki operations (update, propose, accept,
    /// dismiss) against the same article. Prevents background + manual runs
    /// from racing and ensures supersede semantics are consistent. Entries are
    /// created lazily and persist for the lifetime of the process — the
    /// working set is bounded by the number of wiki articles touched.
    wiki_tag_locks:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// In-memory cache for `compute_and_get_canvas_data`. Shared across clones
    /// so every handle sees the same cached payload.
    canvas_cache: CanvasCache,
}

impl AtomicCore {
    /// Open an existing database
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        let db = Arc::new(Database::open(db_path)?);
        let storage = storage::StorageBackend::Sqlite(storage::SqliteStorage::new(db));
        let core = Self {
            storage,
            registry: None,
            wiki_tag_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            canvas_cache: CanvasCache::new(),
        };
        core.register_canvas_rebuilder();
        Ok(core)
    }

    /// Open an existing database with a larger read pool sized for server workloads.
    pub fn open_for_server(db_path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        let db = Database::open_for_server(db_path)?;
        Self::seed_and_backfill(db, None)
    }

    /// Open for server with an optional shared registry for settings/token delegation.
    pub fn open_for_server_with_registry(
        db_path: impl AsRef<Path>,
        registry: Option<Arc<registry::Registry>>,
    ) -> Result<Self, AtomicCoreError> {
        let db = if registry.is_some() {
            Database::open_for_server_with_registry(db_path)?
        } else {
            Database::open_for_server(db_path)?
        };
        Self::seed_and_backfill(db, registry)
    }

    /// Run storage optimization — call on graceful shutdown.
    /// SQLite: PRAGMA optimize. Postgres: no-op.
    pub fn optimize(&self) {
        self.storage.optimize();
    }

    /// Open a Postgres-backed AtomicCore instance.
    ///
    /// Most operations route through the Postgres storage backend. A few operations
    /// (search, wiki generation, chat agent) still require module-level refactoring
    /// and will return `Configuration` errors when used with Postgres.
    #[cfg(feature = "postgres")]
    pub async fn open_postgres(
        database_url: &str,
        db_id: &str,
        registry: Option<Arc<registry::Registry>>,
    ) -> Result<Self, AtomicCoreError> {
        use storage::PostgresStorage;

        let pg_storage = PostgresStorage::connect(database_url, db_id).await?;
        pg_storage.initialize().await?;

        let storage = storage::StorageBackend::Postgres(pg_storage);

        // Seed default category tags if tags table is empty
        let all_tags = storage.get_all_tags_impl().await?;
        if all_tags.is_empty() {
            for category in &["Topics", "People", "Locations", "Organizations", "Events"] {
                storage.create_tag_impl(category, None).await?;
            }
            tracing::info!("Seeded default category tags in Postgres");
        }

        // Seed default settings if no registry and settings table is empty.
        // When a registry exists, settings live there (not in the data DB).
        if registry.is_none() {
            let existing = storage.get_all_settings_sync().await?;
            if existing.is_empty() {
                for (key, value) in settings::DEFAULT_SETTINGS {
                    storage.set_setting_sync(key, value).await?;
                }
                tracing::info!("Seeded default settings in Postgres");
            }
        }

        let core = Self {
            storage,
            registry,
            wiki_tag_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            canvas_cache: CanvasCache::new(),
        };
        core.register_canvas_rebuilder();
        Ok(core)
    }

    /// Create an AtomicCore from an existing PostgresStorage (for multi-db in Postgres mode).
    #[cfg(feature = "postgres")]
    pub fn from_postgres_storage(pg: storage::PostgresStorage) -> Self {
        let core = Self {
            storage: storage::StorageBackend::Postgres(pg),
            registry: None,
            wiki_tag_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            canvas_cache: CanvasCache::new(),
        };
        core.register_canvas_rebuilder();
        core
    }

    /// Open an existing database or create a new one
    pub fn open_or_create(db_path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        let db = Database::open_or_create(db_path)?;
        Self::seed_and_backfill(db, None)
    }

    /// Shared initialization: reconcile vec dimension, backfill centroids.
    ///
    /// Note: default category tags are NOT auto-seeded. The onboarding wizard
    /// (or the user via the settings tab / API) decides which top-level categories
    /// the auto-tagger may extend. Existing databases that were seeded before this
    /// change keep their tags via the V11 migration's backfill.
    fn seed_and_backfill(
        db: Database,
        registry: Option<Arc<registry::Registry>>,
    ) -> Result<Self, AtomicCoreError> {
        // Reconcile vec_chunks dimension with the configured embedding model.
        // Only for empty databases (no atoms yet) — e.g. newly created databases
        // whose migration hardcodes float[1536] but the user's model differs.
        if let Some(ref reg) = registry {
            let conn = db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            let atom_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM atoms", [], |row| row.get(0))
                .unwrap_or(0);

            if atom_count == 0 {
                if let Ok(settings) = reg.get_all_settings() {
                    let config = providers::ProviderConfig::from_settings(&settings);
                    let expected_dim = config.embedding_dimension();

                    let current_dim: usize = conn
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

                    if current_dim != expected_dim {
                        tracing::info!(
                            current_dim,
                            expected_dim,
                            "Reconciling vec_chunks dimension for configured embedding model"
                        );
                        db::recreate_vec_chunks_with_dimension(&conn, expected_dim)?;
                    }
                }
            }
        }

        // Backfill tag centroid embeddings if the table exists but is empty
        // (i.e. an existing DB just got the new schema for the first time)
        {
            let conn = db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            let has_embeddings: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM atom_chunks WHERE embedding IS NOT NULL)",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            let has_centroids: bool = conn
                .query_row("SELECT EXISTS(SELECT 1 FROM tag_embeddings)", [], |row| {
                    row.get(0)
                })
                .unwrap_or(false);

            if has_embeddings && !has_centroids {
                let mut stmt = conn
                    .prepare(
                        "SELECT DISTINCT at.tag_id
                     FROM atom_tags at
                     INNER JOIN atom_chunks ac ON at.atom_id = ac.atom_id
                     WHERE ac.embedding IS NOT NULL",
                    )
                    .map_err(|e| AtomicCoreError::Database(e))?;

                let tag_ids: Vec<String> = stmt
                    .query_map([], |row| row.get(0))
                    .map_err(|e| AtomicCoreError::Database(e))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| AtomicCoreError::Database(e))?;

                if !tag_ids.is_empty() {
                    tracing::info!(count = tag_ids.len(), "Backfilling tag centroid embeddings");
                    embedding::compute_tag_embeddings_batch(&conn, &tag_ids)
                        .map_err(|e| AtomicCoreError::Embedding(e))?;
                    tracing::info!("Tag centroid backfill complete");
                }
            }
        }

        let db = Arc::new(db);
        let storage = storage::StorageBackend::Sqlite(storage::SqliteStorage::new(db));
        let core = Self {
            storage,
            registry,
            wiki_tag_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            canvas_cache: CanvasCache::new(),
        };
        core.register_canvas_rebuilder();
        Ok(core)
    }

    /// Resolved settings map for background AI operations (embedding,
    /// tagging, search, chat, agent). Returns the same merged view as
    /// `get_settings` — registry workspace defaults + this DB's per-DB
    /// overrides + builtin fallbacks — so background helpers see exactly
    /// what the API surfaces. Returning `None` here would make callers fall
    /// back to reading just the storage layer, which would skip registry
    /// defaults; we never want that, so we always return `Some` (or `None`
    /// only on a hard read failure).
    async fn settings_for_background(&self) -> Option<HashMap<String, String>> {
        self.get_settings().await.ok()
    }

    /// Get the storage path (for display purposes).
    pub fn db_path(&self) -> &Path {
        self.storage.storage_path()
    }

    /// Get a reference to the underlying SQLite database (if available).
    /// Returns None for Postgres backend.
    pub fn database(&self) -> Option<Arc<Database>> {
        self.storage.as_sqlite().map(|s| Arc::clone(&s.db))
    }

    /// Get a reference to the underlying storage backend. Used by sibling
    /// modules (briefing, scheduler helpers) that need to issue storage
    /// calls directly without going through the full facade surface.
    pub(crate) fn storage(&self) -> &storage::StorageBackend {
        &self.storage
    }

    // ==================== Settings ====================
    //
    // Resolution model (see `settings::WORKSPACE_ONLY_KEYS` and
    // `settings::SettingSource`):
    //
    //   * Workspace-only keys (theme, font, credentials, machine URLs) live
    //     ONLY in `registry.db`. Reads and writes always target the registry;
    //     per-DB rows are ignored.
    //
    //   * Overridable keys read with the precedence
    //         per-DB row > registry row > DEFAULT_SETTINGS constant.
    //     Writes go to the per-DB table when the workspace has more than one
    //     database (so the user's change is scoped to the active DB and other
    //     DBs keep inheriting). With a single database — the common case —
    //     writes go to the registry instead, so adding a second database
    //     later naturally inherits the user's existing preferences without
    //     any copy-on-promotion gymnastics.
    //
    // Frontends call `get_settings_with_source` to render override
    // affordances; internal Rust callers use `get_settings` for plain values.

    /// Get all settings as a flat key→value map, with per-DB overrides
    /// merged on top of registry defaults. Internal Rust callers (briefing,
    /// agent, embedding pipeline) use this — they don't need source info.
    pub async fn get_settings(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, AtomicCoreError> {
        let resolved = self.get_settings_with_source().await?;
        Ok(resolved.into_iter().map(|(k, v)| (k, v.value)).collect())
    }

    /// Get all settings as a HashMap. Internal helper used by embedding/agent code.
    pub async fn get_settings_map(&self) -> Result<HashMap<String, String>, AtomicCoreError> {
        self.get_settings().await
    }

    /// Get all resolved settings tagged with their source (workspace-only,
    /// workspace default, per-DB override, or builtin default). Used by the
    /// settings API to power inline override UI.
    pub async fn get_settings_with_source(
        &self,
    ) -> Result<HashMap<String, settings::SettingValue>, AtomicCoreError> {
        let mut merged: HashMap<String, settings::SettingValue> = HashMap::new();

        // Layer 1: builtin defaults (lowest priority). Defensive — registry
        // is normally seeded with these via `migrate_settings`, but we
        // include them so a fresh-but-empty registry still resolves.
        for (key, value) in settings::DEFAULT_SETTINGS {
            merged.insert(
                (*key).to_string(),
                settings::SettingValue {
                    value: (*value).to_string(),
                    source: settings::SettingSource::BuiltinDefault,
                },
            );
        }

        // Layer 2: registry — workspace defaults (overridable keys) and the
        // single source of truth for workspace-only keys.
        if let Some(ref reg) = self.registry {
            for (key, value) in reg.get_all_settings()? {
                let source = if settings::is_workspace_only(&key) {
                    settings::SettingSource::Workspace
                } else {
                    settings::SettingSource::WorkspaceDefault
                };
                merged.insert(key, settings::SettingValue { value, source });
            }
        }

        // Layer 3: per-DB. Two cases:
        //   * Registry attached (the SQLite multi-DB shape): per-DB rows are
        //     true overrides on top of the registry. Workspace-only rows in
        //     this layer are legacy data — the registry is the single source
        //     of truth for those keys, so we ignore them here. The V15
        //     migration in `db.rs` wipes seeded-default rows so this layer
        //     only ever contains real overrides.
        //   * No registry (Postgres deployments today): the storage layer's
        //     settings table is the only place anything lives. Treat its
        //     values like a registry layer would — workspace-only → Workspace,
        //     overridable → WorkspaceDefault. Without a registry there's no
        //     "per-DB override" concept to surface.
        let per_db = self.storage.get_all_settings_sync().await?;
        let has_registry = self.registry.is_some();
        for (key, value) in per_db {
            let source = if settings::is_workspace_only(&key) {
                if has_registry {
                    continue;
                }
                settings::SettingSource::Workspace
            } else if has_registry {
                settings::SettingSource::Override
            } else {
                settings::SettingSource::WorkspaceDefault
            };
            merged.insert(key, settings::SettingValue { value, source });
        }

        Ok(merged)
    }

    /// Set a setting value. Routing (see module docs above):
    /// workspace-only → registry; overridable + N≤1 → registry as workspace
    /// default; overridable + N>1 → per-DB as override for the active DB.
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<(), AtomicCoreError> {
        let registry = match &self.registry {
            Some(r) => r,
            None => {
                // No registry attached (single-DB embedded use): there's no
                // workspace layer, so writes always go to the per-DB table.
                return self.storage.set_setting_sync(key, value).await;
            }
        };

        if settings::is_workspace_only(key) {
            return registry.set_setting(key, value);
        }

        // Overridable: route based on database count.
        if registry.database_count()? <= 1 {
            registry.set_setting(key, value)
        } else {
            self.storage.set_setting_sync(key, value).await
        }
    }

    /// Clear the active database's per-DB override for `key`. The next read
    /// will resolve to the workspace default (registry) or the builtin
    /// default. Errors for workspace-only keys — those have no override to
    /// clear.
    pub async fn clear_override(&self, key: &str) -> Result<(), AtomicCoreError> {
        if settings::is_workspace_only(key) {
            return Err(AtomicCoreError::Validation(format!(
                "Setting '{}' is workspace-only and has no per-database override",
                key
            )));
        }
        self.storage.delete_setting_sync(key).await
    }

    /// Read the per-DB override row for `key`, if any. Skips the registry
    /// fallback — used by the "overrides across all databases" endpoint that
    /// needs just the override layer for one key, not the merged value.
    /// Returns Ok(None) for workspace-only keys (they cannot have overrides).
    pub async fn get_setting_override(&self, key: &str) -> Result<Option<String>, AtomicCoreError> {
        if settings::is_workspace_only(key) {
            return Ok(None);
        }
        self.storage.get_setting_sync(key).await
    }

    // ==================== API Token Operations ====================

    /// Create a new named API token. Returns metadata + the raw token (shown once).
    pub async fn create_api_token(
        &self,
        name: &str,
    ) -> Result<(tokens::ApiTokenInfo, String), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.create_api_token(name);
        }
        self.storage.create_api_token_sync(name).await
    }

    /// List all API tokens (metadata only, never includes raw token values).
    pub async fn list_api_tokens(&self) -> Result<Vec<tokens::ApiTokenInfo>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.list_api_tokens();
        }
        self.storage.list_api_tokens_sync().await
    }

    /// Verify a raw API token. Returns token info if valid and not revoked.
    pub async fn verify_api_token(
        &self,
        raw_token: &str,
    ) -> Result<Option<tokens::ApiTokenInfo>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.verify_api_token(raw_token);
        }
        self.storage.verify_api_token_sync(raw_token).await
    }

    /// Revoke an API token by ID.
    pub async fn revoke_api_token(&self, id: &str) -> Result<(), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.revoke_api_token(id);
        }
        self.storage.revoke_api_token_sync(id).await
    }

    /// Update the last_used_at timestamp for a token.
    pub async fn update_token_last_used(&self, id: &str) -> Result<(), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.update_token_last_used(id);
        }
        self.storage.update_token_last_used_sync(id).await
    }

    /// Migrate legacy server_auth_token from settings to api_tokens table.
    pub async fn migrate_legacy_token(&self) -> Result<bool, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.migrate_legacy_token();
        }
        self.storage.migrate_legacy_token_sync().await
    }

    /// Ensure at least one API token exists. Creates a "default" token if none exist.
    pub async fn ensure_default_token(
        &self,
    ) -> Result<Option<(tokens::ApiTokenInfo, String)>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.ensure_default_token();
        }
        self.storage.ensure_default_token_sync().await
    }

    // ==================== OAuth Operations ====================
    //
    // OAuth tables are server-global. SQLite stores them on registry.db; Postgres
    // stores them on the connected database (migration 006). Each method picks the
    // right backend in the same shape as the token methods above.

    /// Register a new OAuth client. Returns the generated `client_id`.
    pub async fn create_oauth_client(
        &self,
        client_name: &str,
        client_secret_hash: &str,
        redirect_uris_json: &str,
    ) -> Result<String, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.create_oauth_client(client_name, client_secret_hash, redirect_uris_json);
        }
        match &self.storage {
            #[cfg(feature = "postgres")]
            storage::StorageBackend::Postgres(pg) => {
                pg.create_oauth_client(client_name, client_secret_hash, redirect_uris_json)
                    .await
            }
            _ => Err(oauth_unavailable()),
        }
    }

    /// Look up an OAuth client's display name by its `client_id`.
    pub async fn get_oauth_client_name(
        &self,
        client_id: &str,
    ) -> Result<Option<String>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.get_oauth_client_name(client_id);
        }
        match &self.storage {
            #[cfg(feature = "postgres")]
            storage::StorageBackend::Postgres(pg) => pg.get_oauth_client_name(client_id).await,
            _ => Err(oauth_unavailable()),
        }
    }

    /// Look up the registered redirect URIs (JSON-encoded) for a client.
    pub async fn get_oauth_client_redirect_uris(
        &self,
        client_id: &str,
    ) -> Result<Option<String>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.get_oauth_client_redirect_uris(client_id);
        }
        match &self.storage {
            #[cfg(feature = "postgres")]
            storage::StorageBackend::Postgres(pg) => {
                pg.get_oauth_client_redirect_uris(client_id).await
            }
            _ => Err(oauth_unavailable()),
        }
    }

    /// Look up the stored client-secret hash for a client.
    pub async fn get_oauth_client_secret_hash(
        &self,
        client_id: &str,
    ) -> Result<Option<String>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.get_oauth_client_secret_hash(client_id);
        }
        match &self.storage {
            #[cfg(feature = "postgres")]
            storage::StorageBackend::Postgres(pg) => {
                pg.get_oauth_client_secret_hash(client_id).await
            }
            _ => Err(oauth_unavailable()),
        }
    }

    /// Persist a freshly issued authorization code.
    #[allow(clippy::too_many_arguments)]
    pub async fn store_oauth_code(
        &self,
        code_hash: &str,
        client_id: &str,
        code_challenge: &str,
        code_challenge_method: &str,
        redirect_uri: &str,
        created_at: &str,
        expires_at: &str,
    ) -> Result<(), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.store_oauth_code(
                code_hash,
                client_id,
                code_challenge,
                code_challenge_method,
                redirect_uri,
                created_at,
                expires_at,
            );
        }
        match &self.storage {
            #[cfg(feature = "postgres")]
            storage::StorageBackend::Postgres(pg) => {
                pg.store_oauth_code(
                    code_hash,
                    client_id,
                    code_challenge,
                    code_challenge_method,
                    redirect_uri,
                    created_at,
                    expires_at,
                )
                .await
            }
            _ => Err(oauth_unavailable()),
        }
    }

    /// Look up an authorization code by its hash.
    pub async fn lookup_oauth_code(
        &self,
        code_hash: &str,
    ) -> Result<Option<registry::OAuthCodeInfo>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.lookup_oauth_code(code_hash);
        }
        match &self.storage {
            #[cfg(feature = "postgres")]
            storage::StorageBackend::Postgres(pg) => pg.lookup_oauth_code(code_hash).await,
            _ => Err(oauth_unavailable()),
        }
    }

    /// Mark an authorization code as redeemed and record the issued token id.
    pub async fn mark_oauth_code_used(
        &self,
        code_hash: &str,
        token_id: Option<&str>,
    ) -> Result<(), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.mark_oauth_code_used(code_hash, token_id);
        }
        match &self.storage {
            #[cfg(feature = "postgres")]
            storage::StorageBackend::Postgres(pg) => {
                pg.mark_oauth_code_used(code_hash, token_id).await
            }
            _ => Err(oauth_unavailable()),
        }
    }

    // ==================== Atom Operations ====================

    /// Count total atoms in this database.
    pub async fn count_atoms(&self) -> Result<i32, AtomicCoreError> {
        self.storage.count_atoms_impl().await
    }

    /// Get all atoms with their tags
    pub async fn get_all_atoms(&self) -> Result<Vec<AtomWithTags>, AtomicCoreError> {
        self.storage.get_all_atoms_impl().await
    }

    /// Get a single atom by ID
    pub async fn get_atom(&self, id: &str) -> Result<Option<AtomWithTags>, AtomicCoreError> {
        self.storage.get_atom_impl(id).await
    }

    /// Get an atom by its source URL
    pub async fn get_atom_by_source_url(
        &self,
        url: &str,
    ) -> Result<Option<AtomWithTags>, AtomicCoreError> {
        self.storage.get_atom_by_source_url_sync(url).await
    }

    /// Create a new atom and trigger embedding generation
    ///
    /// The `on_event` callback will be invoked with progress events during
    /// embedding generation and tag extraction (which happens asynchronously).
    pub async fn create_atom<F>(
        &self,
        request: CreateAtomRequest,
        on_event: F,
    ) -> Result<Option<AtomWithTags>, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        // Skip if an atom with this source_url already exists
        if request.skip_if_source_exists {
            if let Some(ref url) = request.source_url {
                if self.storage.source_url_exists_sync(url).await? {
                    return Ok(None);
                }
            }
        }

        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let content = request.content.clone();

        let atom_with_tags = self.storage.insert_atom_impl(&id, &request, &now).await?;
        self.canvas_cache.invalidate();

        if !content.trim().is_empty() {
            let job = AtomPipelineJobRequest {
                atom_id: id,
                embed_requested: true,
                tag_requested: true,
                not_before: None,
                reason: "create_atom".to_string(),
                replace_existing: false,
            };
            self.storage.enqueue_pipeline_jobs_sync(&[job]).await?;
            self.process_queued_pipeline_jobs(on_event).await?;
        }

        Ok(Some(atom_with_tags))
    }

    /// Create multiple atoms in a single transaction and trigger batch embedding.
    ///
    /// All atoms are inserted in one transaction for efficiency. After commit,
    /// a single batch embedding task is spawned for all atoms.
    /// Atoms with a `source_url` that already exists in the database are skipped.
    /// Cap: 1000 atoms per call.
    pub async fn create_atoms_bulk<F>(
        &self,
        requests: Vec<CreateAtomRequest>,
        on_event: F,
    ) -> Result<BulkCreateResult, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        if requests.is_empty() {
            return Err(AtomicCoreError::Validation(
                "At least one atom is required".to_string(),
            ));
        }
        if requests.len() > 1000 {
            return Err(AtomicCoreError::Validation(
                "Maximum 1000 atoms per bulk create".to_string(),
            ));
        }

        let now = Utc::now().to_rfc3339();
        let mut skipped: usize = 0;

        // Dedup: check existing source_urls if any request opts in
        let any_skip = requests.iter().any(|r| r.skip_if_source_exists);
        let existing_urls = if any_skip {
            let source_urls: Vec<String> = requests
                .iter()
                .filter(|r| r.skip_if_source_exists)
                .filter_map(|r| r.source_url.clone())
                .collect();
            self.storage
                .check_existing_source_urls_sync(&source_urls)
                .await?
        } else {
            std::collections::HashSet::new()
        };

        // Filter requests, skipping duplicates when flagged
        let mut atoms_to_insert: Vec<(String, CreateAtomRequest, String)> =
            Vec::with_capacity(requests.len());
        for request in requests {
            if request.skip_if_source_exists {
                if let Some(ref url) = request.source_url {
                    if existing_urls.contains(url) {
                        skipped += 1;
                        continue;
                    }
                }
            }
            let id = Uuid::new_v4().to_string();
            atoms_to_insert.push((id, request, now.clone()));
        }

        // Bulk insert via storage
        let atoms_with_tags = self
            .storage
            .insert_atoms_bulk_impl(&atoms_to_insert)
            .await?;
        self.canvas_cache.invalidate();

        // Collect atom IDs for background embedding (don't clone content — read from DB later)
        let atom_ids: Vec<String> = atoms_with_tags
            .iter()
            .map(|awt| awt.atom.id.clone())
            .collect();

        if !atom_ids.is_empty() {
            let jobs: Vec<AtomPipelineJobRequest> = atom_ids
                .iter()
                .map(|atom_id| AtomPipelineJobRequest {
                    atom_id: atom_id.clone(),
                    embed_requested: true,
                    tag_requested: true,
                    not_before: None,
                    reason: "create_atoms_bulk".to_string(),
                    replace_existing: false,
                })
                .collect();
            self.storage.enqueue_pipeline_jobs_sync(&jobs).await?;
            self.process_queued_pipeline_jobs(on_event).await?;
        }

        let count = atoms_with_tags.len();
        Ok(BulkCreateResult {
            atoms: atoms_with_tags,
            count,
            skipped,
        })
    }

    /// Update an existing atom and trigger re-embedding
    pub async fn update_atom<F>(
        &self,
        id: &str,
        request: UpdateAtomRequest,
        on_event: F,
    ) -> Result<AtomWithTags, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let now = Utc::now().to_rfc3339();

        let atom_with_tags = self.storage.update_atom_impl(id, &request, &now).await?;
        self.canvas_cache.invalidate();

        let job = AtomPipelineJobRequest {
            atom_id: id.to_string(),
            embed_requested: true,
            tag_requested: true,
            not_before: None,
            reason: "update_atom".to_string(),
            replace_existing: false,
        };
        self.storage.enqueue_pipeline_jobs_sync(&[job]).await?;
        self.process_queued_pipeline_jobs(on_event).await?;

        Ok(atom_with_tags)
    }

    /// Update an atom only if its `updated_at` still matches the caller's
    /// previously-read value, then trigger re-embedding and re-tagging.
    pub async fn update_atom_if_unchanged<F>(
        &self,
        id: &str,
        request: UpdateAtomRequest,
        expected_updated_at: &str,
        on_event: F,
    ) -> Result<AtomWithTags, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let now = Utc::now().to_rfc3339();

        let atom_with_tags = self
            .storage
            .update_atom_if_unchanged_impl(id, &request, &now, expected_updated_at)
            .await?;
        self.canvas_cache.invalidate();

        let job = AtomPipelineJobRequest {
            atom_id: id.to_string(),
            embed_requested: true,
            tag_requested: true,
            not_before: None,
            reason: "update_atom_if_unchanged".to_string(),
            replace_existing: false,
        };
        self.storage.enqueue_pipeline_jobs_sync(&[job]).await?;
        self.process_queued_pipeline_jobs(on_event).await?;

        Ok(atom_with_tags)
    }

    /// Update an existing atom's content/metadata without triggering re-embedding or tagging.
    /// Used by auto-save during inline editing to persist content frequently without
    /// flooding the embedding pipeline. The full `update_atom` should be called when
    /// the user finishes editing to trigger the pipeline.
    pub async fn update_atom_content_only(
        &self,
        id: &str,
        request: UpdateAtomRequest,
    ) -> Result<AtomWithTags, AtomicCoreError> {
        let now = Utc::now().to_rfc3339();
        let had_content_before = self
            .storage
            .get_atom_content_impl(id)
            .await?
            .map(|content| !content.trim().is_empty())
            .unwrap_or(false);
        let result = self
            .storage
            .update_atom_content_only_impl(id, &request, &now)
            .await?;
        self.canvas_cache.invalidate();
        tracing::info!(
            atom_id = %id,
            had_content_before,
            has_content_now = !request.content.trim().is_empty(),
            embedding_status = %result.atom.embedding_status,
            tagging_status = %result.atom.tagging_status,
            "Draft atom save persisted"
        );
        Ok(result)
    }

    /// Delete an atom
    pub async fn delete_atom(&self, id: &str) -> Result<(), AtomicCoreError> {
        self.storage.delete_atom_impl(id).await?;
        self.canvas_cache.invalidate();
        Ok(())
    }

    /// Get atoms by tag (includes atoms with descendant tags).
    ///
    /// `kinds` is non-defaulted to force every caller to declare intent — see
    /// [`crate::models::KindFilter`]. Display callers pass `KindFilter::All`;
    /// synthesis / context-assembly callers (wiki gen, etc.) typically pass
    /// `KindFilter::only(AtomKind::Captured)` to keep agent-emitted findings
    /// out of the corpus.
    pub async fn get_atoms_by_tag(
        &self,
        tag_id: &str,
        kinds: &crate::models::KindFilter,
    ) -> Result<Vec<AtomWithTags>, AtomicCoreError> {
        self.storage.get_atoms_by_tag_impl(tag_id, kinds).await
    }

    /// Get materialized `[[...]]` links emitted by an atom.
    pub async fn get_atom_links(&self, atom_id: &str) -> Result<Vec<AtomLink>, AtomicCoreError> {
        self.storage.get_atom_links_impl(atom_id).await
    }

    /// Suggest atoms for editor `[[...]]` completion.
    ///
    /// Empty queries return recent atoms. Non-empty queries only match the
    /// current derived title; content and hybrid fallback should be composed
    /// by the caller.
    pub async fn suggest_atom_links(
        &self,
        query: &str,
        limit: i32,
    ) -> Result<Vec<AtomLinkSuggestion>, AtomicCoreError> {
        self.storage
            .suggest_atom_links_impl(query, limit.clamp(1, 50))
            .await
    }

    /// List atoms with pagination, filtering, sorting, and summaries (no full content).
    /// This is the primary frontend-facing method for loading atom lists.
    ///
    /// Supports cursor-based (keyset) pagination: when `cursor` and `cursor_id`
    /// are provided, the query seeks directly to that position, giving O(limit)
    /// performance regardless of page depth. Falls back to OFFSET when no cursor is given.
    pub async fn list_atoms(
        &self,
        params: &ListAtomsParams,
        kinds: &crate::models::KindFilter,
    ) -> Result<PaginatedAtoms, AtomicCoreError> {
        self.storage.list_atoms_impl(params, kinds).await
    }

    /// Get a list of distinct source values with counts (for filter dropdowns).
    pub async fn get_source_list(&self) -> Result<Vec<SourceInfo>, AtomicCoreError> {
        self.storage.get_source_list_impl().await
    }

    // ==================== Tag Operations ====================

    /// Get all tags with counts (hierarchical tree), no filtering
    pub async fn get_all_tags(&self) -> Result<Vec<TagWithCount>, AtomicCoreError> {
        self.storage.get_all_tags_impl().await
    }

    /// Get tags with counts, pruning leaf nodes below `min_count`.
    /// Sorted by atom_count descending at every level.
    pub async fn get_all_tags_filtered(
        &self,
        min_count: i32,
    ) -> Result<Vec<TagWithCount>, AtomicCoreError> {
        self.storage.get_all_tags_filtered_impl(min_count).await
    }

    /// Get direct children of a specific tag with pagination.
    /// Returns direct children only (with denormalized atom counts); grandchildren
    /// are loaded lazily via subsequent calls.
    pub async fn get_tag_children(
        &self,
        parent_id: &str,
        min_count: i32,
        limit: i32,
        offset: i32,
    ) -> Result<PaginatedTagChildren, AtomicCoreError> {
        self.storage
            .get_tag_children_impl(parent_id, min_count, limit, offset)
            .await
    }

    /// Load all tags and their direct counts from the database.
    /// Reads the denormalized atom_count column instead of scanning atom_tags.
    /// Create a new tag
    pub async fn create_tag(
        &self,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<Tag, AtomicCoreError> {
        self.storage.create_tag_impl(name, parent_id).await
    }

    /// Update a tag
    pub async fn update_tag(
        &self,
        id: &str,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<Tag, AtomicCoreError> {
        let tag = self.storage.update_tag_impl(id, name, parent_id).await?;
        self.canvas_cache.invalidate();
        Ok(tag)
    }

    /// Delete a tag
    pub async fn delete_tag(&self, id: &str, recursive: bool) -> Result<(), AtomicCoreError> {
        self.storage.delete_tag_impl(id, recursive).await?;
        self.canvas_cache.invalidate();
        Ok(())
    }

    /// Mark or unmark a top-level tag as a candidate for AI auto-tagging to extend.
    /// When false, the auto-tagger will not create new sub-tags under this tag.
    pub async fn set_tag_autotag_target(
        &self,
        id: &str,
        value: bool,
    ) -> Result<(), AtomicCoreError> {
        self.storage.set_tag_autotag_target_impl(id, value).await
    }

    /// Set optional guidance for a top-level auto-tag target. When present,
    /// the guidance is injected next to the category name in the auto-tagging
    /// prompt so the model knows how the user intends that category to be used.
    pub async fn set_tag_autotag_description(
        &self,
        id: &str,
        description: &str,
    ) -> Result<(), AtomicCoreError> {
        self.storage
            .set_tag_autotag_description_impl(id, description)
            .await
    }

    /// Configure auto-tag targets in one shot — used by the onboarding wizard
    /// and the settings tab.
    ///
    /// `keep_default_names`: which of the well-known default category names
    /// (case-insensitive) the user wants. Each one is created if it doesn't exist
    /// and flagged as an auto-tag target.
    ///
    /// `add_custom_names`: new top-level tag names to create with the flag set.
    /// Names that already exist as top-level tags are flagged in place rather than duplicated.
    ///
    /// Defaults that exist but are NOT in `keep_default_names` are deleted if they
    /// have no atoms or sub-tags (the safe case during onboarding). If they have
    /// content, they're unflagged instead so their data isn't lost — re-runs from
    /// settings after the user has tagged things stay non-destructive.
    ///
    /// All steps run in a single storage-layer transaction, so a failure mid-flight
    /// rolls back cleanly rather than leaving the tags table partially modified.
    pub async fn configure_autotag_targets(
        &self,
        keep_default_names: &[String],
        add_custom_names: &[String],
    ) -> Result<Vec<Tag>, AtomicCoreError> {
        self.storage
            .configure_autotag_targets_impl(keep_default_names, add_custom_names)
            .await
    }

    // ==================== Search Operations ====================

    /// Search atoms using the configured search mode.
    pub async fn search(
        &self,
        options: SearchOptions,
    ) -> Result<Vec<SemanticSearchResult>, AtomicCoreError> {
        // SQLite path: use the full search module (handles embedding generation + search)
        if let Some(sqlite) = self.storage.as_sqlite() {
            return search::search_atoms_with_settings(
                &sqlite.db,
                options,
                self.settings_for_background().await,
            )
            .await
            .map_err(|e| AtomicCoreError::Search(e));
        }

        // Postgres path: use storage dispatch methods directly
        let settings = self.get_settings().await?;
        let config = providers::ProviderConfig::from_settings(&settings);
        let tag_id = options.scope_tag_ids.first().map(|s| s.as_str());
        let cutoff = options.since_days.map(search::since_days_cutoff);
        let cutoff_ref = cutoff.as_deref();

        match options.mode {
            search::SearchMode::Keyword => {
                self.storage
                    .keyword_search_sync(
                        &options.query,
                        options.limit,
                        tag_id,
                        cutoff_ref,
                        &options.kinds,
                    )
                    .await
            }
            search::SearchMode::Semantic => {
                // Generate query embedding via provider
                let provider = providers::get_embedding_provider(&config)
                    .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
                let embed_config = providers::EmbeddingConfig::new(config.embedding_model());
                let embeddings = provider
                    .embed_batch(&[options.query.clone()], &embed_config)
                    .await
                    .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
                if embeddings.is_empty() || embeddings[0].is_empty() {
                    return Ok(vec![]);
                }
                self.storage
                    .vector_search_sync(
                        &embeddings[0],
                        options.limit,
                        options.threshold,
                        tag_id,
                        cutoff_ref,
                        &options.kinds,
                    )
                    .await
            }
            search::SearchMode::Hybrid => {
                // Generate embedding for semantic leg
                let provider = providers::get_embedding_provider(&config)
                    .map_err(|e| AtomicCoreError::Search(e.to_string()))?;
                let embed_config = providers::EmbeddingConfig::new(config.embedding_model());
                let embeddings = provider
                    .embed_batch(&[options.query.clone()], &embed_config)
                    .await
                    .map_err(|e| AtomicCoreError::Search(e.to_string()))?;

                let keyword_results = self
                    .storage
                    .keyword_search_sync(
                        &options.query,
                        options.limit * 2,
                        tag_id,
                        cutoff_ref,
                        &options.kinds,
                    )
                    .await?;

                let semantic_results = if !embeddings.is_empty() && !embeddings[0].is_empty() {
                    self.storage
                        .vector_search_sync(
                            &embeddings[0],
                            options.limit * 2,
                            options.threshold,
                            tag_id,
                            cutoff_ref,
                            &options.kinds,
                        )
                        .await?
                } else {
                    vec![]
                };

                // Reciprocal Rank Fusion to merge results
                Ok(search::merge_search_results_rrf(
                    semantic_results,
                    keyword_results,
                    options.limit,
                ))
            }
        }
    }

    /// Keyword-only search across atoms, wiki articles, chats, and tags for the global search palette.
    pub async fn search_global_keyword(
        &self,
        query: &str,
        section_limit: i32,
    ) -> Result<GlobalSearchResponse, AtomicCoreError> {
        if let Some(sqlite) = self.storage.as_sqlite() {
            let sqlite = sqlite.clone();
            let query = query.to_string();
            return tokio::task::spawn_blocking(move || {
                sqlite.global_keyword_search_sync(&query, section_limit)
            })
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("spawn_blocking join: {e}"))
            })?;
        }

        #[cfg(feature = "postgres")]
        if let Some(pg) = self.storage.as_postgres() {
            return pg.global_keyword_search(query, section_limit).await;
        }

        Err(AtomicCoreError::Search(
            "Global keyword search is not implemented for this storage backend".to_string(),
        ))
    }

    /// Find atoms similar to a given atom
    pub async fn find_similar(
        &self,
        atom_id: &str,
        limit: i32,
        threshold: f32,
    ) -> Result<Vec<SimilarAtomResult>, AtomicCoreError> {
        self.storage
            .find_similar_sync(atom_id, limit, threshold)
            .await
    }

    // ==================== Wiki Operations ====================

    /// Build a WikiStrategyContext from current settings.
    async fn build_wiki_strategy_context(
        &self,
        tag_id: &str,
        tag_name: &str,
    ) -> Result<(wiki::WikiStrategy, wiki::WikiStrategyContext), AtomicCoreError> {
        const MAX_CROSS_LINK_TAGS: usize = 50;
        let settings_map = self.get_settings().await?;
        let config = ProviderConfig::from_settings(&settings_map);
        let model = match config.provider_type {
            ProviderType::Ollama => config.llm_model().to_string(),
            ProviderType::OpenAICompat => config.llm_model().to_string(),
            ProviderType::OpenRouter => settings_map
                .get("wiki_model")
                .cloned()
                .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string()),
        };
        let strategy = wiki::WikiStrategy::from_string(
            settings_map
                .get("wiki_strategy")
                .map(|s| s.as_str())
                .unwrap_or("centroid"),
        );
        let related = self
            .storage
            .get_related_tags_impl(tag_id, MAX_CROSS_LINK_TAGS)
            .await
            .unwrap_or_default();
        let linkable_article_names: Vec<(String, String)> = related
            .into_iter()
            .filter(|t| t.has_article)
            .map(|t| (t.tag_id, t.tag_name))
            .collect();
        tracing::info!(strategy = ?strategy, model, cross_link_articles = linkable_article_names.len(), "[wiki] Configuration");

        let ctx = wiki::WikiStrategyContext {
            storage: self.storage.clone(),
            provider_config: config,
            wiki_model: model,
            tag_id: tag_id.to_string(),
            tag_name: tag_name.to_string(),
            linkable_article_names,
            custom_generation_prompt: settings_map.get("wiki_generation_prompt").cloned(),
            custom_update_prompt: settings_map.get("wiki_update_prompt").cloned(),
        };
        Ok((strategy, ctx))
    }

    /// Generate a wiki article for a tag
    pub async fn generate_wiki(
        &self,
        tag_id: &str,
        tag_name: &str,
    ) -> Result<WikiArticleWithCitations, AtomicCoreError> {
        // Hold the per-tag lock for the whole operation so a concurrent
        // propose/accept can't race the regeneration and leave a proposal
        // pointing at an article version that's about to be replaced.
        let _guard = self.wiki_tag_lock(tag_id).await;
        tracing::info!(tag_name, tag_id, "[wiki] Generating article");

        let (strategy, ctx) = self.build_wiki_strategy_context(tag_id, tag_name).await?;

        let result = wiki::strategy_generate(&strategy, &ctx)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e))?;

        // Extract wiki links from generated content
        let wiki_links = wiki::extract_wiki_links(
            &result.article.id,
            &result.article.content,
            &ctx.linkable_article_names,
        );
        tracing::info!(
            wiki_links = wiki_links.len(),
            citations = result.citations.len(),
            "[wiki] Extracted links and citations"
        );

        // Save to database
        self.storage
            .save_wiki_with_links_sync(&result.article, &result.citations, &wiki_links)
            .await?;

        // Any pending proposal was computed against the previous live article
        // and is now meaningless — drop it. Log-and-continue on error; a stale
        // proposal will still be caught by the base_updated_at check on accept.
        if let Err(e) = self.storage.delete_wiki_proposal_sync(tag_id).await {
            tracing::warn!(tag_id, error = %e, "[wiki] Failed to clean up pending proposal after regenerate");
        }

        tracing::info!("[wiki] Article saved successfully");
        Ok(result)
    }

    /// Acquire the per-tag wiki lock. Serializes propose/accept/dismiss/update
    /// operations against the same article so background + manual runs can't
    /// race and proposals can't be applied mid-rewrite.
    async fn wiki_tag_lock(&self, tag_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self
                .wiki_tag_locks
                .lock()
                .expect("wiki_tag_locks mutex poisoned");
            map.entry(tag_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }

    /// Update an existing wiki article with new content (legacy full-rewrite path).
    ///
    /// Deprecated: prefer `propose_wiki_update` + `accept_wiki_proposal` for the
    /// human-in-the-loop review flow. This method remains for backwards
    /// compatibility with external MCP clients and will be removed in a later release.
    pub async fn update_wiki(
        &self,
        tag_id: &str,
        tag_name: &str,
    ) -> Result<WikiArticleWithCitations, AtomicCoreError> {
        tracing::warn!(
            tag_id,
            "[wiki] update_wiki is deprecated; use propose_wiki_update instead"
        );
        let _guard = self.wiki_tag_lock(tag_id).await;
        tracing::info!(tag_name, tag_id, "[wiki] Updating article");

        let existing = self
            .get_wiki(tag_id)
            .await?
            .ok_or_else(|| AtomicCoreError::Wiki("No existing article to update".to_string()))?;

        let (strategy, ctx) = self.build_wiki_strategy_context(tag_id, tag_name).await?;

        let result = wiki::strategy_update(&strategy, &ctx, &existing)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e))?;

        // If no update needed, return existing article
        let result = match result {
            Some(r) => r,
            None => return Ok(existing),
        };

        // Extract wiki links from updated content
        let wiki_links = wiki::extract_wiki_links(
            &result.article.id,
            &result.article.content,
            &ctx.linkable_article_names,
        );

        // Save to database
        self.storage
            .save_wiki_with_links_sync(&result.article, &result.citations, &wiki_links)
            .await?;

        tracing::info!("[wiki] Article updated successfully");
        Ok(result)
    }

    /// Propose an update to an existing wiki article.
    ///
    /// Runs the strategy's chunk selector, then the shared section-ops
    /// generator, and writes the result to `wiki_proposals`. Supersedes any
    /// existing pending proposal for the tag. Returns `None` when the strategy
    /// determines no update is warranted (no new atoms, or the LLM returned
    /// NoChange).
    pub async fn propose_wiki_update(
        &self,
        tag_id: &str,
        tag_name: &str,
    ) -> Result<Option<WikiProposal>, AtomicCoreError> {
        let _guard = self.wiki_tag_lock(tag_id).await;
        tracing::info!(tag_name, tag_id, "[wiki] Proposing article update");

        let existing = self.get_wiki(tag_id).await?.ok_or_else(|| {
            AtomicCoreError::Wiki("No existing article to propose update against".to_string())
        })?;

        let (strategy, ctx) = self.build_wiki_strategy_context(tag_id, tag_name).await?;

        let draft = match wiki::strategy_propose_outcome(&strategy, &ctx, &existing)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e))?
        {
            wiki::WikiProposalOutcome::Draft(d) => d,
            wiki::WikiProposalOutcome::NoChange => {
                // The LLM evaluated update chunks and decided nothing needs to
                // change. Advance the baseline so the same atoms are not
                // re-evaluated on every subsequent "Generate Update" click.
                if let Err(e) = self.storage.advance_wiki_baseline_sync(tag_id, None).await {
                    tracing::warn!(tag_id, error = %e, "[wiki] Failed to advance article baseline on no-change");
                } else {
                    tracing::info!(
                        tag_id,
                        "[wiki] No update warranted; article baseline advanced"
                    );
                }
                return Ok(None);
            }
            wiki::WikiProposalOutcome::NoUpdateChunks => {
                // No chunks were selected. This can mean there are truly no new
                // atoms, but it can also mean older atoms were newly associated
                // with this tag hierarchy. Only advance if the current tag count
                // has not increased beyond the article's recorded baseline.
                match self
                    .storage
                    .advance_wiki_baseline_sync(tag_id, Some(existing.article.atom_count))
                    .await
                {
                    Ok(true) => {
                        tracing::info!(
                            tag_id,
                            "[wiki] No update chunks selected; article baseline advanced"
                        );
                    }
                    Ok(false) => {
                        tracing::info!(
                            tag_id,
                            "[wiki] No update chunks selected; article baseline left unchanged because atom count increased"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(tag_id, error = %e, "[wiki] Failed to advance article baseline after empty update selection");
                    }
                }
                return Ok(None);
            }
        };

        let proposal = WikiProposal {
            id: uuid::Uuid::new_v4().to_string(),
            tag_id: tag_id.to_string(),
            base_article_id: existing.article.id.clone(),
            base_updated_at: existing.article.updated_at.clone(),
            content: draft.merged_content,
            citations: draft.citations,
            ops: draft.ops,
            new_atom_count: draft.new_atom_count,
            created_at: chrono::Utc::now().to_rfc3339(),
        };

        self.storage.save_wiki_proposal_sync(&proposal).await?;
        tracing::info!(
            tag_id,
            proposal_id = %proposal.id,
            ops = proposal.ops.len(),
            "[wiki] Proposal saved"
        );

        Ok(Some(proposal))
    }

    /// Get the pending wiki proposal for a tag, if any.
    pub async fn get_wiki_proposal(
        &self,
        tag_id: &str,
    ) -> Result<Option<WikiProposal>, AtomicCoreError> {
        self.storage.get_wiki_proposal_sync(tag_id).await
    }

    /// Accept the pending wiki proposal: promote to live article, archive the
    /// prior version, delete the proposal row.
    ///
    /// Rejects the accept if the live article has been updated out-of-band
    /// since the proposal was computed (stale base). The caller should catch
    /// this error, refetch, and ask the user to regenerate the proposal.
    pub async fn accept_wiki_proposal(
        &self,
        tag_id: &str,
    ) -> Result<WikiArticleWithCitations, AtomicCoreError> {
        let _guard = self.wiki_tag_lock(tag_id).await;

        let proposal = self
            .storage
            .get_wiki_proposal_sync(tag_id)
            .await?
            .ok_or_else(|| AtomicCoreError::Wiki("No pending proposal for this tag".to_string()))?;

        let existing = self.get_wiki(tag_id).await?.ok_or_else(|| {
            AtomicCoreError::Wiki("Live article disappeared while proposal was pending".to_string())
        })?;

        if existing.article.updated_at != proposal.base_updated_at
            || existing.article.id != proposal.base_article_id
        {
            tracing::warn!(
                tag_id,
                base = %proposal.base_updated_at,
                live = %existing.article.updated_at,
                "[wiki] Stale proposal — live article was updated out-of-band"
            );
            return Err(AtomicCoreError::Wiki(
                "Proposal is stale — live article was updated since the proposal was computed. Regenerate the proposal to review it.".to_string(),
            ));
        }

        let article = WikiArticle {
            id: existing.article.id.clone(),
            tag_id: tag_id.to_string(),
            content: proposal.content.clone(),
            created_at: existing.article.created_at.clone(),
            updated_at: proposal.created_at.clone(),
            atom_count: existing.article.atom_count + proposal.new_atom_count,
        };

        // Extract wiki links from the merged content. Build the linkable-
        // article-names list the same way build_wiki_strategy_context does.
        const MAX_CROSS_LINK_TAGS: usize = 50;
        let related = self
            .storage
            .get_related_tags_impl(tag_id, MAX_CROSS_LINK_TAGS)
            .await
            .unwrap_or_default();
        let linkable_names: Vec<(String, String)> = related
            .into_iter()
            .filter(|t| t.has_article)
            .map(|t| (t.tag_id, t.tag_name))
            .collect();
        let wiki_links = wiki::extract_wiki_links(&article.id, &article.content, &linkable_names);

        // save_wiki_with_links archives the previous version into
        // wiki_article_versions as part of its normal flow.
        self.storage
            .save_wiki_with_links_sync(&article, &proposal.citations, &wiki_links)
            .await?;

        // Delete the proposal row after successful save.
        self.storage.delete_wiki_proposal_sync(tag_id).await?;

        tracing::info!(
            tag_id,
            "[wiki] Proposal accepted and promoted to live article"
        );

        Ok(WikiArticleWithCitations {
            article,
            citations: proposal.citations,
        })
    }

    /// Dismiss the pending wiki proposal (delete without promoting). Idempotent.
    pub async fn dismiss_wiki_proposal(&self, tag_id: &str) -> Result<(), AtomicCoreError> {
        let _guard = self.wiki_tag_lock(tag_id).await;
        self.storage.delete_wiki_proposal_sync(tag_id).await?;
        tracing::info!(tag_id, "[wiki] Proposal dismissed");
        Ok(())
    }

    /// Get an existing wiki article
    pub async fn get_wiki(
        &self,
        tag_id: &str,
    ) -> Result<Option<WikiArticleWithCitations>, AtomicCoreError> {
        self.storage.get_wiki_sync(tag_id).await
    }

    /// Get wiki article status (for checking if update is needed)
    pub async fn get_wiki_status(
        &self,
        tag_id: &str,
    ) -> Result<WikiArticleStatus, AtomicCoreError> {
        self.storage.get_wiki_status_sync(tag_id).await
    }

    /// Delete a wiki article (and any pending proposal for it — once the
    /// underlying article is gone, the proposal references a base that no
    /// longer exists).
    pub async fn delete_wiki(&self, tag_id: &str) -> Result<(), AtomicCoreError> {
        self.storage.delete_wiki_sync(tag_id).await?;
        if let Err(e) = self.storage.delete_wiki_proposal_sync(tag_id).await {
            tracing::warn!(tag_id, error = %e, "[wiki] Failed to clean up pending proposal after delete");
        }
        Ok(())
    }

    /// Get tags related to a given tag by semantic connectivity
    pub async fn get_related_tags(
        &self,
        tag_id: &str,
        limit: usize,
    ) -> Result<Vec<RelatedTag>, AtomicCoreError> {
        self.storage.get_related_tags_impl(tag_id, limit).await
    }

    /// Get wiki links (outgoing cross-references) for an article
    pub async fn get_wiki_links(&self, tag_id: &str) -> Result<Vec<WikiLink>, AtomicCoreError> {
        self.storage.get_wiki_links_sync(tag_id).await
    }

    /// List version history for a wiki article
    pub async fn list_wiki_versions(
        &self,
        tag_id: &str,
    ) -> Result<Vec<WikiVersionSummary>, AtomicCoreError> {
        self.storage.list_wiki_versions_sync(tag_id).await
    }

    /// Get a specific wiki article version
    pub async fn get_wiki_version(
        &self,
        version_id: &str,
    ) -> Result<Option<WikiArticleVersion>, AtomicCoreError> {
        self.storage.get_wiki_version_sync(version_id).await
    }

    // ==================== Reports ====================
    //
    // Thin wrappers around `ReportStore` so transport layers (REST, MCP,
    // future Tauri IPC) don't reach into `core.storage` directly. The
    // runner entry points (`run_report_*`) compose ledger + agentic +
    // storage and are the real heavy lifting; everything else is CRUD.

    pub async fn list_reports(&self) -> Result<Vec<models::Report>, AtomicCoreError> {
        self.storage.list_reports_sync().await
    }

    pub async fn list_enabled_reports(&self) -> Result<Vec<models::Report>, AtomicCoreError> {
        self.storage.list_enabled_reports_sync().await
    }

    pub async fn get_report(&self, id: &str) -> Result<Option<models::Report>, AtomicCoreError> {
        self.storage.get_report_sync(id).await
    }

    pub async fn create_report(
        &self,
        request: models::CreateReportRequest,
    ) -> Result<models::Report, AtomicCoreError> {
        use std::str::FromStr;
        // Validate the cron expression at the API boundary so authoring
        // clients get a 400 instead of a deferred "schedule is invalid"
        // log line at runtime.
        cron::Schedule::from_str(&request.schedule)
            .map_err(|e| AtomicCoreError::Validation(format!("invalid cron expression: {e}")))?;
        if let Some(tz) = &request.schedule_tz {
            tz.parse::<chrono_tz::Tz>().map_err(|e| {
                AtomicCoreError::Validation(format!("invalid timezone '{tz}': {e}"))
            })?;
        }
        self.storage.insert_report_sync(&request).await
    }

    pub async fn update_report(
        &self,
        id: &str,
        request: models::UpdateReportRequest,
    ) -> Result<models::Report, AtomicCoreError> {
        use std::str::FromStr;
        // Validate schedule changes the same way as `create_report`.
        if let Some(s) = &request.schedule {
            cron::Schedule::from_str(s).map_err(|e| {
                AtomicCoreError::Validation(format!("invalid cron expression: {e}"))
            })?;
        }
        if let Some(Some(tz)) = &request.schedule_tz {
            tz.parse::<chrono_tz::Tz>().map_err(|e| {
                AtomicCoreError::Validation(format!("invalid timezone '{tz}': {e}"))
            })?;
        }
        self.storage.update_report_sync(id, &request).await
    }

    pub async fn set_report_enabled(&self, id: &str, enabled: bool) -> Result<(), AtomicCoreError> {
        self.storage.set_report_enabled_sync(id, enabled).await
    }

    pub async fn delete_report(&self, id: &str) -> Result<(), AtomicCoreError> {
        // Clear the dashboard pointer if it referenced this report, so the
        // widget falls into its empty state rather than chasing a dead id.
        // Per-DB setting — read via `storage().get_setting_sync` to bypass
        // the registry-routed `get_settings` path.
        if let Some(featured) = self
            .storage
            .get_setting_sync(FEATURED_REPORT_SETTING)
            .await?
        {
            if featured == id {
                self.storage
                    .delete_setting_sync(FEATURED_REPORT_SETTING)
                    .await?;
            }
        }
        self.storage.delete_report_sync(id).await
    }

    /// Read the per-DB dashboard featured-report id, if any. Returns `None`
    /// when the setting is unset or the referenced report no longer exists.
    /// A stale pointer is self-healing: callers see `None` and the next
    /// `set_featured_report_id` (or a fresh seed) re-points it.
    pub async fn get_featured_report_id(&self) -> Result<Option<String>, AtomicCoreError> {
        let Some(id) = self
            .storage
            .get_setting_sync(FEATURED_REPORT_SETTING)
            .await?
        else {
            return Ok(None);
        };
        if id.is_empty() {
            return Ok(None);
        }
        if self.storage.get_report_sync(&id).await?.is_none() {
            return Ok(None);
        }
        Ok(Some(id))
    }

    /// Set (or clear) the per-DB dashboard featured-report id. Stored
    /// directly through `storage()` rather than `set_setting` so the value
    /// stays isolated per database — the dashboard a user sees should
    /// reflect *that* DB's chosen report, not a registry-wide default.
    pub async fn set_featured_report_id(
        &self,
        report_id: Option<&str>,
    ) -> Result<(), AtomicCoreError> {
        match report_id {
            Some(id) if !id.is_empty() => {
                if self.storage.get_report_sync(id).await?.is_none() {
                    return Err(AtomicCoreError::Validation(format!(
                        "report not found: {id}"
                    )));
                }
                self.storage
                    .set_setting_sync(FEATURED_REPORT_SETTING, id)
                    .await
            }
            _ => {
                self.storage
                    .delete_setting_sync(FEATURED_REPORT_SETTING)
                    .await
            }
        }
    }

    pub async fn list_findings_for_report(
        &self,
        report_id: &str,
        limit: i32,
    ) -> Result<Vec<(models::ReportFinding, models::AtomWithTags)>, AtomicCoreError> {
        self.storage
            .list_findings_for_report_sync(report_id, limit)
            .await
    }

    /// Fetch the citation rows for a finding atom, ordered by `position`.
    /// Dashboard widget pairs this with the finding atom content to render
    /// `[N]` markers as clickable popovers, matching the old briefing UX.
    pub async fn list_citations_for_finding(
        &self,
        finding_atom_id: &str,
    ) -> Result<Vec<models::ReportFindingCitation>, AtomicCoreError> {
        self.storage
            .list_citations_for_finding_sync(finding_atom_id)
            .await
    }

    /// Fetch the provenance row (`ReportFinding`) for a finding atom.
    /// `None` if the atom isn't a finding (no provenance recorded), or
    /// if the atom doesn't exist. The FindingReader frontend view uses
    /// this to surface the parent report name + run timestamp on a
    /// cold deep-link where the row isn't in any in-memory cache.
    pub async fn get_finding_provenance(
        &self,
        finding_atom_id: &str,
    ) -> Result<Option<models::ReportFinding>, AtomicCoreError> {
        self.storage
            .get_finding_provenance_sync(finding_atom_id)
            .await
    }

    /// Manual "run now" entry point — same machinery as the scheduled
    /// loop, but the run row carries `trigger = 'manual'` for history.
    /// Returns immediately with a `RunOutcome::Skipped` if the report
    /// has work in flight; otherwise blocks until the run finishes (the
    /// HTTP handler wraps this in a `tokio::spawn` for 202-style async).
    pub async fn run_report_now(
        &self,
        report_id: &str,
    ) -> Result<reports::RunOutcome, AtomicCoreError> {
        let report = self
            .storage
            .get_report_sync(report_id)
            .await?
            .ok_or_else(|| {
                AtomicCoreError::DatabaseOperation(format!("report {report_id} not found"))
            })?;
        reports::run_report(self, &report, models::TaskRunTrigger::Manual).await
    }

    // ==================== Embedding Management ====================

    /// Process all pending embeddings
    pub async fn process_pending_embeddings<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let on_event = self.wrap_event_for_cache(on_event);
        let canvas_cache = Some(self.canvas_cache.clone());
        match self.settings_for_background().await {
            Some(s) => embedding::process_pending_embeddings_with_settings(
                self.storage.clone(),
                on_event,
                s,
                canvas_cache,
            )
            .await
            .map_err(|e| AtomicCoreError::Embedding(e)),
            None => {
                embedding::process_pending_embeddings(self.storage.clone(), on_event, canvas_cache)
                    .await
                    .map_err(|e| AtomicCoreError::Embedding(e))
            }
        }
    }

    /// Process due jobs from the unified embedding/tagging queue.
    pub async fn process_queued_pipeline_jobs<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let on_event = self.wrap_event_for_cache(on_event);
        let canvas_cache = Some(self.canvas_cache.clone());
        match self.settings_for_background().await {
            Some(s) => embedding::process_queued_pipeline_jobs_with_settings(
                self.storage.clone(),
                on_event,
                s,
                canvas_cache,
            )
            .await
            .map_err(AtomicCoreError::Embedding),
            None => embedding::process_queued_pipeline_jobs(
                self.storage.clone(),
                on_event,
                canvas_cache,
            )
            .await
            .map_err(AtomicCoreError::Embedding),
        }
    }

    /// Process pending embeddings only for atoms last updated at or before
    /// `cutoff`. Used by the draft-pipeline scheduler so active edits can
    /// settle before background AI work begins.
    pub async fn process_pending_embeddings_due<F>(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
        on_event: F,
    ) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let cutoff_rfc3339 = cutoff.to_rfc3339();
        let on_event = self.wrap_event_for_cache(on_event);
        let canvas_cache = Some(self.canvas_cache.clone());
        let count = match self.settings_for_background().await {
            Some(s) => {
                embedding::process_pending_embeddings_due_with_settings(
                    self.storage.clone(),
                    on_event,
                    cutoff_rfc3339.clone(),
                    s,
                    canvas_cache,
                )
                .await
            }
            None => {
                embedding::process_pending_embeddings_due(
                    self.storage.clone(),
                    on_event,
                    cutoff_rfc3339.clone(),
                    canvas_cache,
                )
                .await
            }
        }
        .map_err(|e| AtomicCoreError::Embedding(e))?;
        if count > 0 {
            tracing::info!(cutoff = %cutoff_rfc3339, count, "Queued pending embeddings due for processing");
        }
        Ok(count)
    }

    /// Process all atoms with pending edge computation in batches.
    /// Runs in the background with checkpointing so it survives restarts.
    pub async fn process_pending_edges(&self) -> Result<i32, AtomicCoreError> {
        embedding::process_pending_edges(self.storage.clone(), Some(self.canvas_cache.clone()))
            .await
            .map_err(|e| AtomicCoreError::Embedding(e))
    }

    /// Process deferred graph maintenance immediately.
    ///
    /// This is the synchronous path used by tests and manual callers that need
    /// semantic edges/tag centroids to be current before reading the graph. The
    /// normal server path runs the same work from `GraphMaintenanceTask`.
    pub async fn process_graph_maintenance(&self) -> Result<(), AtomicCoreError> {
        graph_maintenance::run_now(self).await
    }

    /// Reset atoms stuck in 'processing' state back to 'pending'
    pub async fn reset_stuck_processing(&self) -> Result<i32, AtomicCoreError> {
        self.storage.reset_stuck_processing_sync().await
    }

    /// Retry embedding for a specific atom
    pub async fn retry_embedding<F>(
        &self,
        atom_id: &str,
        on_event: F,
    ) -> Result<(), AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let status = self.storage.get_embedding_status_impl(atom_id).await?;
        if status == "processing" {
            return Err(AtomicCoreError::Validation(format!(
                "Atom {} is already being embedded",
                atom_id
            )));
        }

        self.storage
            .get_atom_content_impl(atom_id)
            .await?
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Atom {} not found", atom_id)))?;
        let tagging_status = self.storage.get_tagging_status_impl(atom_id).await?;
        self.storage
            .set_embedding_status_sync(atom_id, "pending", None)
            .await?;
        let job = AtomPipelineJobRequest {
            atom_id: atom_id.to_string(),
            embed_requested: true,
            tag_requested: tagging_status == "pending",
            not_before: None,
            reason: "retry_embedding".to_string(),
            replace_existing: false,
        };
        self.storage.enqueue_pipeline_jobs_sync(&[job]).await?;
        self.process_queued_pipeline_jobs(on_event).await?;

        Ok(())
    }

    /// Claim atoms currently marked `pending`/`processing` and spawn a background
    /// task to re-embed them (with tagging skipped — existing tags are preserved).
    /// Returns the number of atoms queued. Used after a dimension change to
    /// re-embed each database's content.
    pub async fn spawn_reembed_pending<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let pending_ids = self.storage.claim_pending_reembedding_sync().await?;
        let count = pending_ids.len() as i32;

        if count > 0 {
            let jobs: Vec<AtomPipelineJobRequest> = pending_ids
                .into_iter()
                .map(|atom_id| AtomPipelineJobRequest {
                    atom_id,
                    embed_requested: true,
                    tag_requested: false,
                    not_before: None,
                    reason: "spawn_reembed_pending".to_string(),
                    replace_existing: true,
                })
                .collect();
            self.storage.enqueue_pipeline_jobs_sync(&jobs).await?;
            self.process_queued_pipeline_jobs(on_event).await?;
        }

        Ok(count)
    }

    /// Re-embed all atoms in the database
    pub async fn reembed_all_atoms<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let atom_ids = self.storage.claim_all_for_reembedding_sync().await?;
        // The claim flips every atom 'complete' → 'processing' unconditionally,
        // so atoms disappear from the canvas query immediately. Drop any warm
        // cache so reads during the re-embed window don't serve stale data.
        self.canvas_cache.invalidate();
        let count = atom_ids.len() as i32;

        if count > 0 {
            let jobs: Vec<AtomPipelineJobRequest> = atom_ids
                .into_iter()
                .map(|atom_id| AtomPipelineJobRequest {
                    atom_id,
                    embed_requested: true,
                    tag_requested: false,
                    not_before: None,
                    reason: "reembed_all_atoms".to_string(),
                    replace_existing: true,
                })
                .collect();
            self.storage.enqueue_pipeline_jobs_sync(&jobs).await?;
            self.process_queued_pipeline_jobs(on_event).await?;
        }

        Ok(count)
    }

    /// Re-tag all embedding-complete atoms in the database. Removes
    /// auto-source tag assignments whose tag has no wiki article, then queues
    /// claimable atoms for tag-only pipeline processing. Manual assignments
    /// and wiki-backed tag assignments are preserved.
    ///
    /// Returns the number of atoms queued for re-tagging.
    pub async fn retag_all_atoms<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        // Prune first so the re-extraction starts from a clean baseline.
        // Manual rows and wiki-backed tag rows are preserved by the WHERE clause.
        self.storage.delete_auto_tags_without_wiki_sync().await?;

        let atom_ids = self.storage.claim_all_for_retagging_sync().await?;
        let count = atom_ids.len() as i32;

        if count > 0 {
            let jobs: Vec<AtomPipelineJobRequest> = atom_ids
                .into_iter()
                .map(|atom_id| AtomPipelineJobRequest {
                    atom_id,
                    embed_requested: false,
                    tag_requested: true,
                    not_before: None,
                    reason: "retag_all_atoms".to_string(),
                    replace_existing: false,
                })
                .collect();
            self.storage.enqueue_pipeline_jobs_sync(&jobs).await?;
            self.process_queued_pipeline_jobs(on_event).await?;
        }

        Ok(count)
    }

    /// Retry tagging for a specific atom
    pub async fn retry_tagging<F>(&self, atom_id: &str, on_event: F) -> Result<(), AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        // Verify atom exists
        self.storage
            .get_atom_content_impl(atom_id)
            .await?
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Atom {} not found", atom_id)))?;
        let status = self.storage.get_tagging_status_impl(atom_id).await?;
        if status == "processing" {
            return Err(AtomicCoreError::Validation(format!(
                "Atom {} is already being tagged",
                atom_id
            )));
        }
        // Reset tagging status to pending
        self.storage
            .set_tagging_status_sync(atom_id, "pending", None)
            .await?;

        let mut bg_settings = match self.settings_for_background().await {
            Some(settings) => settings,
            None => self.storage.get_all_settings_sync().await?,
        };
        bg_settings.insert("auto_tagging_enabled".to_string(), "true".to_string());
        let on_event = self.wrap_event_for_cache(on_event);
        let job = AtomPipelineJobRequest {
            atom_id: atom_id.to_string(),
            embed_requested: false,
            tag_requested: true,
            not_before: None,
            reason: "retry_tagging".to_string(),
            replace_existing: false,
        };
        self.storage.enqueue_pipeline_jobs_sync(&[job]).await?;
        embedding::process_queued_pipeline_jobs_with_settings(
            self.storage.clone(),
            on_event,
            bg_settings,
            Some(self.canvas_cache.clone()),
        )
        .await
        .map_err(AtomicCoreError::Embedding)?;

        Ok(())
    }

    /// Retry every atom whose embedding stage is currently failed in this database.
    pub async fn retry_failed_embeddings<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let count = self.storage.reset_failed_embedding_statuses_sync().await?;
        if count > 0 {
            self.process_pending_embeddings(on_event).await?;
        }
        Ok(count)
    }

    /// Retry every atom whose tagging stage is currently failed and whose
    /// embeddings are already complete in this database.
    pub async fn retry_failed_tagging<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let count = self.storage.reset_failed_tagging_statuses_sync().await?;
        if count > 0 {
            self.process_pending_tagging(on_event).await?;
        }
        Ok(count)
    }

    // ==================== Clustering ====================

    /// Compute atom clusters based on semantic similarity
    pub async fn compute_clusters(
        &self,
        min_similarity: f32,
        min_cluster_size: i32,
    ) -> Result<Vec<AtomCluster>, AtomicCoreError> {
        self.storage
            .compute_clusters_sync(min_similarity, min_cluster_size)
            .await
    }

    /// Save cluster assignments to the database
    pub async fn save_clusters(&self, clusters: &[AtomCluster]) -> Result<(), AtomicCoreError> {
        self.storage.save_clusters_sync(clusters).await
    }

    /// Get connection counts for hub identification
    pub async fn get_connection_counts(
        &self,
        min_similarity: f32,
    ) -> Result<std::collections::HashMap<String, i32>, AtomicCoreError> {
        self.storage
            .get_connection_counts_sync(min_similarity)
            .await
    }

    // ==================== Compaction ====================

    /// Get all tags formatted for LLM analysis
    pub async fn get_tags_for_compaction(&self) -> Result<String, AtomicCoreError> {
        self.storage.get_tags_for_compaction_impl().await
    }

    /// Apply tag merge operations
    pub async fn apply_tag_merges(
        &self,
        merges: &[compaction::TagMerge],
    ) -> Result<compaction::CompactionResult, AtomicCoreError> {
        let result = self.storage.apply_tag_merges_impl(merges).await?;
        self.canvas_cache.invalidate();
        Ok(result)
    }

    // ==================== Chat Operations ====================

    /// Create a new conversation
    pub async fn create_conversation(
        &self,
        tag_ids: &[String],
        title: Option<&str>,
    ) -> Result<ConversationWithTags, AtomicCoreError> {
        self.storage.create_conversation_sync(tag_ids, title).await
    }

    /// Get all conversations, optionally filtered by tag
    pub async fn get_conversations(
        &self,
        filter_tag_id: Option<&str>,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<ConversationWithTags>, AtomicCoreError> {
        self.storage
            .get_conversations_sync(filter_tag_id, limit, offset)
            .await
    }

    /// Get a single conversation with all messages
    pub async fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationWithMessages>, AtomicCoreError> {
        self.storage.get_conversation_sync(conversation_id).await
    }

    /// Update a conversation (title, archive status)
    pub async fn update_conversation(
        &self,
        id: &str,
        title: Option<&str>,
        is_archived: Option<bool>,
    ) -> Result<Conversation, AtomicCoreError> {
        self.storage
            .update_conversation_sync(id, title, is_archived)
            .await
    }

    /// Delete a conversation
    pub async fn delete_conversation(&self, id: &str) -> Result<(), AtomicCoreError> {
        self.storage.delete_conversation_sync(id).await
    }

    /// Set conversation scope (replace all tags)
    pub async fn set_conversation_scope(
        &self,
        conversation_id: &str,
        tag_ids: &[String],
    ) -> Result<ConversationWithTags, AtomicCoreError> {
        self.storage
            .set_conversation_scope_sync(conversation_id, tag_ids)
            .await
    }

    /// Add a single tag to conversation scope
    pub async fn add_tag_to_scope(
        &self,
        conversation_id: &str,
        tag_id: &str,
    ) -> Result<ConversationWithTags, AtomicCoreError> {
        self.storage
            .add_tag_to_scope_sync(conversation_id, tag_id)
            .await
    }

    /// Remove a single tag from conversation scope
    pub async fn remove_tag_from_scope(
        &self,
        conversation_id: &str,
        tag_id: &str,
    ) -> Result<ConversationWithTags, AtomicCoreError> {
        self.storage
            .remove_tag_from_scope_sync(conversation_id, tag_id)
            .await
    }

    /// Send a chat message and run the agent loop.
    ///
    /// The `on_event` callback receives streaming deltas, tool call events,
    /// and completion/error events during the agent loop.
    pub async fn send_chat_message<F>(
        &self,
        conversation_id: &str,
        content: &str,
        on_event: F,
    ) -> Result<ChatMessageWithContext, AtomicCoreError>
    where
        F: Fn(ChatEvent) + Send + Sync + 'static,
    {
        agent::send_chat_message_with_settings(
            self.storage.clone(),
            conversation_id,
            content,
            on_event,
            self.settings_for_background().await,
        )
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e))
    }

    /// Send a chat message with optional UI context for page-aware and canvas-aware tools.
    pub async fn send_chat_message_with_canvas<F>(
        &self,
        conversation_id: &str,
        content: &str,
        on_event: F,
        canvas_context: Option<CanvasContext>,
        page_context: Option<PageContext>,
    ) -> Result<ChatMessageWithContext, AtomicCoreError>
    where
        F: Fn(ChatEvent) + Send + Sync + 'static,
    {
        agent::send_chat_message_with_canvas(
            self.storage.clone(),
            conversation_id,
            content,
            on_event,
            self.settings_for_background().await,
            canvas_context,
            page_context,
            Some(self.canvas_cache.clone()),
        )
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e))
    }

    // ==================== Canvas Operations ====================

    /// Get all stored atom positions
    pub async fn get_atom_positions(&self) -> Result<Vec<AtomPosition>, AtomicCoreError> {
        self.storage.get_atom_positions_impl().await
    }

    /// Bulk save/update atom positions after simulation completes
    pub async fn save_atom_positions(
        &self,
        positions: &[AtomPosition],
    ) -> Result<(), AtomicCoreError> {
        self.storage.save_atom_positions_impl(positions).await
    }

    /// Get atoms with their average embedding vector for similarity calculations
    pub async fn get_atoms_with_embeddings(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> Result<Vec<AtomWithEmbedding>, AtomicCoreError> {
        self.storage.get_atoms_with_embeddings_impl(kinds).await
    }

    /// Return a handle to the canvas cache so background tasks (e.g. embedding
    /// pipeline completion) can invalidate it from outside this facade.
    pub fn canvas_cache(&self) -> CanvasCache {
        self.canvas_cache.clone()
    }

    /// Invalidate the in-memory canvas cache. Called by every mutation path
    /// that could change canvas output.
    pub fn invalidate_canvas_cache(&self) {
        self.canvas_cache.invalidate();
    }

    /// Wrap a user-provided embedding/tagging event callback so that
    /// visibility-changing events (`EmbeddingComplete`, `TaggingComplete`)
    /// schedule a debounced canvas cache rebuild. The returned closure is
    /// `Clone` so it can be passed to both single-atom and batch spawn sites.
    /// Debounced (not eager) so streaming batches collapse into one rebuild.
    fn wrap_event_for_cache<F>(
        &self,
        on_event: F,
    ) -> impl Fn(EmbeddingEvent) + Send + Sync + Clone + 'static
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let cache = self.canvas_cache.clone();
        let on_event = Arc::new(on_event);
        move |event: EmbeddingEvent| {
            if matches!(
                &event,
                EmbeddingEvent::EmbeddingComplete { .. } | EmbeddingEvent::TaggingComplete { .. }
            ) {
                cache.invalidate_debounced();
            }
            on_event(event);
        }
    }

    /// Compute PCA 2D projection of all atom embeddings and return positioned atoms,
    /// top-K edges per atom, and cluster centroid labels.
    /// Pure read operation — does not persist positions to the database.
    /// Works with both SQLite and Postgres backends via storage dispatch.
    ///
    /// Results are memoized via `canvas_cache`; subsequent calls return the
    /// cached `Arc` until a mutation invalidates it. Cold-cache rebuilds are
    /// serialized by a compute guard so N simultaneous misses collapse into
    /// one compute (the first waiter runs it; the rest re-read the cache).
    pub async fn compute_and_get_canvas_data(
        &self,
    ) -> Result<Arc<GlobalCanvasData>, AtomicCoreError> {
        if let Some(cached) = self.canvas_cache.get() {
            return Ok(cached);
        }
        // Serialize the first compute so concurrent misses don't all pay the
        // full PCA + edge-load cost. Double-checked after acquiring the
        // guard — if another waiter already populated the cache while we
        // blocked, use theirs.
        let _guard = self.canvas_cache.compute_guard().await;
        if let Some(cached) = self.canvas_cache.get() {
            return Ok(cached);
        }
        let data = Self::compute_canvas_data_impl(&self.storage).await?;
        self.canvas_cache.set(Arc::clone(&data));
        Ok(data)
    }

    /// The pure compute path for the global canvas payload. No cache
    /// interaction — callers decide whether to read/write the cache. Takes
    /// `&StorageBackend` instead of `&self` so the debounced rebuilder
    /// closure (registered on `CanvasCache`) can invoke it without needing
    /// a full `AtomicCore` handle.
    async fn compute_canvas_data_impl(
        storage: &storage::StorageBackend,
    ) -> Result<Arc<GlobalCanvasData>, AtomicCoreError> {
        // Load all average embeddings via storage abstraction (single scan of atom_chunks)
        let embeddings = storage.get_all_embedding_pairs_sync().await?;
        if embeddings.is_empty() {
            return Ok(Arc::new(GlobalCanvasData {
                atoms: vec![],
                edges: vec![],
                clusters: vec![],
            }));
        }

        // Run PCA projection (pure math, backend-agnostic)
        let projected = projection::compute_2d_projection(&embeddings);

        // Build position lookup
        let position_map: std::collections::HashMap<String, (f64, f64)> = projected
            .iter()
            .map(|(id, x, y)| (id.clone(), (*x, *y)))
            .collect();

        // Load lightweight canvas metadata (id, title, first tag, tag count, tag_ids)
        // — no full content, no embedding blobs, single query with LEFT JOIN.
        // Canvas is a display surface; show every kind, including findings
        // once reports exist, so the user can see their full knowledge graph.
        let atom_metadata = storage
            .get_canvas_atom_metadata_light_sync(&crate::models::KindFilter::All)
            .await?;
        let mut atom_tag_map = storage.get_all_atom_tag_ids_sync().await?;

        let atoms: Vec<CanvasAtomPosition> = atom_metadata
            .into_iter()
            .filter_map(|(atom_id, title, primary_tag, tag_count, source_url)| {
                let (x, y) = position_map.get(&atom_id)?;
                let tag_ids = atom_tag_map.remove(&atom_id).unwrap_or_default();
                Some(CanvasAtomPosition {
                    atom_id,
                    x: *x,
                    y: *y,
                    title,
                    primary_tag,
                    tag_count,
                    tag_ids,
                    source_url,
                })
            })
            .collect();

        // Load semantic edges once, use for both canvas edges and clustering
        let all_edges = storage.get_semantic_edges_raw_sync(0.5).await?;

        // Build top-k canvas edges from the loaded data
        let edges = Self::filter_top_k_edges(&all_edges, 2);

        // Compute clusters from the same edge data (no second DB scan)
        let cluster_data = clustering::compute_clusters_from_edges(&all_edges, 3);
        // Enrich clusters with dominant tag names
        let cluster_data = storage.enrich_clusters_with_tags_sync(cluster_data).await?;
        let clusters = Self::build_cluster_centroids(&cluster_data, &position_map);

        Ok(Arc::new(GlobalCanvasData {
            atoms,
            edges,
            clusters,
        }))
    }

    /// Register the canvas cache rebuilder so background-debounced
    /// invalidations can recompute the payload. Called once per constructor.
    /// Captures a `StorageBackend` clone — no reference cycle.
    fn register_canvas_rebuilder(&self) {
        let storage = self.storage.clone();
        self.canvas_cache.set_rebuilder(Box::new(move || {
            // The rebuilder is invoked inside `tokio::task::spawn_blocking`
            // (see `CanvasCache::invalidate_debounced`), so blocking on the
            // current runtime handle is safe and does not starve the
            // executor. We bridge from the sync rebuilder signature to the
            // async compute path here.
            tokio::runtime::Handle::current().block_on(Self::compute_canvas_data_impl(&storage))
        }));
    }

    /// Filter edges to keep at most top_k per atom, input must be sorted by score DESC.
    fn filter_top_k_edges(
        all_edges: &[(String, String, f32)],
        top_k: usize,
    ) -> Vec<CanvasEdgeData> {
        let mut per_atom: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut kept: Vec<(&str, &str, f32)> = Vec::new();

        for (src, tgt, score) in all_edges {
            let src_count = per_atom.get(src.as_str()).copied().unwrap_or(0);
            let tgt_count = per_atom.get(tgt.as_str()).copied().unwrap_or(0);
            if src_count >= top_k && tgt_count >= top_k {
                continue;
            }
            *per_atom.entry(src.as_str()).or_insert(0) += 1;
            *per_atom.entry(tgt.as_str()).or_insert(0) += 1;
            kept.push((src.as_str(), tgt.as_str(), *score));
        }

        let min_w = kept.iter().map(|(_, _, w)| *w).fold(f32::MAX, f32::min);
        let max_w = kept.iter().map(|(_, _, w)| *w).fold(f32::MIN, f32::max);
        let range = (max_w - min_w).max(0.001);

        kept.into_iter()
            .map(|(src, tgt, score)| CanvasEdgeData {
                source: src.to_string(),
                target: tgt.to_string(),
                weight: (score - min_w) / range,
            })
            .collect()
    }

    /// Build cluster centroid labels from cluster data and position map (pure math).
    fn build_cluster_centroids(
        clusters: &[AtomCluster],
        position_map: &std::collections::HashMap<String, (f64, f64)>,
    ) -> Vec<CanvasClusterLabel> {
        let mut labels = Vec::new();
        for cluster in clusters {
            let mut cx = 0.0f64;
            let mut cy = 0.0f64;
            let mut count = 0;
            for aid in &cluster.atom_ids {
                if let Some(&(x, y)) = position_map.get(aid) {
                    cx += x;
                    cy += y;
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            cx /= count as f64;
            cy /= count as f64;

            let label = if cluster.dominant_tags.len() >= 2 {
                format!("{}, {}", cluster.dominant_tags[0], cluster.dominant_tags[1])
            } else if !cluster.dominant_tags.is_empty() {
                cluster.dominant_tags[0].clone()
            } else {
                format!("Cluster {}", cluster.cluster_id + 1)
            };

            labels.push(CanvasClusterLabel {
                id: format!("cluster:{}", cluster.cluster_id),
                x: cx,
                y: cy,
                label,
                atom_count: cluster.atom_ids.len() as i32,
                atom_ids: cluster.atom_ids.clone(),
            });
        }
        labels
    }

    // ==================== Semantic Graph Operations ====================

    /// Get semantic edges above a minimum similarity threshold (capped at 10k for safety)
    pub async fn get_semantic_edges(
        &self,
        min_similarity: f32,
    ) -> Result<Vec<SemanticEdge>, AtomicCoreError> {
        self.storage.get_semantic_edges_sync(min_similarity).await
    }

    /// Get neighborhood graph for an atom (for local graph view)
    pub async fn get_atom_neighborhood(
        &self,
        atom_id: &str,
        depth: i32,
        min_similarity: f32,
    ) -> Result<NeighborhoodGraph, AtomicCoreError> {
        self.storage
            .get_atom_neighborhood_sync(atom_id, depth, min_similarity)
            .await
    }

    /// Rebuild semantic edges for all atoms with embeddings.
    ///
    /// Returns the number of atoms **queued** for edge recomputation, not
    /// the number of edges written. This call returns as soon as the edge
    /// pipeline is spawned — actual edge computation runs in the background
    /// and completes asynchronously. Callers watching for completion should
    /// subscribe to pipeline events rather than treating the return value
    /// as a completion signal.
    pub async fn rebuild_semantic_edges(&self) -> Result<i32, AtomicCoreError> {
        let count = self.storage.rebuild_semantic_edges_sync().await?;
        self.canvas_cache.invalidate();
        if count > 0 {
            // Kick off the background edge pipeline with the cache handle so
            // each completed batch invalidates the cache as edges land on disk.
            embedding::process_pending_edges(self.storage.clone(), Some(self.canvas_cache.clone()))
                .await
                .map_err(|e| AtomicCoreError::Embedding(e))?;
        }
        Ok(count)
    }

    // ==================== Hierarchical Canvas ====================

    /// Get a single level of the hierarchical canvas view.
    ///
    /// - `parent_id = None`: root level showing tag categories
    /// - `parent_id = Some(tag_id)`: children of that tag (sub-tags or atoms)
    /// - `children_hint`: for SemanticCluster drill-down, the list of child IDs to display
    pub async fn get_canvas_level(
        &self,
        parent_id: Option<&str>,
        children_hint: Option<Vec<String>>,
    ) -> Result<CanvasLevel, AtomicCoreError> {
        self.storage
            .get_canvas_level_sync(parent_id, children_hint)
            .await
    }

    // ==================== Embedding Status ====================

    /// Get the embedding status for a specific atom
    pub async fn get_embedding_status(&self, atom_id: &str) -> Result<String, AtomicCoreError> {
        self.storage.get_embedding_status_impl(atom_id).await
    }

    /// Get pipeline status (embedding counts + failed atoms)
    pub async fn get_pipeline_status(&self) -> Result<models::PipelineStatus, AtomicCoreError> {
        self.storage.get_pipeline_status().await
    }

    /// Process pending tag extraction for atoms with complete embeddings
    pub async fn process_pending_tagging<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        self.storage
            .enqueue_pipeline_jobs_from_statuses_sync(None)
            .await?;
        self.process_queued_pipeline_jobs(on_event).await
    }

    /// Process pending tagging only for atoms last updated at or before `cutoff`.
    pub async fn process_pending_tagging_due<F>(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
        on_event: F,
    ) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let cutoff_rfc3339 = cutoff.to_rfc3339();
        self.storage
            .enqueue_pipeline_jobs_from_statuses_sync(Some(&cutoff_rfc3339))
            .await?;
        let count = self.process_queued_pipeline_jobs(on_event).await?;
        if count > 0 {
            tracing::info!(cutoff = %cutoff_rfc3339, count, "Queued pending pipeline jobs due for processing");
        }
        Ok(count)
    }

    /// Process a single atom's pipeline immediately using the latest persisted
    /// content. Intended for editor dismiss/finalize after draft autosaves.
    pub async fn process_atom_pipeline<F>(
        &self,
        atom_id: &str,
        on_event: F,
    ) -> Result<(), AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let content = self
            .storage
            .get_atom_content_impl(atom_id)
            .await?
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Atom not found: {}", atom_id)))?;

        self.storage
            .set_embedding_status_sync(atom_id, "pending", None)
            .await?;
        self.storage
            .set_tagging_status_sync(atom_id, "pending", None)
            .await?;
        tracing::info!(
            atom_id = %atom_id,
            has_content = !content.trim().is_empty(),
            "Explicit atom pipeline processing requested"
        );

        if content.trim().is_empty() {
            tracing::info!(atom_id = %atom_id, "Explicit atom pipeline request skipped because content is empty");
            return Ok(());
        }

        let job = AtomPipelineJobRequest {
            atom_id: atom_id.to_string(),
            embed_requested: true,
            tag_requested: true,
            not_before: None,
            reason: "process_atom_pipeline".to_string(),
            replace_existing: false,
        };
        self.storage.enqueue_pipeline_jobs_sync(&[job]).await?;
        self.process_queued_pipeline_jobs(on_event).await?;

        Ok(())
    }

    // ==================== Cluster Cache ====================

    /// Get cached clusters, computing if missing
    pub async fn get_clusters(&self) -> Result<Vec<AtomCluster>, AtomicCoreError> {
        self.storage.get_clusters_sync().await
    }

    // ==================== Settings with Re-embed ====================

    /// Set a setting, handling embedding-space changes.
    /// Embedding model/provider changes require re-embedding even when the
    /// vector dimension stays the same: equal dimensions do not imply the same
    /// vector space. Existing chunk rows are preserved by the embed-only queue.
    /// Failed atoms are auto-retried for non-space provider config changes
    /// such as API keys or base URLs.
    pub async fn set_setting_with_reembed<F>(
        &self,
        key: &str,
        value: &str,
        on_event: F,
    ) -> Result<SettingChangeResult, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let current_settings = self.get_settings().await?;
        let value_changed = current_settings.get(key).map(|s| s.as_str()) != Some(value);

        let mut embedding_space_changed = false;
        let mut dimension_changed = false;
        let mut old_dim = 0usize;
        let mut new_dim = 0usize;

        if settings::is_embedding_space_key(key) && value_changed {
            embedding_space_changed = true;
            let current_config = ProviderConfig::from_settings(&current_settings);
            old_dim = current_config.embedding_dimension();

            let mut new_settings = current_settings.clone();
            new_settings.insert(key.to_string(), value.to_string());
            let new_config = ProviderConfig::from_settings(&new_settings);
            new_dim = new_config.embedding_dimension();

            if old_dim != new_dim {
                tracing::info!(
                    old_dim,
                    new_dim,
                    key,
                    "Embedding dimension change detected — recreating vector index and re-embedding all atoms"
                );
                dimension_changed = true;
            }
        }

        // Route through the standard resolver: workspace-only keys land in
        // registry, overridable keys land in registry while N≤1 and per-DB
        // (override for the active DB) when N>1. Re-embedding below targets
        // only the active DB, which matches that routing — when an override
        // creates divergence, only the changed DB needs re-embedding.
        self.set_setting(key, value).await?;

        let mut queued_reembedding = 0i32;
        if dimension_changed {
            // Recreate the active database's vector index at the new dimension.
            // This clears old vectors, preserves chunk content, clears semantic
            // edges/tag centroids, and resets every atom's embedding_status to
            // 'pending'.
            self.storage.recreate_vector_index_sync(new_dim).await?;
            self.canvas_cache.invalidate();
            tracing::info!(
                new_dim,
                "Recreated active database vector index for dimension change"
            );
            // Now spawn re-embedding — atoms are in 'pending' status after the reset.
            queued_reembedding = self.spawn_reembed_pending(on_event.clone()).await?;
            tracing::info!(
                queued_reembedding,
                "Queued atoms for re-embedding after dimension change"
            );
        } else if embedding_space_changed {
            queued_reembedding = self.reembed_all_atoms(on_event.clone()).await?;
            tracing::info!(
                queued_reembedding,
                key,
                "Queued atoms for re-embedding after embedding-space setting change"
            );
        }

        // Auto-retry failed atoms when provider config changes
        // (covers: URL, API key, model, provider type)
        let retry_keys = [
            "provider",
            "embedding_model",
            "ollama_embedding_model",
            "ollama_host",
            "openai_compat_embedding_model",
            "openai_compat_base_url",
            "openai_compat_api_key",
            "openrouter_api_key",
        ];
        let mut retried_failed = 0i32;
        if retry_keys.contains(&key) && !embedding_space_changed && value_changed {
            retried_failed = self.storage.reset_failed_embeddings_sync().await?;
            if retried_failed > 0 {
                tracing::info!(
                    retried_failed,
                    key,
                    "Provider config updated — retrying previously failed atoms"
                );
                let _ = self.process_pending_embeddings(on_event.clone()).await;
                let _ = self.process_pending_tagging(on_event).await;
            }
        }

        Ok(SettingChangeResult {
            embedding_space_changed,
            dimension_changed,
            old_dim,
            new_dim,
            total_atom_count: queued_reembedding,
            retried_failed_count: retried_failed,
        })
    }

    /// Clear a per-DB override, handling embedding-space changes.
    ///
    /// Clearing an embedding-space override can change the active database's
    /// resolved vector space just like setting one can. Dimension changes
    /// recreate the active DB's vector index before queueing pending atoms;
    /// same-dimension space changes re-embed all atoms so stale vectors are
    /// not left behind.
    pub async fn clear_override_with_reembed<F>(
        &self,
        key: &str,
        on_event: F,
    ) -> Result<SettingChangeResult, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let current_settings = self.get_settings().await?;
        let current_config = ProviderConfig::from_settings(&current_settings);
        let current_value = current_settings.get(key).cloned();

        self.clear_override(key).await?;

        let new_settings = self.get_settings().await?;
        let new_config = ProviderConfig::from_settings(&new_settings);
        let new_value = new_settings.get(key).cloned();
        let value_changed = current_value != new_value;

        let mut embedding_space_changed = false;
        let mut dimension_changed = false;
        let mut old_dim = 0usize;
        let mut new_dim = 0usize;
        let mut queued_reembedding = 0i32;

        if settings::is_embedding_space_key(key) && value_changed {
            embedding_space_changed = true;
            old_dim = current_config.embedding_dimension();
            new_dim = new_config.embedding_dimension();

            if old_dim != new_dim {
                dimension_changed = true;
                tracing::info!(
                    old_dim,
                    new_dim,
                    key,
                    "Embedding dimension change detected after clearing override"
                );
                self.storage.recreate_vector_index_sync(new_dim).await?;
                self.canvas_cache.invalidate();
                queued_reembedding = self.spawn_reembed_pending(on_event.clone()).await?;
                tracing::info!(
                    queued_reembedding,
                    "Queued atoms for re-embedding after clearing dimension override"
                );
            } else {
                queued_reembedding = self.reembed_all_atoms(on_event.clone()).await?;
                tracing::info!(
                    queued_reembedding,
                    key,
                    "Queued atoms for re-embedding after clearing embedding-space override"
                );
            }
        }

        Ok(SettingChangeResult {
            embedding_space_changed,
            dimension_changed,
            old_dim,
            new_dim,
            total_atom_count: queued_reembedding,
            retried_failed_count: 0,
        })
    }

    // ==================== Utility Operations ====================

    /// Check sqlite-vec version
    pub async fn check_sqlite_vec(&self) -> Result<String, AtomicCoreError> {
        self.storage.check_vector_extension_sync().await
    }

    /// Verify that the current provider is properly configured
    pub async fn verify_provider_configured(&self) -> Result<bool, AtomicCoreError> {
        let settings_map = self.get_settings().await?;
        let config = ProviderConfig::from_settings(&settings_map);

        match config.provider_type {
            ProviderType::OpenRouter => Ok(config
                .openrouter_api_key
                .as_ref()
                .map_or(false, |k| !k.is_empty())),
            ProviderType::Ollama => Ok(!config.ollama_host.is_empty()),
            ProviderType::OpenAICompat => Ok(!config.openai_compat_base_url.is_empty()),
        }
    }

    /// Get all wiki articles (summaries for list view)
    pub async fn get_all_wiki_articles(&self) -> Result<Vec<WikiArticleSummary>, AtomicCoreError> {
        self.storage.get_all_wiki_articles_sync().await
    }

    /// Get cached model capabilities from the settings table.
    pub async fn get_cached_capabilities(
        &self,
    ) -> Result<Option<providers::models::ModelCapabilitiesCache>, AtomicCoreError> {
        let json = self
            .storage
            .get_setting_sync("model_capabilities_cache")
            .await?;
        match json {
            Some(j) => {
                let cache: providers::models::ModelCapabilitiesCache = serde_json::from_str(&j)
                    .map_err(|e| {
                        AtomicCoreError::Configuration(format!(
                            "Failed to parse capabilities cache: {}",
                            e
                        ))
                    })?;
                Ok(Some(cache))
            }
            None => Ok(None),
        }
    }

    /// Save model capabilities cache to the settings table.
    pub async fn save_capabilities_cache(
        &self,
        cache: &providers::models::ModelCapabilitiesCache,
    ) -> Result<(), AtomicCoreError> {
        let json = serde_json::to_string(cache).map_err(|e| {
            AtomicCoreError::Configuration(format!("Failed to serialize capabilities cache: {}", e))
        })?;
        self.storage
            .set_setting_sync("model_capabilities_cache", &json)
            .await
    }

    // ==================== Import Operations ====================

    /// Import an Obsidian vault into the knowledge base.
    ///
    /// Discovers markdown files, parses notes, creates atoms with hierarchical tags,
    /// and triggers embedding generation. Progress is reported via `on_progress` and
    /// embedding events via `on_event`.
    pub async fn import_obsidian_vault<F, P>(
        &self,
        vault_path: &str,
        max_notes: Option<i32>,
        on_event: F,
        on_progress: P,
    ) -> Result<ImportResult, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
        P: Fn(ImportProgress),
    {
        let vault_path = std::path::Path::new(vault_path);

        if !vault_path.exists() {
            return Err(AtomicCoreError::Validation(format!(
                "Vault not found at {:?}",
                vault_path
            )));
        }

        let vault_name = vault_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Vault".to_string());

        let exclude_patterns: Vec<&str> = import::obsidian::DEFAULT_EXCLUDES.to_vec();
        let mut note_files = import::obsidian::discover_notes(vault_path, &exclude_patterns)
            .map_err(|e| AtomicCoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        if note_files.is_empty() {
            return Ok(ImportResult {
                imported: 0,
                skipped: 0,
                errors: 0,
                tags_created: 0,
                tags_linked: 0,
            });
        }

        if let Some(max) = max_notes {
            note_files.truncate(max as usize);
        }

        let total = note_files.len() as i32;
        let mut stats = ImportResult {
            imported: 0,
            skipped: 0,
            errors: 0,
            tags_created: 0,
            tags_linked: 0,
        };

        let mut tag_cache: HashMap<(String, Option<String>), String> = HashMap::new();
        let mut imported_atoms: Vec<(String, String)> = Vec::new();

        for (index, file_path) in note_files.iter().enumerate() {
            let relative_path = file_path.strip_prefix(vault_path).unwrap_or(file_path);
            let relative_str = relative_path.to_string_lossy().to_string();

            let note = match import::obsidian::parse_obsidian_note(
                file_path,
                relative_path,
                &vault_name,
            ) {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(file = %relative_str, error = %e, "Error parsing file");
                    stats.errors += 1;
                    on_progress(ImportProgress {
                        current: index as i32 + 1,
                        total,
                        current_file: relative_str,
                        status: "error".to_string(),
                    });
                    continue;
                }
            };

            if note.content.trim().len() < 10 {
                stats.skipped += 1;
                on_progress(ImportProgress {
                    current: index as i32 + 1,
                    total,
                    current_file: relative_str,
                    status: "skipped".to_string(),
                });
                continue;
            }

            // Check for duplicate by source_url
            if self
                .storage
                .source_url_exists_sync(&note.source_url)
                .await?
            {
                stats.skipped += 1;
                on_progress(ImportProgress {
                    current: index as i32 + 1,
                    total,
                    current_file: relative_str,
                    status: "skipped".to_string(),
                });
                continue;
            }

            let atom_id = Uuid::new_v4().to_string();

            // Use insert_atom_impl for the atom insert
            match self
                .storage
                .insert_atom_impl(
                    &atom_id,
                    &CreateAtomRequest {
                        content: note.content.clone(),
                        source_url: Some(note.source_url.clone()),
                        published_at: None,
                        tag_ids: vec![],
                        ..Default::default()
                    },
                    &note.created_at,
                )
                .await
            {
                Ok(_) => {
                    imported_atoms.push((atom_id.clone(), note.content.clone()));
                }
                Err(e) => {
                    tracing::error!(file = %relative_str, error = %e, "Error inserting atom");
                    stats.errors += 1;
                    on_progress(ImportProgress {
                        current: index as i32 + 1,
                        total,
                        current_file: relative_str,
                        status: "error".to_string(),
                    });
                    continue;
                }
            }

            // Process hierarchical folder tags using the raw conn helper
            // (get_or_create_tag uses parent_id, which the trait method doesn't support directly)
            let sqlite = self.storage.as_sqlite().ok_or_else(|| {
                AtomicCoreError::Configuration(
                    "Obsidian import is not yet supported with Postgres backend".to_string(),
                )
            })?;
            let conn = sqlite
                .db
                .conn
                .lock()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            let mut folder_tag_ids: Vec<String> = Vec::new();
            for htag in &note.folder_tags {
                let parent_id = if htag.parent_path.is_empty() {
                    None
                } else {
                    let parent_index = htag.parent_path.len() - 1;
                    folder_tag_ids.get(parent_index).map(|s| s.as_str())
                };

                if let Some(tag_id) =
                    get_or_create_tag(&conn, &mut tag_cache, &htag.name, parent_id, &mut stats)
                {
                    folder_tag_ids.push(tag_id.clone());
                    if let Err(e) = conn.execute(
                        "INSERT OR IGNORE INTO atom_tags (atom_id, tag_id, source) VALUES (?1, ?2, 'manual')",
                        rusqlite::params![&atom_id, &tag_id],
                    ) {
                        tracing::error!(tag_name = %htag.name, error = %e, "Error linking folder tag to atom");
                        continue;
                    }
                    stats.tags_linked += 1;
                }
            }

            // Process flat frontmatter tags
            for tag_name in &note.frontmatter_tags {
                if let Some(tag_id) =
                    get_or_create_tag(&conn, &mut tag_cache, tag_name, None, &mut stats)
                {
                    if let Err(e) = conn.execute(
                        "INSERT OR IGNORE INTO atom_tags (atom_id, tag_id, source) VALUES (?1, ?2, 'manual')",
                        rusqlite::params![&atom_id, &tag_id],
                    ) {
                        tracing::error!(tag_name = %tag_name, error = %e, "Error linking tag to atom");
                        continue;
                    }
                    stats.tags_linked += 1;
                }
            }
            drop(conn);

            stats.imported += 1;
            on_progress(ImportProgress {
                current: index as i32 + 1,
                total,
                current_file: relative_str,
                status: "importing".to_string(),
            });
        }

        // Trigger embedding processing for all imported atoms
        if !imported_atoms.is_empty() {
            let jobs: Vec<AtomPipelineJobRequest> = imported_atoms
                .iter()
                .map(|(atom_id, _)| AtomPipelineJobRequest {
                    atom_id: atom_id.clone(),
                    embed_requested: true,
                    tag_requested: true,
                    not_before: None,
                    reason: "import_markdown".to_string(),
                    replace_existing: false,
                })
                .collect();
            self.canvas_cache.invalidate();
            self.storage.enqueue_pipeline_jobs_sync(&jobs).await?;
            self.process_queued_pipeline_jobs(on_event).await?;
        }

        Ok(stats)
    }

    // ==================== Content Ingestion ====================

    /// Ingest a single URL: fetch, extract article, create atom, trigger embedding.
    /// Deduplicates by source_url. Returns an error if the URL was already ingested
    /// or if the page isn't article-shaped.
    pub async fn ingest_url<F, G>(
        &self,
        request: ingest::IngestionRequest,
        on_ingest: F,
        on_embed: G,
    ) -> Result<ingest::IngestionResult, AtomicCoreError>
    where
        F: Fn(ingest::IngestionEvent) + Send + Sync + 'static,
        G: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let request_id = Uuid::new_v4().to_string();

        // Dedup check
        if self.storage.source_url_exists_sync(&request.url).await? {
            return Err(AtomicCoreError::Validation(format!(
                "URL already ingested: {}",
                request.url
            )));
        }

        // Resolve: fetch + extract
        let resolved = ingest::resolve_url(&request.url, &request_id, &on_ingest)
            .await
            .map_err(|e| {
                on_ingest(ingest::IngestionEvent::IngestionFailed {
                    request_id: request_id.clone(),
                    url: request.url.clone(),
                    error: e.clone(),
                });
                AtomicCoreError::Ingestion(e)
            })?;

        let title = if let Some(hint) = &request.title_hint {
            if !hint.is_empty() {
                hint.clone()
            } else {
                resolved.title.clone()
            }
        } else {
            resolved.title.clone()
        };

        let content_length = resolved.markdown.len();

        // Create atom (this triggers embedding in background)
        let atom = self
            .create_atom(
                CreateAtomRequest {
                    content: resolved.markdown,
                    source_url: Some(request.url.clone()),
                    published_at: request.published_at,
                    tag_ids: request.tag_ids,
                    ..Default::default()
                },
                on_embed,
            )
            .await?
            .ok_or_else(|| {
                AtomicCoreError::Validation("Atom creation returned None".to_string())
            })?;

        let result = ingest::IngestionResult {
            atom_id: atom.atom.id.clone(),
            url: request.url.clone(),
            title: title.clone(),
            content_length,
        };

        on_ingest(ingest::IngestionEvent::IngestionComplete {
            request_id,
            atom_id: atom.atom.id,
            url: request.url,
            title,
        });

        Ok(result)
    }

    /// Ingest multiple URLs concurrently.
    /// Each URL is processed independently — individual failures don't affect others.
    pub async fn ingest_urls<F, G>(
        &self,
        requests: Vec<ingest::IngestionRequest>,
        on_ingest: F,
        on_embed: G,
    ) -> Vec<Result<ingest::IngestionResult, AtomicCoreError>>
    where
        F: Fn(ingest::IngestionEvent) + Send + Sync + Clone + 'static,
        G: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let mut handles = Vec::with_capacity(requests.len());

        for request in requests {
            let core = self.clone();
            let on_ingest = on_ingest.clone();
            let on_embed = on_embed.clone();
            handles.push(tokio::spawn(async move {
                core.ingest_url(request, on_ingest, on_embed).await
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => results.push(Err(AtomicCoreError::Ingestion(format!(
                    "Task join error: {}",
                    e
                )))),
            }
        }
        results
    }

    // ==================== Feed Management ====================

    /// Create a new RSS feed. Validates by fetching and parsing the feed URL.
    pub async fn create_feed<F, G>(
        &self,
        request: CreateFeedRequest,
        on_ingest: F,
        on_embed: G,
    ) -> Result<Feed, AtomicCoreError>
    where
        F: Fn(ingest::IngestionEvent) + Send + Sync + Clone + 'static,
        G: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        // Fetch feed data (XML/JSON) — use shared HTTP client with proper User-Agent
        let feed_data = ingest::fetch::fetch_bytes(&request.url)
            .await
            .map_err(|e| AtomicCoreError::Ingestion(format!("Cannot fetch feed: {}", e)))?;

        let parsed =
            ingest::rss::parse_feed(&feed_data).map_err(|e| AtomicCoreError::Ingestion(e))?;

        let feed = self
            .storage
            .create_feed_sync(
                &request.url,
                parsed.title.as_deref(),
                parsed.site_url.as_deref(),
                request.poll_interval,
                &request.tag_ids,
            )
            .await?;

        // Poll immediately after creation
        let core = self.clone();
        let feed_id = feed.id.clone();
        executor::spawn(async move {
            let _ = core.poll_feed(&feed_id, on_ingest, on_embed).await;
        });

        Ok(feed)
    }

    /// List all feeds.
    pub async fn list_feeds(&self) -> Result<Vec<Feed>, AtomicCoreError> {
        self.storage.list_feeds_sync().await
    }

    /// Get a single feed by ID.
    pub async fn get_feed(&self, id: &str) -> Result<Feed, AtomicCoreError> {
        self.storage.get_feed_sync(id).await
    }

    /// Update a feed's settings.
    pub async fn update_feed(
        &self,
        id: &str,
        request: UpdateFeedRequest,
    ) -> Result<Feed, AtomicCoreError> {
        self.storage
            .update_feed_sync(
                id,
                None, // title not in UpdateFeedRequest
                request.poll_interval,
                request.is_paused,
                request.tag_ids.as_deref(),
            )
            .await
    }

    /// Delete a feed. Does NOT delete atoms created from this feed.
    pub async fn delete_feed(&self, id: &str) -> Result<(), AtomicCoreError> {
        self.storage.delete_feed_sync(id).await
    }

    /// Poll a single feed: fetch XML, parse, dedup via feed_items, ingest new articles.
    pub async fn poll_feed<F, G>(
        &self,
        feed_id: &str,
        on_ingest: F,
        on_embed: G,
    ) -> Result<ingest::FeedPollResult, AtomicCoreError>
    where
        F: Fn(ingest::IngestionEvent) + Send + Sync + Clone + 'static,
        G: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let feed = self.get_feed(feed_id).await?;

        // Fetch feed XML — use shared HTTP client with proper User-Agent
        let feed_data = match ingest::fetch::fetch_bytes(&feed.url).await {
            Ok(bytes) => bytes,
            Err(e) => {
                let err = format!("Cannot fetch feed: {}", e);
                self.update_feed_error(feed_id, &err).await;
                on_ingest(ingest::IngestionEvent::FeedPollFailed {
                    feed_id: feed_id.to_string(),
                    error: err.clone(),
                });
                return Err(AtomicCoreError::Ingestion(err));
            }
        };

        let parsed = match ingest::rss::parse_feed(&feed_data) {
            Ok(p) => p,
            Err(e) => {
                self.update_feed_error(feed_id, &e).await;
                on_ingest(ingest::IngestionEvent::FeedPollFailed {
                    feed_id: feed_id.to_string(),
                    error: e.clone(),
                });
                return Err(AtomicCoreError::Ingestion(e));
            }
        };

        let mut new_items = 0i32;
        let mut skipped = 0i32;
        let mut errors = 0i32;

        for item in &parsed.items {
            // Claim the GUID atomically — if another poll already claimed it, skip.
            if !self.claim_feed_item(feed_id, &item.guid).await? {
                continue;
            }

            let link = match &item.link {
                Some(l) => l.clone(),
                None => {
                    self.mark_feed_item_skipped(feed_id, &item.guid, "No link in feed item")
                        .await?;
                    skipped += 1;
                    continue;
                }
            };

            let request_id = Uuid::new_v4().to_string();
            match ingest::resolve_url(&link, &request_id, &on_ingest).await {
                Ok(resolved) => {
                    match self
                        .create_atom(
                            CreateAtomRequest {
                                content: resolved.markdown,
                                source_url: Some(link),
                                published_at: item.published_at.clone(),
                                tag_ids: feed.tag_ids.clone(),
                                skip_if_source_exists: true,
                            },
                            on_embed.clone(),
                        )
                        .await
                    {
                        Ok(Some(atom)) => {
                            self.complete_feed_item(feed_id, &item.guid, &atom.atom.id)
                                .await?;
                            new_items += 1;
                        }
                        Ok(None) => {
                            self.mark_feed_item_skipped(
                                feed_id,
                                &item.guid,
                                "duplicate source_url",
                            )
                            .await?;
                            skipped += 1;
                        }
                        Err(e) => {
                            self.mark_feed_item_skipped(feed_id, &item.guid, &e.to_string())
                                .await?;
                            errors += 1;
                        }
                    }
                }
                Err(reason) => {
                    self.mark_feed_item_skipped(feed_id, &item.guid, &reason)
                        .await?;
                    skipped += 1;
                }
            }
        }

        // Update feed metadata
        self.storage.mark_feed_polled_sync(feed_id, None).await?;
        // Backfill title/site_url from feed data if not already set
        if parsed.title.is_some() || parsed.site_url.is_some() {
            self.storage
                .backfill_feed_metadata_sync(
                    feed_id,
                    parsed.title.as_deref(),
                    parsed.site_url.as_deref(),
                )
                .await?;
        }

        let result = ingest::FeedPollResult {
            feed_id: feed_id.to_string(),
            new_items,
            skipped,
            errors,
        };

        on_ingest(ingest::IngestionEvent::FeedPollComplete {
            feed_id: feed_id.to_string(),
            new_items,
            skipped,
            errors,
        });

        Ok(result)
    }

    /// Poll all feeds that are due (not paused, enough time elapsed).
    pub async fn poll_due_feeds<F, G>(
        &self,
        on_ingest: F,
        on_embed: G,
    ) -> Vec<ingest::FeedPollResult>
    where
        F: Fn(ingest::IngestionEvent) + Send + Sync + Clone + 'static,
        G: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let due_feed_ids: Vec<String> = match self.storage.get_due_feeds_sync().await {
            Ok(feeds) => feeds.into_iter().map(|f| f.id).collect(),
            Err(_) => return vec![],
        };

        let mut results = Vec::new();
        for feed_id in due_feed_ids {
            match self
                .poll_feed(&feed_id, on_ingest.clone(), on_embed.clone())
                .await
            {
                Ok(r) => results.push(r),
                Err(e) => {
                    tracing::error!(feed_id = %feed_id, error = %e, "Feed poll failed");
                }
            }
        }
        results
    }

    /// Atomically claim a feed item GUID. Returns true if this call claimed it,
    /// false if it was already claimed by another poll.
    async fn claim_feed_item(&self, feed_id: &str, guid: &str) -> Result<bool, AtomicCoreError> {
        self.storage.claim_feed_item_sync(feed_id, guid).await
    }

    /// Mark a claimed feed item as successfully ingested with its atom_id.
    async fn complete_feed_item(
        &self,
        feed_id: &str,
        guid: &str,
        atom_id: &str,
    ) -> Result<(), AtomicCoreError> {
        self.storage
            .complete_feed_item_sync(feed_id, guid, atom_id)
            .await
    }

    /// Mark a claimed feed item as skipped with a reason.
    async fn mark_feed_item_skipped(
        &self,
        feed_id: &str,
        guid: &str,
        reason: &str,
    ) -> Result<(), AtomicCoreError> {
        self.storage
            .mark_feed_item_skipped_sync(feed_id, guid, reason)
            .await
    }

    /// Helper: update a feed's last_error field.
    async fn update_feed_error(&self, feed_id: &str, error: &str) {
        let _ = self
            .storage
            .mark_feed_polled_sync(feed_id, Some(error))
            .await;
    }

    /// List deterministic knowledge-quality signals for the active database.
    pub async fn list_knowledge_signals(
        &self,
        filter: KnowledgeSignalFilter,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
        knowledge_signals::list_knowledge_signals(self, filter).await
    }

    /// Dismiss a knowledge-quality signal until its identity changes or it is restored.
    pub async fn dismiss_knowledge_signal(&self, signal_key: &str) -> Result<(), AtomicCoreError> {
        knowledge_signals::dismiss_signal(self, signal_key).await
    }

    /// Snooze a knowledge-quality signal until an RFC3339 timestamp.
    pub async fn snooze_knowledge_signal(
        &self,
        signal_key: &str,
        until: &str,
    ) -> Result<(), AtomicCoreError> {
        knowledge_signals::snooze_signal(self, signal_key, until).await
    }

    /// Persist per-database configuration for a knowledge signal provider.
    pub async fn set_knowledge_signal_provider_config(
        &self,
        provider_id: &str,
        config: KnowledgeSignalProviderConfig,
    ) -> Result<KnowledgeSignalProviderConfig, AtomicCoreError> {
        knowledge_signals::set_provider_config(self, provider_id, config).await
    }

    /// Restore a dismissed or snoozed knowledge-quality signal.
    pub async fn restore_knowledge_signal(&self, signal_key: &str) -> Result<(), AtomicCoreError> {
        knowledge_signals::restore_signal(self, signal_key).await
    }

    /// Recompute centroid embeddings for all tags that have atoms with embeddings.
    /// Useful for backfilling after this feature is added to an existing database.
    pub async fn recompute_all_tag_embeddings(&self) -> Result<i32, AtomicCoreError> {
        self.storage.recompute_all_tag_embeddings_sync().await
    }
}

fn oauth_unavailable() -> AtomicCoreError {
    AtomicCoreError::Configuration(
        "OAuth is unavailable: no SQLite registry is attached and the storage backend does not support OAuth".to_string(),
    )
}

/// Helper to get or create a tag, using a cache to avoid duplicate lookups.
fn get_or_create_tag(
    conn: &rusqlite::Connection,
    tag_cache: &mut HashMap<(String, Option<String>), String>,
    name: &str,
    parent_id: Option<&str>,
    stats: &mut ImportResult,
) -> Option<String> {
    let cache_key = (name.to_lowercase(), parent_id.map(|s| s.to_string()));

    if let Some(cached_id) = tag_cache.get(&cache_key) {
        return Some(cached_id.clone());
    }

    let existing: Option<String> = if let Some(pid) = parent_id {
        conn.query_row(
            "SELECT id FROM tags WHERE LOWER(name) = LOWER(?1) AND parent_id = ?2 LIMIT 1",
            rusqlite::params![name, pid],
            |row| row.get(0),
        )
        .ok()
    } else {
        conn.query_row(
            "SELECT id FROM tags WHERE LOWER(name) = LOWER(?1) AND parent_id IS NULL LIMIT 1",
            [name],
            |row| row.get(0),
        )
        .ok()
    };

    let id = match existing {
        Some(id) => id,
        None => {
            let new_id = Uuid::new_v4().to_string();
            let now = Utc::now().to_rfc3339();
            if let Err(e) = conn.execute(
                "INSERT INTO tags (id, name, parent_id, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![&new_id, name, parent_id, &now],
            ) {
                tracing::error!(tag_name = %name, error = %e, "Error creating tag");
                return None;
            }
            stats.tags_created += 1;
            new_id
        }
    };

    tag_cache.insert(cache_key, id.clone());
    Some(id)
}

// ==================== Helper Functions ====================

/// Batch-load all average embeddings in a single query, returning a map from atom_id -> avg embedding.
/// This replaces 33K individual get_average_embedding() calls with one streaming query.
pub(crate) fn get_all_average_embeddings(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, Vec<f32>>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT atom_id, embedding FROM atom_chunks WHERE embedding IS NOT NULL ORDER BY atom_id",
    )?;

    let mut map: std::collections::HashMap<String, Vec<f32>> = std::collections::HashMap::new();
    let mut current_atom_id: Option<String> = None;
    let mut current_sum: Vec<f32> = Vec::new();
    let mut current_count: f32 = 0.0;

    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;

    for row in rows {
        let (atom_id, blob) = row?;
        let dim = blob.len() / 4;
        if dim == 0 {
            continue;
        }

        if current_atom_id.as_deref() != Some(&atom_id) {
            // Flush previous atom's average
            if let Some(prev_id) = current_atom_id.take() {
                if current_count > 0.0 {
                    for val in &mut current_sum {
                        *val /= current_count;
                    }
                    map.insert(prev_id, current_sum.clone());
                }
            }
            current_atom_id = Some(atom_id.clone());
            current_sum = vec![0.0f32; dim];
            current_count = 0.0;
        }

        if blob.len() == current_sum.len() * 4 {
            for i in 0..current_sum.len() {
                let bytes: [u8; 4] = [
                    blob[i * 4],
                    blob[i * 4 + 1],
                    blob[i * 4 + 2],
                    blob[i * 4 + 3],
                ];
                current_sum[i] += f32::from_le_bytes(bytes);
            }
            current_count += 1.0;
        }
    }

    // Flush the last atom
    if let Some(prev_id) = current_atom_id {
        if current_count > 0.0 {
            for val in &mut current_sum {
                *val /= current_count;
            }
            map.insert(prev_id, current_sum);
        }
    }

    Ok(map)
}

/// Get dominant tags for a cluster of atoms
pub(crate) fn get_dominant_tags_for_cluster(
    conn: &Connection,
    atom_ids: &[String],
) -> Result<Vec<String>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(vec![]);
    }

    let placeholders: Vec<String> = atom_ids.iter().map(|_| "?".to_string()).collect();
    let placeholders_str = placeholders.join(",");

    let sql = format!(
        "SELECT t.name, COUNT(*) as cnt
         FROM atom_tags at
         JOIN tags t ON at.tag_id = t.id
         WHERE at.atom_id IN ({})
         GROUP BY t.id
         ORDER BY cnt DESC
         LIMIT 3",
        placeholders_str
    );

    let mut stmt = conn.prepare(&sql)?;

    let params: Vec<&dyn rusqlite::ToSql> =
        atom_ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

    let tags: Vec<String> = stmt
        .query_map(params.as_slice(), |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(tags)
}

/// Build neighborhood graph for an atom
pub(crate) fn build_neighborhood_graph(
    conn: &Connection,
    atom_id: &str,
    depth: i32,
    min_similarity: f32,
) -> Result<NeighborhoodGraph, AtomicCoreError> {
    use std::collections::HashMap;

    let mut atoms_at_depth: HashMap<String, i32> = HashMap::new();
    atoms_at_depth.insert(atom_id.to_string(), 0);

    // Depth 1 semantic connections
    {
        let mut stmt = conn.prepare(
            "SELECT
                CASE WHEN source_atom_id = ?1 THEN target_atom_id ELSE source_atom_id END as other_atom_id,
                similarity_score
             FROM semantic_edges
             WHERE (source_atom_id = ?1 OR target_atom_id = ?1)
               AND similarity_score >= ?2
             ORDER BY similarity_score DESC
             LIMIT 20",
        )?;

        let results: Vec<(String, f32)> = stmt
            .query_map(rusqlite::params![atom_id, min_similarity], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        for (other_id, _) in &results {
            atoms_at_depth.entry(other_id.clone()).or_insert(1);
        }
    }

    // Depth 1 tag connections
    let center_tags: Vec<String> = {
        let mut stmt = conn.prepare("SELECT tag_id FROM atom_tags WHERE atom_id = ?1")?;
        let results = stmt
            .query_map([atom_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        results
    };

    if !center_tags.is_empty() {
        let placeholders: String = center_tags
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let query = format!(
            "SELECT atom_id, COUNT(*) as shared_count
             FROM atom_tags
             WHERE tag_id IN ({})
               AND atom_id != ?
             GROUP BY atom_id
             HAVING shared_count >= 1
             ORDER BY shared_count DESC
             LIMIT 20",
            placeholders
        );

        let mut stmt = conn.prepare(&query)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = center_tags
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        params.push(&atom_id);

        let tag_results: Vec<(String, i32)> = stmt
            .query_map(params.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        for (other_id, _) in &tag_results {
            atoms_at_depth.entry(other_id.clone()).or_insert(1);
        }
    }

    // Depth 2 if requested
    if depth >= 2 {
        let depth1_ids: Vec<String> = atoms_at_depth
            .iter()
            .filter(|(_, d)| **d == 1)
            .map(|(id, _)| id.clone())
            .collect();

        for d1_id in &depth1_ids {
            let mut stmt = conn.prepare(
                "SELECT
                    CASE WHEN source_atom_id = ?1 THEN target_atom_id ELSE source_atom_id END
                 FROM semantic_edges
                 WHERE (source_atom_id = ?1 OR target_atom_id = ?1)
                   AND similarity_score >= ?2
                 ORDER BY similarity_score DESC
                 LIMIT 5",
            )?;

            let d2_ids: Vec<String> = stmt
                .query_map(rusqlite::params![d1_id, min_similarity], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;

            for d2_id in d2_ids {
                atoms_at_depth.entry(d2_id).or_insert(2);
            }
        }
    }

    // Limit total atoms
    let max_atoms = if depth >= 2 { 30 } else { 20 };
    let mut sorted_atoms: Vec<(String, i32)> = atoms_at_depth.into_iter().collect();
    sorted_atoms.sort_by_key(|(_, d)| *d);
    sorted_atoms.truncate(max_atoms);

    let atom_ids: Vec<String> = sorted_atoms.iter().map(|(id, _)| id.clone()).collect();
    let atom_depths: HashMap<String, i32> = sorted_atoms.into_iter().collect();

    // Batch fetch atom data
    let atom_placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let atom_query = format!(
        "SELECT {} FROM atoms WHERE id IN ({})",
        ATOM_COLUMNS, atom_placeholders
    );
    let mut atom_stmt = conn.prepare(&atom_query)?;
    let atom_rows: Vec<Atom> = atom_stmt
        .query_map(rusqlite::params_from_iter(atom_ids.iter()), atom_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let atom_lookup: HashMap<String, Atom> =
        atom_rows.into_iter().map(|a| (a.id.clone(), a)).collect();

    // Batch fetch tags for all atoms
    let tag_map = get_atom_tags_map_for_ids(conn, &atom_ids)?;

    let mut atoms = Vec::new();
    for aid in &atom_ids {
        if let Some(atom) = atom_lookup.get(aid) {
            let tags = tag_map.get(aid).cloned().unwrap_or_default();
            let depth = *atom_depths.get(aid).unwrap_or(&0);
            atoms.push(NeighborhoodAtom {
                atom: AtomWithTags {
                    atom: atom.clone(),
                    tags,
                },
                depth,
            });
        }
    }

    // Batch fetch all semantic edges between these atoms (single query)
    let edge_query = format!(
        "SELECT source_atom_id, target_atom_id, similarity_score
         FROM semantic_edges
         WHERE source_atom_id IN ({0}) AND target_atom_id IN ({0})",
        atom_placeholders
    );
    // Need to pass atom_ids twice (once for source, once for target)
    let mut edge_params: Vec<String> = atom_ids.clone();
    edge_params.extend(atom_ids.clone());
    let mut edge_stmt = conn.prepare(&edge_query)?;
    let semantic_edges: HashMap<(String, String), f32> = edge_stmt
        .query_map(rusqlite::params_from_iter(edge_params.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f32>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .map(|(src, tgt, score)| ((src, tgt), score))
        .collect();

    // Batch fetch shared tag counts between all atom pairs (single query)
    let shared_tag_query = format!(
        "SELECT a1.atom_id, a2.atom_id, COUNT(*) as shared
         FROM atom_tags a1
         INNER JOIN atom_tags a2 ON a1.tag_id = a2.tag_id
         WHERE a1.atom_id IN ({0}) AND a2.atom_id IN ({0})
           AND a1.atom_id < a2.atom_id
         GROUP BY a1.atom_id, a2.atom_id",
        atom_placeholders
    );
    let mut shared_stmt = conn.prepare(&shared_tag_query)?;
    let shared_tags_map: HashMap<(String, String), i32> = shared_stmt
        .query_map(rusqlite::params_from_iter(edge_params.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i32>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .map(|(a, b, count)| ((a, b), count))
        .collect();

    // Build edges from pre-fetched data
    let mut edges = Vec::new();
    for i in 0..atom_ids.len() {
        for j in (i + 1)..atom_ids.len() {
            let id_a = &atom_ids[i];
            let id_b = &atom_ids[j];

            // Look up semantic score (edges stored with consistent ordering)
            let semantic_score = semantic_edges
                .get(&(id_a.clone(), id_b.clone()))
                .or_else(|| semantic_edges.get(&(id_b.clone(), id_a.clone())))
                .copied();

            // Look up shared tags (stored with a < b ordering)
            let (key_a, key_b) = if id_a < id_b {
                (id_a, id_b)
            } else {
                (id_b, id_a)
            };
            let shared_tags = shared_tags_map
                .get(&(key_a.clone(), key_b.clone()))
                .copied()
                .unwrap_or(0);

            if semantic_score.is_some() || shared_tags > 0 {
                let edge_type = match (semantic_score.is_some(), shared_tags > 0) {
                    (true, true) => "both",
                    (true, false) => "semantic",
                    (false, true) => "tag",
                    (false, false) => continue,
                };

                let semantic_strength = semantic_score.unwrap_or(0.0);
                let tag_strength = (shared_tags as f32 * 0.15).min(0.6);
                let strength = (semantic_strength + tag_strength).min(1.0);

                edges.push(NeighborhoodEdge {
                    source_id: id_a.clone(),
                    target_id: id_b.clone(),
                    edge_type: edge_type.to_string(),
                    strength,
                    shared_tag_count: shared_tags,
                    similarity_score: semantic_score,
                });
            }
        }
    }

    Ok(NeighborhoodGraph {
        center_atom_id: atom_id.to_string(),
        atoms,
        edges,
    })
}

// ==================== Helper Functions ====================

/// Strip image markdown from text: ![alt](url) -> empty
/// Strip inline markdown to plain text using pulldown-cmark.
/// Extracts only text content, dropping images, links (keeps link text), and formatting.
fn strip_inline_markdown(text: &str) -> String {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    let parser = Parser::new(text);
    let mut out = String::with_capacity(text.len());
    let mut skip = false;

    for event in parser {
        match event {
            Event::Text(t) if !skip => out.push_str(&t),
            Event::Code(t) if !skip => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak if !skip => out.push(' '),
            // Skip image alt text
            Event::Start(Tag::Image { .. }) => skip = true,
            Event::End(TagEnd::Image) => skip = false,
            _ => {}
        }
    }
    out
}

/// Check if a line is non-text content that should be skipped in snippets.
fn is_non_text_line(trimmed: &str) -> bool {
    trimmed.starts_with("```") ||                              // code fence
    trimmed.starts_with("![") ||                               // image
    trimmed.chars().all(|c| c == '-' || c == '*' || c == '_' || c == ' ') && trimmed.len() >= 3 || // hr
    (trimmed.starts_with("http://") || trimmed.starts_with("https://")) && !trimmed.contains(' ')
    // bare URL
}

/// Extract a plain-text title (first line) and snippet (subsequent text) from markdown content.
/// Strips all markdown formatting. Skips images, bare URLs, code fences, and horizontal rules
/// from the snippet. Returns (title, snippet) with snippet up to `max_snippet_len` characters.
pub fn extract_title_and_snippet(content: &str, max_snippet_len: usize) -> (String, String) {
    let mut title = String::new();
    let mut snippet = String::new();
    let mut in_code_block = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Track code blocks
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }

        // Skip empty lines and content inside code blocks
        if trimmed.is_empty() || in_code_block {
            continue;
        }

        // Skip non-text lines (images, bare URLs, horizontal rules) for both title and snippet
        if is_non_text_line(trimmed) {
            continue;
        }

        // First text line becomes the title
        if title.is_empty() {
            let stripped = if trimmed.starts_with('#') {
                trimmed.trim_start_matches('#').trim_start()
            } else {
                trimmed
            };
            let candidate = strip_inline_markdown(stripped).trim().to_string();
            if !candidate.is_empty() {
                title = candidate;
            }
            continue;
        }

        // Strip heading markers
        let stripped = if trimmed.starts_with('#') {
            trimmed.trim_start_matches('#').trim_start()
        } else {
            trimmed
        };

        let plain = strip_inline_markdown(stripped);
        let plain = plain.trim();
        if plain.is_empty() {
            continue;
        }

        if !snippet.is_empty() {
            snippet.push(' ');
        }
        snippet.push_str(plain);

        // Stop once we have enough
        if snippet.len() >= max_snippet_len {
            break;
        }
    }

    // Truncate snippet to max length
    if snippet.len() > max_snippet_len {
        let truncated: String = snippet.chars().take(max_snippet_len).collect();
        snippet = format!("{}...", truncated.trim_end());
    }

    (title, snippet)
}

/// Parse a source identifier from a source_url.
/// - HTTP(S) URLs: extract hostname, strip `www.` prefix
/// - Other scheme:// URIs (kindle://, obsidian://): use the scheme
/// - Fallback: return the raw string
pub(crate) fn parse_source(source_url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(source_url) {
        if let Some(host) = parsed.host_str() {
            return host.strip_prefix("www.").unwrap_or(host).to_string();
        }
        return parsed.scheme().to_string();
    }
    source_url.to_string()
}

/// Standard SELECT columns for reading an Atom from the DB.
pub(crate) const ATOM_COLUMNS: &str = "id, content, title, snippet, source_url, source, published_at, created_at, updated_at, COALESCE(embedding_status, 'pending'), COALESCE(tagging_status, 'pending'), embedding_error, tagging_error, COALESCE(kind, 'captured')";

/// Same columns but table-aliased for JOINs.
pub(crate) const ATOM_COLUMNS_A: &str = "a.id, a.content, a.title, a.snippet, a.source_url, a.source, a.published_at, a.created_at, a.updated_at, COALESCE(a.embedding_status, 'pending'), COALESCE(a.tagging_status, 'pending'), a.embedding_error, a.tagging_error, COALESCE(a.kind, 'captured')";

/// Parse an Atom from a row selected with ATOM_COLUMNS.
pub(crate) fn atom_from_row(row: &rusqlite::Row) -> rusqlite::Result<Atom> {
    let kind_str: String = row.get(13)?;
    let kind = kind_str
        .parse::<crate::models::AtomKind>()
        .unwrap_or(crate::models::AtomKind::Captured);
    Ok(Atom {
        id: row.get(0)?,
        content: row.get(1)?,
        title: row.get(2)?,
        snippet: row.get(3)?,
        source_url: row.get(4)?,
        source: row.get(5)?,
        published_at: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        embedding_status: row.get(9)?,
        tagging_status: row.get(10)?,
        embedding_error: row.get(11)?,
        tagging_error: row.get(12)?,
        kind,
    })
}

/// Get tags for a specific atom
pub(crate) fn get_tags_for_atom(
    conn: &Connection,
    atom_id: &str,
) -> Result<Vec<Tag>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
             FROM tags t
             INNER JOIN atom_tags at ON t.id = at.tag_id
             WHERE at.atom_id = ?1",
    )?;

    let tags = stmt
        .query_map([atom_id], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
                parent_id: row.get(2)?,
                created_at: row.get(3)?,
                is_autotag_target: row.get::<_, i32>(4)? != 0,
                autotag_description: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(tags)
}

/// Bulk fetch all atom-tag relationships in a single query.
/// Returns a map from atom_id to Vec<Tag>.
pub(crate) fn get_all_atom_tags_map(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, Vec<Tag>>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
             FROM atom_tags at
             INNER JOIN tags t ON at.tag_id = t.id",
    )?;

    let mut map: std::collections::HashMap<String, Vec<Tag>> = std::collections::HashMap::new();

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            Tag {
                id: row.get(1)?,
                name: row.get(2)?,
                parent_id: row.get(3)?,
                created_at: row.get(4)?,
                is_autotag_target: row.get::<_, i32>(5)? != 0,
                autotag_description: row.get(6)?,
            },
        ))
    })?;

    for row in rows {
        let (atom_id, tag) = row?;
        map.entry(atom_id).or_default().push(tag);
    }

    Ok(map)
}

/// Bulk fetch atom-tag relationships for a specific set of atom IDs.
pub(crate) fn get_atom_tags_map_for_ids(
    conn: &Connection,
    atom_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<Tag>>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at, t.is_autotag_target, t.autotag_description
         FROM atom_tags at
         INNER JOIN tags t ON at.tag_id = t.id
         WHERE at.atom_id IN ({})",
        placeholders
    );

    let mut stmt = conn.prepare(&query)?;

    let mut map: std::collections::HashMap<String, Vec<Tag>> = std::collections::HashMap::new();

    let rows = stmt.query_map(rusqlite::params_from_iter(atom_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            Tag {
                id: row.get(1)?,
                name: row.get(2)?,
                parent_id: row.get(3)?,
                created_at: row.get(4)?,
                is_autotag_target: row.get::<_, i32>(5)? != 0,
                autotag_description: row.get(6)?,
            },
        ))
    })?;

    for row in rows {
        let (atom_id, tag) = row?;
        map.entry(atom_id).or_default().push(tag);
    }

    Ok(map)
}

/// Helper function to get all descendant tag IDs recursively
/// Build hierarchical tag tree with counts using pre-computed direct counts.
/// Each parent's count = its own direct count + sum of children's counts.
/// (May double-count atoms tagged with both parent and child; acceptable for display.)
///
/// Children are sorted by `atom_count` descending. When `min_count > 0`, leaf
/// nodes with `atom_count < min_count` are pruned (structural parents are kept).
/// `children_total` records the unfiltered child count so clients know when to
/// fetch the full list.
pub(crate) fn build_tag_tree_with_counts(
    all_tags: &[Tag],
    _parent_id: Option<&str>,
    direct_counts: &std::collections::HashMap<String, i32>,
    min_count: i32,
) -> Vec<TagWithCount> {
    // Build index: parent_id -> children, so each lookup is O(1) instead of O(N)
    let mut children_by_parent: std::collections::HashMap<Option<&str>, Vec<&Tag>> =
        std::collections::HashMap::new();
    for tag in all_tags {
        children_by_parent
            .entry(tag.parent_id.as_deref())
            .or_default()
            .push(tag);
    }

    fn build_subtree(
        parent_id: Option<&str>,
        children_by_parent: &std::collections::HashMap<Option<&str>, Vec<&Tag>>,
        direct_counts: &std::collections::HashMap<String, i32>,
        min_count: i32,
        is_root: bool,
    ) -> Vec<TagWithCount> {
        let Some(children) = children_by_parent.get(&parent_id) else {
            return Vec::new();
        };
        let children_total = children.len() as i32;
        let mut result: Vec<TagWithCount> = children
            .iter()
            .map(|tag| {
                let child_nodes = build_subtree(
                    Some(&tag.id),
                    children_by_parent,
                    direct_counts,
                    min_count,
                    false,
                );
                let own_count = direct_counts.get(&tag.id).copied().unwrap_or(0);
                let children_count: i32 = child_nodes.iter().map(|c| c.atom_count).sum();
                TagWithCount {
                    tag: (*tag).clone(),
                    atom_count: own_count + children_count,
                    children_total: children_by_parent
                        .get(&Some(tag.id.as_str()))
                        .map(|c| c.len() as i32)
                        .unwrap_or(0),
                    children: child_nodes,
                }
            })
            .filter(|t| {
                if min_count <= 0 || is_root {
                    true // keep all roots and when no filtering
                } else {
                    // Keep if meets threshold OR has qualifying children (structural parent)
                    t.atom_count >= min_count || !t.children.is_empty()
                }
            })
            .collect();
        // Sort children by atom_count descending
        result.sort_by(|a, b| b.atom_count.cmp(&a.atom_count));
        // Preserve children_total from before filtering (set on parent via caller)
        let _ = children_total; // used by caller
        result
    }

    build_subtree(None, &children_by_parent, direct_counts, min_count, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// Test utility: Create a test database with the five default category tags seeded.
    /// (Production code no longer auto-seeds; tests opt in via this helper.)
    async fn create_test_db() -> (AtomicCore, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db = AtomicCore::open_or_create(temp_file.path()).unwrap();
        let defaults: Vec<String> = ["Topics", "People", "Locations", "Organizations", "Events"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        db.configure_autotag_targets(&defaults, &[]).await.unwrap();
        (db, temp_file)
    }

    /// Test utility: Create an empty test database with no seeded tags.
    #[allow(dead_code)]
    fn create_empty_test_db() -> (AtomicCore, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db = AtomicCore::open_or_create(temp_file.path()).unwrap();
        (db, temp_file)
    }

    /// Get a seeded category tag by name (e.g., "Topics")
    fn get_seeded_tag(db: &AtomicCore, name: &str) -> Tag {
        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        let (id, tag_name, parent_id, created_at, is_autotag_target): (String, String, Option<String>, String, i32) = conn
            .query_row(
                "SELECT id, name, parent_id, created_at, is_autotag_target FROM tags WHERE LOWER(name) = LOWER(?1)",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        Tag {
            id,
            name: tag_name,
            parent_id,
            created_at,
            is_autotag_target: is_autotag_target != 0,
            autotag_description: String::new(),
        }
    }

    /// Test utility: Create a test atom
    async fn create_test_atom(db: &AtomicCore, content: &str) -> AtomWithTags {
        db.create_atom(
            CreateAtomRequest {
                content: content.to_string(),
                ..Default::default()
            },
            |_| {}, // no-op callback
        )
        .await
        .unwrap()
        .unwrap()
    }

    #[tokio::test]
    async fn obsidian_import_marks_user_tags_manual() {
        let dir = TempDir::new().expect("create tempdir");
        let vault = dir.path().join("Vault");
        let project_dir = vault.join("Projects");
        std::fs::create_dir_all(&project_dir).expect("create vault folder");
        std::fs::write(
            project_dir.join("note.md"),
            "---\ntags: [frontmatter-tag]\n---\n# Imported note\n\nEnough content to import.",
        )
        .expect("write note");

        let core =
            AtomicCore::open_or_create(dir.path().join("atomic.db")).expect("open sqlite test db");
        let vault_path = vault.to_string_lossy().to_string();
        let result = core
            .import_obsidian_vault(&vault_path, None, |_| {}, |_| {})
            .await
            .expect("import vault");

        assert_eq!(result.imported, 1);
        assert_eq!(result.tags_linked, 2);

        let sqlite = core.storage.as_sqlite().expect("sqlite storage");
        let conn = sqlite.db.conn.lock().expect("lock db");
        let mut stmt = conn
            .prepare(
                "SELECT t.name, at.source
                 FROM atom_tags at
                 INNER JOIN tags t ON t.id = at.tag_id
                 ORDER BY LOWER(t.name)",
            )
            .expect("prepare source query");
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query tag sources")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect tag sources");

        assert_eq!(
            rows,
            vec![
                ("frontmatter-tag".to_string(), "manual".to_string()),
                ("Projects".to_string(), "manual".to_string()),
            ]
        );
    }

    // ==================== Settings Resolver Tests ====================
    //
    // These exercise the registry-defaults + per-DB-overrides model end-to-end:
    // workspace-only keys go to the registry, overridable keys route by N,
    // and `get_settings_with_source` reports the right source for each layer.

    use crate::settings::SettingSource;
    use registry::Registry;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Build a registry plus N AtomicCore instances bound to that registry,
    /// one per "database". The registry already starts with a "default" DB,
    /// so requesting `extra_dbs=0` gives N=1.
    fn make_workspace(extra_dbs: usize) -> (Arc<Registry>, Vec<AtomicCore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let registry = Arc::new(Registry::open_or_create(dir.path()).unwrap());
        for i in 0..extra_dbs {
            registry.create_database(&format!("db-{i}")).unwrap();
        }
        let dbs = registry.list_databases().unwrap();
        let cores: Vec<AtomicCore> = dbs
            .iter()
            .map(|info| {
                let path = dir.path().join(format!("{}.db", info.id));
                AtomicCore::open_for_server_with_registry(&path, Some(Arc::clone(&registry)))
                    .unwrap()
            })
            .collect();
        (registry, cores, dir)
    }

    fn assert_vec_chunks_dimension(core: &AtomicCore, dimension: usize) {
        let sqlite = core.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='vec_chunks'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            sql.contains(&format!("float[{dimension}]")),
            "vec_chunks schema should use float[{dimension}], got {sql}"
        );
    }

    #[tokio::test]
    async fn test_workspace_only_writes_always_hit_registry() {
        // theme is workspace-only — even with multiple DBs, set_setting on
        // any core lands in registry.db, and every core sees the same value.
        let (registry, cores, _dir) = make_workspace(2);
        cores[0].set_setting("theme", "dracula").await.unwrap();

        for core in &cores {
            let settings = core.get_settings().await.unwrap();
            assert_eq!(settings.get("theme").map(String::as_str), Some("dracula"));
        }
        assert_eq!(registry.get_setting("theme").unwrap(), "dracula");
    }

    #[tokio::test]
    async fn test_overridable_with_n1_writes_to_registry() {
        // With one DB, set_setting(provider, ...) goes to the registry as a
        // workspace default — so a future second DB inherits it instead of
        // starting on the builtin default.
        let (registry, cores, dir) = make_workspace(0);
        cores[0]
            .set_setting("chat_model", "openai/gpt-4o")
            .await
            .unwrap();

        // The single DB's per-DB settings table stays empty for this key.
        let per_db = cores[0].storage.get_all_settings_sync().await.unwrap();
        assert!(!per_db.contains_key("chat_model"));
        assert_eq!(registry.get_setting("chat_model").unwrap(), "openai/gpt-4o");

        // Spin up a second DB; it inherits the workspace default.
        registry.create_database("second").unwrap();
        let second_path = dir.path().join("second.db");
        let second =
            AtomicCore::open_for_server_with_registry(&second_path, Some(Arc::clone(&registry)))
                .unwrap();
        let settings = second.get_settings().await.unwrap();
        assert_eq!(
            settings.get("chat_model").map(String::as_str),
            Some("openai/gpt-4o"),
        );
    }

    #[tokio::test]
    async fn test_overridable_with_n2_writes_per_db() {
        // With two DBs, set_setting(provider, ...) on one core writes only
        // to that DB's per-DB table. The other DB keeps inheriting from
        // the workspace default.
        let (registry, cores, _dir) = make_workspace(1);
        registry
            .set_setting("chat_model", "workspace/default")
            .unwrap();

        cores[0]
            .set_setting("chat_model", "override/for-first")
            .await
            .unwrap();

        // First DB sees its override.
        let s0 = cores[0].get_settings().await.unwrap();
        assert_eq!(
            s0.get("chat_model").map(String::as_str),
            Some("override/for-first"),
        );

        // Second DB still sees the workspace default.
        let s1 = cores[1].get_settings().await.unwrap();
        assert_eq!(
            s1.get("chat_model").map(String::as_str),
            Some("workspace/default"),
        );
    }

    #[tokio::test]
    async fn test_clear_override_falls_back_to_default() {
        let (registry, cores, _dir) = make_workspace(1);
        registry
            .set_setting("chat_model", "workspace/default")
            .unwrap();
        cores[0]
            .set_setting("chat_model", "override/for-first")
            .await
            .unwrap();

        cores[0].clear_override("chat_model").await.unwrap();

        let s = cores[0].get_settings().await.unwrap();
        assert_eq!(
            s.get("chat_model").map(String::as_str),
            Some("workspace/default"),
            "after clearing override, resolves back to workspace default"
        );
    }

    #[tokio::test]
    async fn test_clear_override_rejects_workspace_only() {
        let (_registry, cores, _dir) = make_workspace(0);
        let result = cores[0].clear_override("theme").await;
        assert!(
            result.is_err(),
            "clear_override on a workspace-only key must error"
        );
    }

    #[tokio::test]
    async fn test_clear_embedding_dimension_override_recreates_vector_index() {
        let (registry, cores, _dir) = make_workspace(1);
        registry.set_setting("provider", "openai_compat").unwrap();
        registry
            .set_setting("openai_compat_embedding_dimension", "1536")
            .unwrap();

        cores[0]
            .set_setting_with_reembed("openai_compat_embedding_dimension", "768", |_| {})
            .await
            .unwrap();
        assert_vec_chunks_dimension(&cores[0], 768);

        let result = cores[0]
            .clear_override_with_reembed("openai_compat_embedding_dimension", |_| {})
            .await
            .unwrap();

        assert!(result.embedding_space_changed);
        assert!(result.dimension_changed);
        assert_eq!(result.old_dim, 768);
        assert_eq!(result.new_dim, 1536);
        assert_vec_chunks_dimension(&cores[0], 1536);
    }

    #[tokio::test]
    async fn test_clear_embedding_model_override_marks_space_changed() {
        let (registry, cores, _dir) = make_workspace(1);
        registry
            .set_setting("embedding_model", "openai/text-embedding-3-small")
            .unwrap();

        cores[0]
            .set_setting("embedding_model", "custom/same-dimension")
            .await
            .unwrap();

        let result = cores[0]
            .clear_override_with_reembed("embedding_model", |_| {})
            .await
            .unwrap();

        assert!(result.embedding_space_changed);
        assert!(!result.dimension_changed);
        assert_eq!(result.old_dim, 1536);
        assert_eq!(result.new_dim, 1536);
    }

    #[tokio::test]
    async fn test_get_settings_with_source_labels_each_layer() {
        let (registry, cores, _dir) = make_workspace(1);
        // Workspace default for an overridable key.
        registry
            .set_setting("chat_model", "workspace/default")
            .unwrap();
        // Per-DB override for another overridable key on the first DB.
        cores[0]
            .set_setting("tagging_model", "override/tag")
            .await
            .unwrap();
        // Workspace-only key (theme).
        registry.set_setting("theme", "dracula").unwrap();

        let s = cores[0].get_settings_with_source().await.unwrap();

        assert_eq!(s["theme"].source, SettingSource::Workspace);
        assert_eq!(s["theme"].value, "dracula");

        assert_eq!(s["chat_model"].source, SettingSource::WorkspaceDefault);
        assert_eq!(s["chat_model"].value, "workspace/default");

        assert_eq!(s["tagging_model"].source, SettingSource::Override);
        assert_eq!(s["tagging_model"].value, "override/tag");

        // The second DB sees the same workspace default but no override.
        let s2 = cores[1].get_settings_with_source().await.unwrap();
        assert_eq!(s2["tagging_model"].source, SettingSource::WorkspaceDefault);
    }

    #[tokio::test]
    async fn test_per_db_row_for_workspace_only_key_is_ignored() {
        // A legacy per-DB row for a workspace-only key (left over from before
        // the resolver landed) must not poison the resolved value.
        let (registry, cores, _dir) = make_workspace(0);
        registry.set_setting("theme", "registry-value").unwrap();
        // Sneak a legacy row directly into the per-DB table.
        cores[0]
            .storage
            .set_setting_sync("theme", "stale-per-db-value")
            .await
            .unwrap();

        let s = cores[0].get_settings().await.unwrap();
        assert_eq!(
            s.get("theme").map(String::as_str),
            Some("registry-value"),
            "workspace-only keys ignore per-DB rows even when present"
        );
    }

    // ==================== Atom CRUD Tests ====================

    #[tokio::test]
    async fn test_create_atom_returns_atom() {
        let (db, _temp) = create_test_db().await;

        let atom = create_test_atom(&db, "Test content for atom").await;

        assert!(!atom.atom.id.is_empty());
        assert_eq!(atom.atom.content, "Test content for atom");
        assert_eq!(atom.atom.embedding_status, "pending");
        assert!(atom.tags.is_empty());
    }

    #[tokio::test]
    async fn test_get_atom_by_id() {
        let (db, _temp) = create_test_db().await;

        let created = create_test_atom(&db, "Content to retrieve").await;
        let retrieved = db.get_atom(&created.atom.id).await.unwrap();

        assert!(retrieved.is_some());
        let atom = retrieved.unwrap();
        assert_eq!(atom.atom.id, created.atom.id);
        assert_eq!(atom.atom.content, "Content to retrieve");
    }

    #[tokio::test]
    async fn test_get_atom_not_found() {
        let (db, _temp) = create_test_db().await;

        let result = db.get_atom("nonexistent-id-12345").await.unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_all_atoms() {
        let (db, _temp) = create_test_db().await;

        // Create multiple atoms
        create_test_atom(&db, "First atom").await;
        create_test_atom(&db, "Second atom").await;
        create_test_atom(&db, "Third atom").await;

        let all_atoms = db.get_all_atoms().await.unwrap();

        assert_eq!(all_atoms.len(), 3);
    }

    #[tokio::test]
    async fn test_delete_atom() {
        let (db, _temp) = create_test_db().await;

        let atom = create_test_atom(&db, "Atom to delete").await;
        let atom_id = atom.atom.id.clone();

        // Verify it exists
        assert!(db.get_atom(&atom_id).await.unwrap().is_some());

        // Delete it
        db.delete_atom(&atom_id).await.unwrap();

        // Verify it's gone
        assert!(db.get_atom(&atom_id).await.unwrap().is_none());
    }

    // ==================== Tag CRUD Tests ====================

    #[tokio::test]
    async fn test_create_tag_root() {
        let (db, _temp) = create_test_db().await;

        let tag = db.create_tag("CustomRoot", None).await.unwrap();

        assert!(!tag.id.is_empty());
        assert_eq!(tag.name, "CustomRoot");
        assert!(tag.parent_id.is_none());
    }

    #[tokio::test]
    async fn test_seeded_category_tags_exist() {
        let (db, _temp) = create_test_db().await;
        let all_tags = db.get_all_tags().await.unwrap();
        let names: Vec<&str> = all_tags.iter().map(|t| t.tag.name.as_str()).collect();
        assert!(names.contains(&"Topics"));
        assert!(names.contains(&"People"));
        assert!(names.contains(&"Locations"));
        assert!(names.contains(&"Organizations"));
        assert!(names.contains(&"Events"));
    }

    #[tokio::test]
    async fn test_create_tag_with_parent() {
        let (db, _temp) = create_test_db().await;

        // Use seeded parent tag
        let parent = get_seeded_tag(&db, "Topics");

        // Create child tag
        let child = db.create_tag("AI", Some(&parent.id)).await.unwrap();

        assert_eq!(child.name, "AI");
        assert_eq!(child.parent_id, Some(parent.id));
    }

    #[tokio::test]
    async fn test_get_all_tags_hierarchical() {
        let (db, _temp) = create_test_db().await;

        // Use seeded Topics, add hierarchy: Topics -> AI -> Machine Learning
        let topics = get_seeded_tag(&db, "Topics");
        let ai = db.create_tag("AI", Some(&topics.id)).await.unwrap();
        let _ml = db
            .create_tag("Machine Learning", Some(&ai.id))
            .await
            .unwrap();

        let all_tags = db.get_all_tags().await.unwrap();

        // Should have 6 seeded root tags; find Topics and check its children
        let topics_node = all_tags.iter().find(|t| t.tag.name == "Topics").unwrap();
        assert_eq!(topics_node.children.len(), 1);
        assert_eq!(topics_node.children[0].tag.name, "AI");
        assert_eq!(topics_node.children[0].children.len(), 1);
        assert_eq!(
            topics_node.children[0].children[0].tag.name,
            "Machine Learning"
        );
    }

    #[tokio::test]
    async fn test_delete_tag() {
        let (db, _temp) = create_test_db().await;

        let tag = db.create_tag("ToDelete", None).await.unwrap();
        let tag_id = tag.id.clone();

        // Verify it exists in get_all_tags
        let tags_before = db.get_all_tags().await.unwrap();
        assert!(tags_before.iter().any(|t| t.tag.id == tag_id));

        // Delete it
        db.delete_tag(&tag_id, false).await.unwrap();

        // Verify it's gone
        let tags_after = db.get_all_tags().await.unwrap();
        assert!(!tags_after.iter().any(|t| t.tag.id == tag_id));
    }

    #[tokio::test]
    async fn test_delete_tag_removes_wiki_fts_rows_for_descendants() {
        let (db, _temp) = create_test_db().await;

        let parent = db.create_tag("Parent", None).await.unwrap();
        let child = db.create_tag("Child", Some(&parent.id)).await.unwrap();

        {
            let sqlite = db.storage.as_sqlite().unwrap();
            let conn = sqlite.db.conn.lock().unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            for (wiki_id, tag_id, tag_name, content) in [
                (
                    uuid::Uuid::new_v4().to_string(),
                    parent.id.clone(),
                    "Parent".to_string(),
                    "parent wiki".to_string(),
                ),
                (
                    uuid::Uuid::new_v4().to_string(),
                    child.id.clone(),
                    "Child".to_string(),
                    "child wiki".to_string(),
                ),
            ] {
                conn.execute(
                    "INSERT INTO wiki_articles (id, tag_id, content, created_at, updated_at, atom_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                    rusqlite::params![&wiki_id, &tag_id, &content, &now, &now],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO wiki_articles_fts (id, tag_id, tag_name, content)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![&wiki_id, &tag_id, &tag_name, &content],
                )
                .unwrap();
            }
        }

        db.delete_tag(&parent.id, true).await.unwrap();

        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        let remaining_fts: i64 = conn
            .query_row("SELECT COUNT(*) FROM wiki_articles_fts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining_fts, 0);
    }

    #[tokio::test]
    async fn test_guarded_wiki_baseline_advance_keeps_older_retagged_atoms_pending() {
        let (db, _temp) = create_empty_test_db();
        let tag = db.create_tag("Retagged", None).await.unwrap();
        let article_updated_at = "2026-01-02T00:00:00+00:00";

        {
            let sqlite = db.storage.as_sqlite().unwrap();
            let conn = sqlite.db.conn.lock().unwrap();
            for atom_id in ["atom1", "atom2"] {
                conn.execute(
                    "INSERT INTO atoms (id, content, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?3)",
                    rusqlite::params![atom_id, "older atom content", "2026-01-01T00:00:00+00:00"],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO atom_tags (atom_id, tag_id) VALUES (?1, ?2)",
                    rusqlite::params![atom_id, &tag.id],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO wiki_articles (id, tag_id, content, created_at, updated_at, atom_count)
                 VALUES (?1, ?2, ?3, ?4, ?4, 1)",
                rusqlite::params![
                    "wiki1",
                    &tag.id,
                    "Existing article",
                    article_updated_at
                ],
            )
            .unwrap();
        }

        let advanced = db
            .storage
            .advance_wiki_baseline_sync(&tag.id, Some(1))
            .await
            .unwrap();
        assert!(
            !advanced,
            "baseline must not advance when current atom count increased"
        );

        let status = db.get_wiki_status(&tag.id).await.unwrap();
        assert_eq!(status.article_atom_count, 1);
        assert_eq!(status.current_atom_count, 2);
        assert_eq!(status.new_atoms_available, 1);
        assert_eq!(status.updated_at.as_deref(), Some(article_updated_at));
    }

    #[tokio::test]
    async fn test_global_search_ignores_stale_wiki_fts_rows() {
        let (db, _temp) = create_test_db().await;

        let stale_tag = db.create_tag("StaleTag", None).await.unwrap();

        {
            let sqlite = db.storage.as_sqlite().unwrap();
            let conn = sqlite.db.conn.lock().unwrap();
            let stale_wiki_id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO wiki_articles_fts (id, tag_id, tag_name, content)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    &stale_wiki_id,
                    &stale_tag.id,
                    &stale_tag.name,
                    "orphaned wiki content"
                ],
            )
            .unwrap();
        }

        let response = db.search_global_keyword("orphaned", 10).await.unwrap();

        assert!(response.wiki.is_empty());
    }

    #[tokio::test]
    async fn test_global_search_returns_snippet_and_match_offsets() {
        let (db, _temp) = create_test_db().await;

        // Content with multiple matches so we can exercise match_offsets count
        // and confirm the snippet markers wrap the hits.
        let content = "Alpha beta gamma. Beta is nice. Something about beta again.";
        db.create_atom(
            CreateAtomRequest {
                content: content.to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let response = db.search_global_keyword("beta", 10).await.unwrap();
        assert_eq!(response.atoms.len(), 1, "expected one atom-level hit");

        let result = &response.atoms[0];
        let snippet = result
            .match_snippet
            .as_ref()
            .expect("match_snippet should be populated");
        assert!(
            snippet.contains('\u{E000}') && snippet.contains('\u{E001}'),
            "snippet must contain FTS match markers, got: {:?}",
            snippet
        );

        let offsets = result
            .match_offsets
            .as_ref()
            .expect("match_offsets should be populated for keyword hits");
        assert_eq!(offsets.len(), 3, "expected three 'beta' matches");
        for off in offsets {
            let slice = &content[off.start as usize..off.end as usize];
            assert_eq!(
                slice.to_lowercase(),
                "beta",
                "each offset must slice to the matched term"
            );
        }

        // Three matches fit under the cap, so the count should match the list.
        assert_eq!(
            result.match_count,
            Some(3),
            "match_count should carry the true total"
        );

        // Serialize and confirm the atom's own stored `snippet` is still at the
        // top level — the FTS excerpt must live under a distinct key so JSON
        // consumers don't silently lose the saved preview to a duplicated key.
        let json = serde_json::to_value(result).unwrap();
        let obj = json.as_object().expect("result serializes as an object");
        assert!(
            obj.contains_key("snippet"),
            "atom preview (Atom.snippet) must be preserved at top level"
        );
        assert!(
            obj.contains_key("match_snippet"),
            "FTS excerpt must be exposed as match_snippet"
        );
        let preview = obj.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            !preview.contains('\u{E000}'),
            "preview must not accidentally carry FTS markers, got {:?}",
            preview
        );
    }

    #[tokio::test]
    async fn test_global_search_caps_match_offsets_but_reports_total() {
        let (db, _temp) = create_test_db().await;

        // 25 occurrences of the token `zeta` — well past the cap of 10.
        let mut content = String::new();
        for _ in 0..25 {
            content.push_str("zeta ");
        }
        db.create_atom(
            CreateAtomRequest {
                content: content.trim().to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let response = db.search_global_keyword("zeta", 10).await.unwrap();
        assert_eq!(response.atoms.len(), 1);
        let result = &response.atoms[0];

        let offsets = result
            .match_offsets
            .as_ref()
            .expect("match_offsets should be populated");
        assert_eq!(
            offsets.len(),
            10,
            "match_offsets must be capped at MAX_MATCH_OFFSETS_PER_RESULT (10)"
        );
        assert_eq!(
            result.match_count,
            Some(25),
            "match_count must carry the true total even when offsets are capped"
        );
    }

    #[tokio::test]
    async fn test_global_search_returns_wiki_snippet_and_offsets() {
        let (db, _temp) = create_test_db().await;
        let tag = db.create_tag("Zebras", None).await.unwrap();

        // Insert a wiki article directly (mirrors how wiki generation writes).
        let content = "Zebras are striped. The zebra herd migrates annually. See also: zebra.";
        let wiki_id = {
            let sqlite = db.storage.as_sqlite().unwrap();
            let conn = sqlite.db.conn.lock().unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO wiki_articles (id, tag_id, content, atom_count, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    &id,
                    &tag.id,
                    content,
                    1_i32,
                    "2026-01-01T00:00:00Z",
                    "2026-01-01T00:00:00Z",
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO wiki_articles_fts (id, tag_id, tag_name, content)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![&id, &tag.id, &tag.name, content],
            )
            .unwrap();
            id
        };

        let response = db.search_global_keyword("zebra", 10).await.unwrap();
        assert_eq!(response.wiki.len(), 1, "expected one wiki hit");

        let hit = &response.wiki[0];
        assert_eq!(hit.id, wiki_id);
        assert_eq!(hit.content, content, "full content should round-trip");
        let snip = hit
            .match_snippet
            .as_ref()
            .expect("match_snippet should be populated");
        assert!(
            snip.contains('\u{E000}') && snip.contains('\u{E001}'),
            "wiki snippet must carry FTS markers"
        );

        let offsets = hit
            .match_offsets
            .as_ref()
            .expect("match_offsets should be populated for keyword hits");
        // FTS5 phrase-quotes the query, so `zebra` matches whole tokens only —
        // "Zebras" is a different token and is *not* counted here.
        assert_eq!(offsets.len(), 2, "expected two 'zebra' matches");
        for off in offsets {
            let slice = &content[off.start as usize..off.end as usize];
            assert_eq!(
                slice.to_lowercase(),
                "zebra",
                "offset should slice to the literal token 'zebra'"
            );
        }
        assert_eq!(
            hit.match_count,
            Some(2),
            "wiki match_count should match the FTS total"
        );
    }

    #[tokio::test]
    async fn test_atoms_fts_stays_in_sync_on_update_and_delete() {
        let (db, _temp) = create_test_db().await;

        let atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "First version has the marker zebrafish in it".to_string(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        // Initial search hits.
        let hits = db.search_global_keyword("zebrafish", 10).await.unwrap();
        assert_eq!(hits.atoms.len(), 1);

        // Replace content so the FTS row must be re-synced.
        db.update_atom(
            &atom.atom.id,
            UpdateAtomRequest {
                content: "Rewritten body without the marker term".to_string(),
                source_url: None,
                published_at: None,
                tag_ids: None,
            },
            |_| {},
        )
        .await
        .unwrap();

        let hits = db.search_global_keyword("zebrafish", 10).await.unwrap();
        assert!(
            hits.atoms.is_empty(),
            "updated atom should no longer match the old term"
        );
        let hits = db.search_global_keyword("rewritten", 10).await.unwrap();
        assert_eq!(hits.atoms.len(), 1, "new content must be searchable");

        // Delete and confirm the atom drops out of the FTS index.
        db.delete_atom(&atom.atom.id).await.unwrap();
        let hits = db.search_global_keyword("rewritten", 10).await.unwrap();
        assert!(hits.atoms.is_empty(), "deleted atom must not appear");
    }

    // ==================== Atom-Tag Relationship Tests ====================

    #[tokio::test]
    async fn test_create_atom_with_tags() {
        let (db, _temp) = create_test_db().await;

        // Create tags first
        let tag1 = db.create_tag("Tag1", None).await.unwrap();
        let tag2 = db.create_tag("Tag2", None).await.unwrap();

        // Create atom with tags
        let atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "Tagged content".to_string(),
                    tag_ids: vec![tag1.id.clone(), tag2.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        // Verify tags are attached
        assert_eq!(atom.tags.len(), 2);
        let tag_names: Vec<&str> = atom.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(tag_names.contains(&"Tag1"));
        assert!(tag_names.contains(&"Tag2"));
    }

    #[tokio::test]
    async fn test_get_atoms_by_tag_includes_descendants() {
        let (db, _temp) = create_test_db().await;

        // Use seeded Topics, add child: Topics -> AI
        let topics = get_seeded_tag(&db, "Topics");
        let ai = db.create_tag("AI", Some(&topics.id)).await.unwrap();

        // Create atom tagged with AI (child)
        let atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "AI content".to_string(),
                    tag_ids: vec![ai.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        // Query by parent tag (Topics) should include atoms tagged with AI
        let atoms = db
            .get_atoms_by_tag(&topics.id, &crate::models::KindFilter::All)
            .await
            .unwrap();

        assert_eq!(atoms.len(), 1);
        assert_eq!(atoms[0].atom.id, atom.atom.id);
    }

    #[tokio::test]
    async fn test_atom_tag_counts() {
        let (db, _temp) = create_test_db().await;

        // Use seeded parent tag
        let topics = get_seeded_tag(&db, "Topics");

        // Create 3 atoms with this tag
        for i in 0..3 {
            db.create_atom(
                CreateAtomRequest {
                    content: format!("Atom {}", i),
                    tag_ids: vec![topics.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap();
        }

        // Get tags and check count
        let all_tags = db.get_all_tags().await.unwrap();
        let topics_tag = all_tags.iter().find(|t| t.tag.name == "Topics").unwrap();

        assert_eq!(topics_tag.atom_count, 3);
    }

    #[test]
    fn test_strip_inline_markdown() {
        // Backslash escapes
        assert_eq!(strip_inline_markdown(r"U\.S\. policy"), "U.S. policy");
        // Bold and italic
        assert_eq!(
            strip_inline_markdown("**bold** and *italic*"),
            "bold and italic"
        );
        // Links: keep text, drop URL
        assert_eq!(
            strip_inline_markdown("[click here](https://example.com)"),
            "click here"
        );
        // Images: drop entirely
        assert_eq!(
            strip_inline_markdown("before ![alt](img.png) after"),
            "before  after"
        );
        // Inline code
        assert_eq!(strip_inline_markdown("use `foo()` here"), "use foo() here");
        // Mixed
        assert_eq!(
            strip_inline_markdown(r"The **U\.S\.** has [a link](http://x.com)"),
            "The U.S. has a link"
        );
    }

    // ==================== Atom-kind discriminator (V18) ====================
    //
    // Tests live inline so they can use the unexposed test-only helper
    // `stamp_report_kind` to simulate a `kind = 'report'` atom — phase 2's
    // report writer will be the production path that emits these.

    /// Test-only: flip an atom's kind to 'report' via raw SQL. Phase 1 has no
    /// production write path that produces report atoms; this helper exists
    /// only to verify the filter discipline holds *once* such atoms exist.
    fn stamp_report_kind(db: &AtomicCore, atom_id: &str) {
        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        conn.execute(
            "UPDATE atoms SET kind = 'report' WHERE id = ?1",
            rusqlite::params![atom_id],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_migration_defaults_existing_atoms_to_captured() {
        let (db, _temp) = create_empty_test_db();
        let atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Captured note".to_string(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        // Newly inserted atom carries the default kind...
        assert_eq!(atom.atom.kind, models::AtomKind::Captured);
        // ...and round-trips back from the DB.
        let read_back = db.get_atom(&atom.atom.id).await.unwrap().unwrap();
        assert_eq!(read_back.atom.kind, models::AtomKind::Captured);
    }

    #[tokio::test]
    async fn test_get_atoms_by_tag_filters_by_kind() {
        let (db, _temp) = create_test_db().await;
        let topics = get_seeded_tag(&db, "Topics");
        let tag = db.create_tag("AI", Some(&topics.id)).await.unwrap();

        let captured = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Captured atom".to_string(),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let report = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Future finding".to_string(),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        stamp_report_kind(&db, &report.atom.id);

        // All: both kinds returned
        let all = db
            .get_atoms_by_tag(&tag.id, &models::KindFilter::All)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);

        // Only(Captured): report excluded
        let captured_only = db
            .get_atoms_by_tag(
                &tag.id,
                &models::KindFilter::only(models::AtomKind::Captured),
            )
            .await
            .unwrap();
        assert_eq!(captured_only.len(), 1);
        assert_eq!(captured_only[0].atom.id, captured.atom.id);
        assert_eq!(captured_only[0].atom.kind, models::AtomKind::Captured);

        // Only(Report): only report returned
        let report_only = db
            .get_atoms_by_tag(&tag.id, &models::KindFilter::only(models::AtomKind::Report))
            .await
            .unwrap();
        assert_eq!(report_only.len(), 1);
        assert_eq!(report_only[0].atom.id, report.atom.id);
        assert_eq!(report_only[0].atom.kind, models::AtomKind::Report);
    }

    #[tokio::test]
    async fn test_list_atoms_filters_by_kind() {
        let (db, _temp) = create_empty_test_db();
        let captured = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Captured".to_string(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let report = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Report".to_string(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        stamp_report_kind(&db, &report.atom.id);

        let params = ListAtomsParams {
            tag_id: None,
            limit: 50,
            offset: 0,
            cursor: None,
            cursor_id: None,
            source_filter: SourceFilter::All,
            source_value: None,
            sort_by: SortField::Updated,
            sort_order: SortOrder::Desc,
        };

        let all = db
            .list_atoms(&params, &models::KindFilter::All)
            .await
            .unwrap();
        assert_eq!(all.atoms.len(), 2);
        assert_eq!(all.total_count, 2);

        let captured_only = db
            .list_atoms(
                &params,
                &models::KindFilter::only(models::AtomKind::Captured),
            )
            .await
            .unwrap();
        assert_eq!(captured_only.atoms.len(), 1);
        assert_eq!(captured_only.atoms[0].id, captured.atom.id);
        // The denormalized fast-path is kind-blind — filtering forces the
        // slow path. Confirm total_count reflects the filter too.
        assert_eq!(captured_only.total_count, 1);
    }

    #[tokio::test]
    async fn test_count_atoms_with_tags_filters_by_kind() {
        let (db, _temp) = create_test_db().await;
        let topics = get_seeded_tag(&db, "Topics");
        let tag = db.create_tag("AI", Some(&topics.id)).await.unwrap();

        db.create_atom(
            CreateAtomRequest {
                content: "# Captured".to_string(),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();
        let report = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Report".to_string(),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        stamp_report_kind(&db, &report.atom.id);

        let storage = &db.storage;
        let all = storage
            .count_atoms_with_tags_impl(&[tag.id.clone()], &models::KindFilter::All)
            .await
            .unwrap();
        assert_eq!(all, 2);

        let captured_only = storage
            .count_atoms_with_tags_impl(
                &[tag.id.clone()],
                &models::KindFilter::only(models::AtomKind::Captured),
            )
            .await
            .unwrap();
        assert_eq!(captured_only, 1);
    }

    #[tokio::test]
    async fn test_kind_filter_empty_only_matches_nothing() {
        // The `Only(vec![])` defensive case: must match nothing, not silently
        // degrade to "no filter."
        let (db, _temp) = create_empty_test_db();
        db.create_atom(
            CreateAtomRequest {
                content: "# One".to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let params = ListAtomsParams {
            tag_id: None,
            limit: 50,
            offset: 0,
            cursor: None,
            cursor_id: None,
            source_filter: SourceFilter::All,
            source_value: None,
            sort_by: SortField::Updated,
            sort_order: SortOrder::Desc,
        };
        let result = db
            .list_atoms(&params, &models::KindFilter::Only(vec![]))
            .await
            .unwrap();
        assert_eq!(result.atoms.len(), 0);
        assert_eq!(result.total_count, 0);
    }

    #[tokio::test]
    async fn test_atom_kind_roundtrips_through_create_path() {
        // CreateAtomRequest does not accept `kind` — every user-facing create
        // path produces a Captured atom. This test pins that invariant.
        let (db, _temp) = create_empty_test_db();
        let atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "test".to_string(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(atom.atom.kind, models::AtomKind::Captured);
    }

    /// Test-only: insert a chunk row for an atom (bypasses the embedding
    /// pipeline). Used by the wiki source-chunk filter test to give both the
    /// captured and report atoms something the wiki query would otherwise
    /// happily pull.
    fn insert_test_chunk(db: &AtomicCore, atom_id: &str, chunk_index: i32, content: &str) {
        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO atom_chunks (id, atom_id, chunk_index, content) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, atom_id, chunk_index, content],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_wiki_source_chunks_excludes_report_atoms() {
        // Centroid + unranked chunk selection MUST filter by kind at scope
        // resolution. Filtering only the atom_count (the bug) would leave
        // report-kind chunks in the LLM's source material while the count
        // claimed they weren't there.
        let (db, _temp) = create_test_db().await;
        let topics = get_seeded_tag(&db, "Topics");
        let tag = db.create_tag("AI", Some(&topics.id)).await.unwrap();

        let captured = db
            .create_atom(
                CreateAtomRequest {
                    content: "captured content".to_string(),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let report = db
            .create_atom(
                CreateAtomRequest {
                    content: "report finding content".to_string(),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        stamp_report_kind(&db, &report.atom.id);

        // Chunks for both atoms; the report atom's chunk is the contamination
        // signal — it must NOT appear in wiki source results.
        insert_test_chunk(&db, &captured.atom.id, 0, "captured chunk");
        insert_test_chunk(&db, &report.atom.id, 0, "report chunk should not leak");

        // No tag centroid in test DB → exercises the unranked fallback path,
        // which is where the previous bug lived (its SQL re-joined atom_tags
        // and never saw atoms.kind).
        let sqlite = db.storage.as_sqlite().unwrap();
        let (chunks, atom_count) = sqlite
            .get_wiki_source_chunks_sync(&tag.id, 100_000)
            .unwrap();

        assert_eq!(atom_count, 1, "count should reflect captured-only scope");
        let atom_ids: std::collections::HashSet<&str> =
            chunks.iter().map(|c| c.atom_id.as_str()).collect();
        assert!(
            !atom_ids.contains(report.atom.id.as_str()),
            "report-kind atom must not appear in wiki source chunks"
        );
        assert!(
            atom_ids.contains(captured.atom.id.as_str()),
            "captured atom should still be a wiki source"
        );
    }

    #[tokio::test]
    async fn test_wiki_update_chunks_excludes_report_atoms() {
        let (db, _temp) = create_test_db().await;
        let topics = get_seeded_tag(&db, "Topics");
        let tag = db.create_tag("AI", Some(&topics.id)).await.unwrap();

        // Anchor "last_update" before creating any new atoms.
        let last_update = "1970-01-01T00:00:00Z";

        let captured = db
            .create_atom(
                CreateAtomRequest {
                    content: "captured".to_string(),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let report = db
            .create_atom(
                CreateAtomRequest {
                    content: "report".to_string(),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        stamp_report_kind(&db, &report.atom.id);
        insert_test_chunk(&db, &captured.atom.id, 0, "captured update chunk");
        insert_test_chunk(&db, &report.atom.id, 0, "report update chunk leak");

        let sqlite = db.storage.as_sqlite().unwrap();
        let result = sqlite
            .get_wiki_update_chunks_sync(&tag.id, last_update, 100_000)
            .unwrap();
        let (chunks, atom_count) = result.expect("expected update chunks");
        assert_eq!(atom_count, 1);
        let atom_ids: std::collections::HashSet<&str> =
            chunks.iter().map(|c| c.atom_id.as_str()).collect();
        assert!(!atom_ids.contains(report.atom.id.as_str()));
        assert!(atom_ids.contains(captured.atom.id.as_str()));
    }

    // ==================== task_runs execution ledger (V19) ====================
    //
    // Phase 1.5 ships the ledger plumbing dormant — these tests are the
    // production proof that the conditional-update predicates, lease
    // semantics, and retry/abandon branches behave as the plan doc spec'd
    // before phase 2 (reports) wires up a real caller.

    use crate::models::{TaskRun, TaskRunState, TaskRunTrigger};
    use crate::scheduler::ledger;
    use chrono::Utc;

    /// Build a freshly-pending row at `now` with the supplied attempts.
    /// Returns the inserted row by id (re-read so timestamps round-trip
    /// through the storage layer exactly as production callers would see).
    async fn insert_pending_run(
        db: &AtomicCore,
        task_id: &str,
        now: chrono::DateTime<Utc>,
        max_attempts: i32,
    ) -> TaskRun {
        let id = uuid::Uuid::now_v7().to_string();
        let now_str = now.to_rfc3339();
        let row = TaskRun {
            id: id.clone(),
            task_id: task_id.to_string(),
            subject_id: None,
            state: TaskRunState::Pending,
            trigger: TaskRunTrigger::Schedule,
            attempts: 0,
            max_attempts,
            lease_until: None,
            next_attempt_at: now_str.clone(),
            scope: None,
            result_id: None,
            last_error: None,
            started_at: None,
            finished_at: None,
            created_at: now_str.clone(),
            updated_at: now_str,
        };
        db.storage.insert_task_run_sync(&row).await.unwrap();
        db.storage.get_task_run_sync(&id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn ledger_insert_and_read_round_trip() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;
        let back = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.id, row.id);
        assert_eq!(back.state, TaskRunState::Pending);
        assert_eq!(back.trigger, TaskRunTrigger::Schedule);
        assert_eq!(back.attempts, 0);
        assert_eq!(back.max_attempts, 3);
        assert!(back.lease_until.is_none());
    }

    #[tokio::test]
    async fn ledger_pending_claim_wins_exactly_once_under_contention() {
        // The conditional UPDATE `state = 'pending'` predicate is the only
        // guard against double-claim. Hammer it from many tasks at once and
        // confirm exactly one wins.
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;
        let lease_until = (now + chrono::Duration::minutes(15)).to_rfc3339();

        let mut handles = Vec::new();
        for _ in 0..16 {
            let storage = db.storage.clone();
            let id = row.id.clone();
            let now_str = now.to_rfc3339();
            let lease = lease_until.clone();
            handles.push(tokio::spawn(async move {
                storage
                    .claim_pending_task_run_sync(&id, &now_str, &lease)
                    .await
            }));
        }

        let mut wins = 0;
        for h in handles {
            if h.await.unwrap().unwrap() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "exactly one claimant should win");

        let after = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, TaskRunState::Running);
        assert_eq!(after.attempts, 1, "claim should bump attempts by one");
        assert!(after.lease_until.is_some());
        assert!(after.started_at.is_some());
    }

    #[tokio::test]
    async fn ledger_running_with_live_lease_blocks_second_claim() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;
        let lease = (now + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &lease)
            .await
            .unwrap());

        // Second pending-claim should fail (state is now 'running').
        assert!(!db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &lease)
            .await
            .unwrap());

        // And the reclaim path should fail too — lease is still live.
        let still_now = now.to_rfc3339();
        assert!(!db
            .storage
            .reclaim_expired_task_run_sync(&row.id, &still_now, &lease)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn ledger_expired_lease_reclaimed_without_bumping_attempts() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;

        // First claim, attempts → 1.
        let stale_lease = (now - chrono::Duration::minutes(1)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &stale_lease)
            .await
            .unwrap());
        let after_claim = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_claim.attempts, 1);

        // Pretend a process crash happened — lease is already in the past.
        // Reclaim should succeed and leave attempts at 1.
        let later = now + chrono::Duration::minutes(2);
        let fresh_lease = (later + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .reclaim_expired_task_run_sync(&row.id, &later.to_rfc3339(), &fresh_lease)
            .await
            .unwrap());
        let after_reclaim = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_reclaim.attempts, 1,
            "reclaim must NOT bump attempts — process crash isn't a logic failure"
        );
        assert_eq!(after_reclaim.state, TaskRunState::Running);
        assert_eq!(
            after_reclaim.lease_until.as_deref(),
            Some(fresh_lease.as_str())
        );
    }

    #[tokio::test]
    async fn ledger_heartbeat_extends_lease_only_when_running() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;

        // Heartbeat on a pending row is a no-op (state predicate fails
        // before the lease fence is even checked).
        let lease = (now + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(!db
            .storage
            .heartbeat_task_run_sync(&row.id, &lease, &lease)
            .await
            .unwrap());

        // Claim, then heartbeat extends.
        let first_lease = (now + chrono::Duration::minutes(5)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &first_lease)
            .await
            .unwrap());
        let new_lease = (now + chrono::Duration::minutes(20)).to_rfc3339();
        assert!(db
            .storage
            .heartbeat_task_run_sync(&row.id, &first_lease, &new_lease)
            .await
            .unwrap());
        let after = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.lease_until.as_deref(), Some(new_lease.as_str()));
    }

    #[tokio::test]
    async fn ledger_complete_is_terminal_and_clears_lease() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;
        let lease = (now + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &lease)
            .await
            .unwrap());
        assert!(db
            .storage
            .complete_task_run_sync(&row.id, &lease, Some("result-atom-1"), &now.to_rfc3339())
            .await
            .unwrap());

        let after = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, TaskRunState::Succeeded);
        assert_eq!(after.result_id.as_deref(), Some("result-atom-1"));
        assert!(after.finished_at.is_some());
        assert!(after.lease_until.is_none());

        // A second complete on a terminal row is a no-op.
        assert!(!db
            .storage
            .complete_task_run_sync(&row.id, &lease, Some("result-atom-2"), &now.to_rfc3339())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn ledger_fail_retry_under_max_routes_back_to_pending() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;
        let lease = (now + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &lease)
            .await
            .unwrap());

        let next_attempt = (now + chrono::Duration::minutes(2)).to_rfc3339();
        assert!(db
            .storage
            .fail_task_run_retry_sync(&row.id, &lease, "boom", &now.to_rfc3339(), &next_attempt)
            .await
            .unwrap());

        let after = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, TaskRunState::Pending);
        assert_eq!(after.next_attempt_at, next_attempt);
        assert_eq!(after.last_error.as_deref(), Some("boom"));
        assert!(after.lease_until.is_none());
        assert!(after.started_at.is_none());
        // attempts was bumped by the claim and stays at 1 on retry — the
        // claim is the canonical "started an attempt" marker.
        assert_eq!(after.attempts, 1);
    }

    #[tokio::test]
    async fn ledger_fail_abandon_is_terminal() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 1).await;
        let lease = (now + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &lease)
            .await
            .unwrap());

        assert!(db
            .storage
            .fail_task_run_abandon_sync(&row.id, &lease, "stop", &now.to_rfc3339())
            .await
            .unwrap());

        let after = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, TaskRunState::Abandoned);
        assert_eq!(after.last_error.as_deref(), Some("stop"));
        assert!(after.finished_at.is_some());
        assert!(after.lease_until.is_none());
    }

    #[tokio::test]
    async fn ledger_find_runnable_returns_the_active_row() {
        // Post-V21 there can only ever be one non-terminal row per
        // (task_id, subject_id) — the partial unique index enforces
        // that. find_runnable just needs to surface it when it's due.
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;
        let picked = db
            .storage
            .find_runnable_task_run_sync("task-A", None, &now.to_rfc3339())
            .await
            .unwrap()
            .expect("a runnable row");
        assert_eq!(picked.id, row.id);
    }

    #[tokio::test]
    async fn ledger_partial_unique_blocks_second_active_row() {
        // Direct test of the V21 constraint: a second pending row for
        // the same (task_id, subject_id) must be rejected. Insert via
        // try_insert returns false; the strict `insert_task_run` errors.
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let _first = insert_pending_run(&db, "task-A", now, 3).await;

        let id = uuid::Uuid::now_v7().to_string();
        let now_str = now.to_rfc3339();
        let duplicate = TaskRun {
            id,
            task_id: "task-A".to_string(),
            subject_id: None,
            state: TaskRunState::Pending,
            trigger: TaskRunTrigger::Schedule,
            attempts: 0,
            max_attempts: 3,
            lease_until: None,
            next_attempt_at: now_str.clone(),
            scope: None,
            result_id: None,
            last_error: None,
            started_at: None,
            finished_at: None,
            created_at: now_str.clone(),
            updated_at: now_str,
        };
        let inserted = db
            .storage
            .try_insert_task_run_sync(&duplicate)
            .await
            .unwrap();
        assert!(!inserted, "try_insert must refuse a duplicate active row");
        // Strict insert errors so callers can't accidentally clobber.
        assert!(db.storage.insert_task_run_sync(&duplicate).await.is_err());
    }

    #[tokio::test]
    async fn ledger_claim_or_create_no_dup_when_two_workers_race() {
        // Two workers calling claim_or_create with no existing active
        // row must produce at most one running row total. Without the
        // V21 fence both could insert + claim distinct rows and the
        // report would execute twice.
        let (db, _temp) = create_test_db().await;
        let n = 16;
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let core = db.clone();
            handles.push(tokio::spawn(async move {
                crate::scheduler::ledger::claim_or_create(
                    &core,
                    "task-A",
                    None,
                    TaskRunTrigger::Schedule,
                    3,
                )
                .await
            }));
        }
        let mut claimed = 0;
        for h in handles {
            if let Some(handle) = h.await.unwrap().unwrap() {
                claimed += 1;
                // Drop the handle without completing — leaves row in
                // running, which is fine since we're just counting wins.
                drop(handle);
            }
        }
        assert_eq!(claimed, 1, "exactly one concurrent claim_or_create wins");
        let rows = db
            .storage
            .list_recent_task_runs_sync("task-A", None, 100)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "only one task_runs row exists");
    }

    #[tokio::test]
    async fn ledger_find_runnable_ignores_future_and_terminal_rows() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        // Future-pending row.
        let future_id = uuid::Uuid::now_v7().to_string();
        let future_at = (now + chrono::Duration::hours(1)).to_rfc3339();
        let future_row = TaskRun {
            id: future_id,
            task_id: "task-A".to_string(),
            subject_id: None,
            state: TaskRunState::Pending,
            trigger: TaskRunTrigger::Schedule,
            attempts: 0,
            max_attempts: 3,
            lease_until: None,
            next_attempt_at: future_at.clone(),
            scope: None,
            result_id: None,
            last_error: None,
            started_at: None,
            finished_at: None,
            created_at: future_at.clone(),
            updated_at: future_at,
        };
        db.storage.insert_task_run_sync(&future_row).await.unwrap();

        // Succeeded row (terminal).
        let done_id = uuid::Uuid::now_v7().to_string();
        let done = TaskRun {
            id: done_id,
            task_id: "task-A".to_string(),
            subject_id: None,
            state: TaskRunState::Succeeded,
            trigger: TaskRunTrigger::Schedule,
            attempts: 1,
            max_attempts: 3,
            lease_until: None,
            next_attempt_at: now.to_rfc3339(),
            scope: None,
            result_id: Some("a".to_string()),
            last_error: None,
            started_at: Some(now.to_rfc3339()),
            finished_at: Some(now.to_rfc3339()),
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
        };
        db.storage.insert_task_run_sync(&done).await.unwrap();

        assert!(db
            .storage
            .find_runnable_task_run_sync("task-A", None, &now.to_rfc3339())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn ledger_list_recent_orders_desc_and_respects_limit() {
        // Post-V21 we can only have one non-terminal row at a time, so
        // history rows must be terminal. Insert + claim + complete five
        // times to build a multi-row history under the same task_id.
        let (db, _temp) = create_test_db().await;
        let base = Utc::now();
        for i in 0..5 {
            let row =
                insert_pending_run(&db, "task-A", base + chrono::Duration::seconds(i), 3).await;
            let now_s = (base + chrono::Duration::seconds(i)).to_rfc3339();
            let lease =
                (base + chrono::Duration::seconds(i) + chrono::Duration::minutes(15)).to_rfc3339();
            assert!(db
                .storage
                .claim_pending_task_run_sync(&row.id, &now_s, &lease)
                .await
                .unwrap());
            assert!(db
                .storage
                .complete_task_run_sync(&row.id, &lease, Some("done"), &now_s)
                .await
                .unwrap());
        }
        let rows = db
            .storage
            .list_recent_task_runs_sync("task-A", None, 3)
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        for w in rows.windows(2) {
            assert!(
                w[0].created_at >= w[1].created_at,
                "descending by created_at"
            );
        }
    }

    #[tokio::test]
    async fn ledger_claim_or_create_inserts_then_claims_when_no_row_exists() {
        // High-level happy path: no existing row → fresh pending inserted,
        // claim wins, RunHandle returned, complete succeeds.
        let (db, _temp) = create_test_db().await;
        let handle = ledger::claim_or_create(&db, "task-A", None, TaskRunTrigger::Manual, 3)
            .await
            .unwrap()
            .expect("claimed");
        let id = handle.run().id.clone();
        let won = handle.complete(Some("atom-1".to_string())).await.unwrap();
        assert!(won);

        let after = db.storage.get_task_run_sync(&id).await.unwrap().unwrap();
        assert_eq!(after.state, TaskRunState::Succeeded);
        assert_eq!(after.trigger, TaskRunTrigger::Manual);
    }

    #[tokio::test]
    async fn ledger_claim_or_create_skips_when_running_lease_is_live() {
        // Regression for: when a task already has a running row with an
        // unexpired lease, claim_or_create must NOT insert a duplicate
        // pending row that gets claimed in parallel. The fix is the
        // `find_active_task_run` probe that catches the live-leased row
        // before the insert branch fires.
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;
        let lease = (now + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &lease)
            .await
            .unwrap());

        // Same task is already in flight — claim_or_create returns None.
        let outcome = ledger::claim_or_create(&db, "task-A", None, TaskRunTrigger::Schedule, 3)
            .await
            .unwrap();
        assert!(
            outcome.is_none(),
            "claim_or_create must not start a parallel run while a lease is live"
        );

        // And no duplicate row was inserted.
        let history = db
            .storage
            .list_recent_task_runs_sync("task-A", None, 10)
            .await
            .unwrap();
        assert_eq!(history.len(), 1, "no duplicate row inserted");
    }

    #[tokio::test]
    async fn ledger_claim_or_create_skips_when_pending_backoff_unexpired() {
        // Regression for the same dup-row class: a row that failed and
        // backed off into pending with `next_attempt_at` in the future
        // must also block a parallel claim_or_create from inserting a
        // duplicate. The retry window is implicitly owned by whoever
        // wrote the backoff timestamp.
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let future_id = uuid::Uuid::now_v7().to_string();
        let future_at = (now + chrono::Duration::minutes(5)).to_rfc3339();
        let row = TaskRun {
            id: future_id,
            task_id: "task-A".to_string(),
            subject_id: None,
            state: TaskRunState::Pending,
            trigger: TaskRunTrigger::Schedule,
            attempts: 1,
            max_attempts: 3,
            lease_until: None,
            next_attempt_at: future_at.clone(),
            scope: None,
            result_id: None,
            last_error: Some("transient".to_string()),
            started_at: None,
            finished_at: None,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
        };
        db.storage.insert_task_run_sync(&row).await.unwrap();

        let outcome = ledger::claim_or_create(&db, "task-A", None, TaskRunTrigger::Schedule, 3)
            .await
            .unwrap();
        assert!(
            outcome.is_none(),
            "future-backoff row blocks duplicate insert"
        );

        let history = db
            .storage
            .list_recent_task_runs_sync("task-A", None, 10)
            .await
            .unwrap();
        assert_eq!(history.len(), 1, "no duplicate row inserted");
    }

    #[tokio::test]
    async fn ledger_stale_complete_fenced_by_lease_after_reclaim() {
        // Regression for: a worker whose lease has been reclaimed by a
        // peer must not be able to mark the reclaimed (re-attempted) run
        // as succeeded. The lease fence on terminal writers catches this.
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;

        // Worker A claims with an already-stale lease (lease_until < now).
        let stale_lease = (now - chrono::Duration::minutes(1)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &stale_lease)
            .await
            .unwrap());

        // Worker B reclaims a few minutes later — sets a new lease value.
        let later = now + chrono::Duration::minutes(2);
        let fresh_lease = (later + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .reclaim_expired_task_run_sync(&row.id, &later.to_rfc3339(), &fresh_lease)
            .await
            .unwrap());

        // Worker A finally returns and tries to complete with its OLD lease.
        // Storage must refuse — the lease fence does not match.
        let stale_complete = db
            .storage
            .complete_task_run_sync(
                &row.id,
                &stale_lease,
                Some("A's result"),
                &later.to_rfc3339(),
            )
            .await
            .unwrap();
        assert!(!stale_complete, "stale complete must fail the lease fence");

        // Same for retry / abandon.
        let stale_retry = db
            .storage
            .fail_task_run_retry_sync(
                &row.id,
                &stale_lease,
                "A's error",
                &later.to_rfc3339(),
                &(later + chrono::Duration::minutes(5)).to_rfc3339(),
            )
            .await
            .unwrap();
        assert!(!stale_retry, "stale retry must fail the lease fence");

        let stale_abandon = db
            .storage
            .fail_task_run_abandon_sync(&row.id, &stale_lease, "A's error", &later.to_rfc3339())
            .await
            .unwrap();
        assert!(!stale_abandon, "stale abandon must fail the lease fence");

        // Row is still running under B's lease — untouched.
        let still = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.state, TaskRunState::Running);
        assert_eq!(still.lease_until.as_deref(), Some(fresh_lease.as_str()));

        // Worker B can complete using the fresh lease.
        assert!(db
            .storage
            .complete_task_run_sync(
                &row.id,
                &fresh_lease,
                Some("B's result"),
                &later.to_rfc3339(),
            )
            .await
            .unwrap());
        let after = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, TaskRunState::Succeeded);
        assert_eq!(after.result_id.as_deref(), Some("B's result"));
    }

    #[tokio::test]
    async fn ledger_stale_heartbeat_fenced_by_lease_after_reclaim() {
        // The heartbeat path uses the same fence — worker A's extension
        // must not silently overwrite worker B's freshly-reclaimed lease.
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;

        let stale_lease = (now - chrono::Duration::minutes(1)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &stale_lease)
            .await
            .unwrap());

        let later = now + chrono::Duration::minutes(2);
        let fresh_lease = (later + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .reclaim_expired_task_run_sync(&row.id, &later.to_rfc3339(), &fresh_lease)
            .await
            .unwrap());

        let extended_by_a = (later + chrono::Duration::minutes(30)).to_rfc3339();
        let stale_heartbeat = db
            .storage
            .heartbeat_task_run_sync(&row.id, &stale_lease, &extended_by_a)
            .await
            .unwrap();
        assert!(
            !stale_heartbeat,
            "stale heartbeat must fail the lease fence"
        );

        let after = db
            .storage
            .get_task_run_sync(&row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.lease_until.as_deref(),
            Some(fresh_lease.as_str()),
            "B's lease must remain untouched"
        );
    }

    #[tokio::test]
    async fn ledger_find_active_returns_running_row_regardless_of_lease() {
        // find_active is the probe claim_or_create uses to detect "task
        // already has work in flight". It must return rows that
        // find_runnable_task_run would skip — specifically, running rows
        // with a live lease.
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 3).await;
        let live_lease = (now + chrono::Duration::hours(1)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &live_lease)
            .await
            .unwrap());

        // find_runnable skips it (live lease).
        assert!(db
            .storage
            .find_runnable_task_run_sync("task-A", None, &now.to_rfc3339())
            .await
            .unwrap()
            .is_none());

        // find_active returns it.
        let active = db
            .storage
            .find_active_task_run_sync("task-A", None)
            .await
            .unwrap()
            .expect("active row");
        assert_eq!(active.id, row.id);
        assert_eq!(active.state, TaskRunState::Running);
    }

    #[tokio::test]
    async fn ledger_find_active_ignores_terminal_rows() {
        let (db, _temp) = create_test_db().await;
        let now = Utc::now();
        let row = insert_pending_run(&db, "task-A", now, 1).await;
        let lease = (now + chrono::Duration::minutes(15)).to_rfc3339();
        assert!(db
            .storage
            .claim_pending_task_run_sync(&row.id, &now.to_rfc3339(), &lease)
            .await
            .unwrap());
        assert!(db
            .storage
            .complete_task_run_sync(&row.id, &lease, Some("done"), &now.to_rfc3339())
            .await
            .unwrap());

        assert!(db
            .storage
            .find_active_task_run_sync("task-A", None)
            .await
            .unwrap()
            .is_none());
    }

    // ==================== Reports primitive (V20) ====================
    //
    // Phase 3 collapsed the legacy briefing path onto this primitive; the
    // seeded "Daily Briefing" report is now the only thing producing daily
    // synthesis atoms. These tests cover schema CRUD, scope resolution
    // (the hot path of run_report), and the empty-scope short-circuit —
    // the LLM-driven full-loop tests live in the integration suite where
    // a wiremock provider is already wired.

    use crate::models::{
        CitationPolicy, ContextScopeMode, ContextScopeWindow, CreateReportRequest, ReportFinding,
        ReportFindingCitation, SourceScopeWindow, UpdateReportRequest,
    };
    use crate::reports::{run_report, RunOutcome};

    fn basic_report(name: &str, schedule: &str) -> CreateReportRequest {
        CreateReportRequest {
            name: name.to_string(),
            research_prompt: "investigate".to_string(),
            schedule: schedule.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn report_crud_round_trip() {
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(basic_report("Daily", "0 0 7 * * *"))
            .await
            .unwrap();
        assert_eq!(r.name, "Daily");
        assert_eq!(r.citation_policy, CitationPolicy::SourceOnly);
        assert!(r.enabled);

        let listed = db.list_reports().await.unwrap();
        assert_eq!(listed.len(), 1);

        let got = db.get_report(&r.id).await.unwrap().unwrap();
        assert_eq!(got.id, r.id);

        let updated = db
            .update_report(
                &r.id,
                UpdateReportRequest {
                    name: Some("Renamed".into()),
                    citation_policy: Some(CitationPolicy::SourceAndContext),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "Renamed");
        assert_eq!(updated.citation_policy, CitationPolicy::SourceAndContext);

        db.set_report_enabled(&r.id, false).await.unwrap();
        let after_disable = db.get_report(&r.id).await.unwrap().unwrap();
        assert!(!after_disable.enabled);
        // list_enabled_reports skips disabled rows.
        assert!(db.list_enabled_reports().await.unwrap().is_empty());

        db.delete_report(&r.id).await.unwrap();
        assert!(db.list_reports().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn report_create_rejects_invalid_cron_as_validation() {
        // Invalid user input must surface as `Validation` so the HTTP
        // layer maps it to 400, not 500.
        let (db, _temp) = create_test_db().await;
        let err = db
            .create_report(basic_report("Bad", "this is not a cron"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AtomicCoreError::Validation(_)),
            "expected Validation variant, got: {err:?}"
        );
        assert!(format!("{err}").contains("cron"), "got: {err}");
    }

    #[tokio::test]
    async fn report_create_rejects_invalid_timezone_as_validation() {
        let (db, _temp) = create_test_db().await;
        let req = CreateReportRequest {
            schedule_tz: Some("Not/A_Zone".to_string()),
            ..basic_report("Bad TZ", "0 0 7 * * *")
        };
        let err = db.create_report(req).await.unwrap_err();
        assert!(
            matches!(err, AtomicCoreError::Validation(_)),
            "expected Validation variant, got: {err:?}"
        );
        assert!(format!("{err}").contains("timezone"), "got: {err}");
    }

    #[tokio::test]
    async fn report_update_rejects_invalid_cron_as_validation() {
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(basic_report("Initial", "0 0 7 * * *"))
            .await
            .unwrap();
        let err = db
            .update_report(
                &r.id,
                UpdateReportRequest {
                    schedule: Some("nope".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, AtomicCoreError::Validation(_)),
            "expected Validation variant, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn scope_source_iso8601_duration_window_filters_old_atoms() {
        let (db, _temp) = create_test_db().await;
        let recent = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Recent".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        // Backdate one atom so the window excludes it.
        let old = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Old".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        conn.execute(
            "UPDATE atoms SET created_at = '1970-01-01T00:00:00Z' WHERE id = ?1",
            rusqlite::params![old.atom.id],
        )
        .unwrap();
        drop(conn);

        let r = db
            .create_report(CreateReportRequest {
                source_scope_window: Some(SourceScopeWindow::Duration("P7D".into())),
                ..basic_report("Recent only", "0 0 * * * *")
            })
            .await
            .unwrap();

        let now = chrono::Utc::now();
        let resolved = crate::reports::scope::resolve_source(&db, &r, now)
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> =
            resolved.atoms.iter().map(|a| a.atom.id.as_str()).collect();
        assert!(ids.contains(recent.atom.id.as_str()));
        assert!(!ids.contains(old.atom.id.as_str()));
    }

    #[tokio::test]
    async fn scope_source_since_last_run_uses_last_run_at() {
        let (db, _temp) = create_test_db().await;
        let _early = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Early".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        // Stamp last_run_at to "now" so the next-created atom is the only
        // thing the report should see.
        let last_run = chrono::Utc::now().to_rfc3339();
        // Sleep enough to ensure created_at > last_run strictly. SQLite's
        // RFC3339 resolution is to the second.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let late = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Late".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let mut r = db
            .create_report(CreateReportRequest {
                source_scope_window: Some(SourceScopeWindow::SinceLastRun),
                ..basic_report("Late only", "0 0 * * * *")
            })
            .await
            .unwrap();
        r.last_run_at = Some(last_run);

        let resolved = crate::reports::scope::resolve_source(&db, &r, chrono::Utc::now())
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> =
            resolved.atoms.iter().map(|a| a.atom.id.as_str()).collect();
        assert!(ids.contains(late.atom.id.as_str()));
        assert_eq!(ids.len(), 1, "only the late atom should be in scope");
    }

    #[tokio::test]
    async fn scope_source_tag_subtree_includes_descendants() {
        let (db, _temp) = create_test_db().await;
        let topics = get_seeded_tag(&db, "Topics");
        let parent = db.create_tag("AI", Some(&topics.id)).await.unwrap();
        let child = db.create_tag("LLMs", Some(&parent.id)).await.unwrap();

        let direct = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Direct".into(),
                    tag_ids: vec![parent.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let descendant = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Descendant".into(),
                    tag_ids: vec![child.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let _unrelated = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Unrelated".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let r = db
            .create_report(CreateReportRequest {
                source_scope_tag_ids: vec![parent.id.clone()],
                ..basic_report("AI subtree", "0 0 * * * *")
            })
            .await
            .unwrap();
        let resolved = crate::reports::scope::resolve_source(&db, &r, chrono::Utc::now())
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> =
            resolved.atoms.iter().map(|a| a.atom.id.as_str()).collect();
        assert!(ids.contains(direct.atom.id.as_str()), "direct match");
        assert!(
            ids.contains(descendant.atom.id.as_str()),
            "descendant should be included via subtree expansion"
        );
        assert_eq!(ids.len(), 2);
    }

    #[tokio::test]
    async fn scope_source_kind_filter_excludes_findings_by_default() {
        let (db, _temp) = create_test_db().await;
        let captured = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Captured".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let report_atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "# Pretend finding".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        // Mark the second as kind=report (phase 2 production path will
        // be `write_finding_transactionally`; here we stamp directly).
        stamp_report_kind(&db, &report_atom.atom.id);

        let r = db
            .create_report(basic_report("Default kinds", "0 0 * * * *"))
            .await
            .unwrap();
        let resolved = crate::reports::scope::resolve_source(&db, &r, chrono::Utc::now())
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> =
            resolved.atoms.iter().map(|a| a.atom.id.as_str()).collect();
        assert!(ids.contains(captured.atom.id.as_str()));
        assert!(!ids.contains(report_atom.atom.id.as_str()));
    }

    #[tokio::test]
    async fn context_filter_excludes_source_and_prior_findings() {
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(basic_report("ctx", "0 0 * * * *"))
            .await
            .unwrap();

        // Set up: a source batch + a prior finding linked to this report.
        let source_atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "source".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        let prior_finding = db
            .create_atom(
                CreateAtomRequest {
                    content: "prior".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        stamp_report_kind(&db, &prior_finding.atom.id);
        let prov = ReportFinding {
            finding_atom_id: prior_finding.atom.id.clone(),
            report_id: Some(r.id.clone()),
            run_id: None,
            report_name_snapshot: r.name.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        // Use the storage path directly to seed provenance without going
        // through the full transactional helper.
        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO report_findings
                (finding_atom_id, report_id, run_id, report_name_snapshot, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                prov.finding_atom_id,
                prov.report_id,
                prov.run_id,
                prov.report_name_snapshot,
                prov.created_at,
            ],
        )
        .unwrap();
        drop(conn);

        let source = crate::reports::scope::ResolvedSource {
            atoms: vec![source_atom.clone()],
            total_in_scope: 1,
            since_cutoff: None,
        };
        let ctx = crate::reports::scope::build_context_filter(&db, &r, &source, chrono::Utc::now())
            .await
            .unwrap();
        assert!(ctx.excluded_atom_ids.contains(&source_atom.atom.id));
        assert!(ctx.excluded_atom_ids.contains(&prior_finding.atom.id));
    }

    #[tokio::test]
    async fn context_filter_older_than_source_uses_source_cutoff() {
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(CreateReportRequest {
                context_scope_window: Some(ContextScopeWindow::OlderThanSource),
                ..basic_report("ctx-older", "0 0 * * * *")
            })
            .await
            .unwrap();
        let source = crate::reports::scope::ResolvedSource {
            atoms: vec![],
            total_in_scope: 0,
            since_cutoff: Some("2026-05-01T00:00:00Z".to_string()),
        };
        let ctx = crate::reports::scope::build_context_filter(&db, &r, &source, chrono::Utc::now())
            .await
            .unwrap();
        match ctx.time_window {
            Some(crate::reports::scope::TimeWindow::Before(c)) => {
                assert_eq!(c, "2026-05-01T00:00:00Z");
            }
            other => panic!("expected Before window, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_filter_same_as_source_inherits_tags() {
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(CreateReportRequest {
                source_scope_tag_ids: vec!["tag-a".to_string(), "tag-b".to_string()],
                context_scope_mode: ContextScopeMode::SameAsSource,
                ..basic_report("ctx-same", "0 0 * * * *")
            })
            .await
            .unwrap();
        let source = crate::reports::scope::ResolvedSource {
            atoms: vec![],
            total_in_scope: 0,
            since_cutoff: None,
        };
        let ctx = crate::reports::scope::build_context_filter(&db, &r, &source, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(ctx.tag_ids, vec!["tag-a".to_string(), "tag-b".to_string()]);
    }

    #[tokio::test]
    async fn run_report_empty_scope_succeeds_without_llm_or_atom() {
        // No atoms in the DB → scope resolves to empty → run_report
        // short-circuits to RunOutcome::EmptyScope without calling the
        // LLM provider (which isn't configured in this test). The cache
        // advances `last_run_at`; the ledger row terminates `succeeded`.
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(basic_report("empty", "0 0 * * * *"))
            .await
            .unwrap();
        let before = db.list_reports().await.unwrap()[0].last_run_at.clone();
        let outcome = run_report(&db, &r, TaskRunTrigger::Manual).await.unwrap();
        match outcome {
            RunOutcome::EmptyScope { .. } => {}
            other => panic!("expected EmptyScope, got {other:?}"),
        }
        // No finding atom written.
        let findings = db.list_findings_for_report(&r.id, 10).await.unwrap();
        assert!(findings.is_empty());
        // Cache advanced.
        let after = db.list_reports().await.unwrap()[0].last_run_at.clone();
        assert!(after.is_some());
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn empty_scope_watermark_uses_resolution_time_not_completion() {
        // Regression: `last_run_at` after a run must reflect the moment we
        // resolved scope — not the (later) moment the run finished. Atoms
        // captured during the run belong to the *next* batch; advancing
        // past completion would silently swallow them.
        //
        // Empty-scope path is sufficient — it skips the LLM and exits with
        // a watermark from `scope_resolution_time` at the top of execute().
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(basic_report("watermark", "0 0 * * * *"))
            .await
            .unwrap();
        let before = chrono::Utc::now();
        let outcome = run_report(&db, &r, TaskRunTrigger::Manual).await.unwrap();
        let after = chrono::Utc::now();
        assert!(matches!(outcome, RunOutcome::EmptyScope { .. }));
        let last_run = db.list_reports().await.unwrap()[0]
            .last_run_at
            .clone()
            .expect("last_run_at set");
        let stamped = chrono::DateTime::parse_from_rfc3339(&last_run).unwrap();
        assert!(
            stamped >= before && stamped <= after,
            "watermark {stamped} should be inside the run window [{before}, {after}]"
        );
    }

    #[tokio::test]
    async fn run_report_skips_when_active_run_exists() {
        // claim_or_create returns None when a run is already in flight.
        // We simulate that by inserting a `task_runs` row directly in the
        // running state with a live lease.
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(basic_report("dup", "0 0 * * * *"))
            .await
            .unwrap();
        let task_id = format!("report::{}", r.id);
        let now = chrono::Utc::now();
        let lease_until = (now + chrono::Duration::minutes(15)).to_rfc3339();
        let id = uuid::Uuid::now_v7().to_string();
        let row = crate::models::TaskRun {
            id,
            task_id,
            subject_id: None,
            state: crate::models::TaskRunState::Running,
            trigger: TaskRunTrigger::Schedule,
            attempts: 1,
            max_attempts: 3,
            lease_until: Some(lease_until),
            next_attempt_at: now.to_rfc3339(),
            scope: None,
            result_id: None,
            last_error: None,
            started_at: Some(now.to_rfc3339()),
            finished_at: None,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
        };
        db.storage.insert_task_run_sync(&row).await.unwrap();

        let outcome = run_report(&db, &r, TaskRunTrigger::Manual).await.unwrap();
        match outcome {
            RunOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_finding_transactionally_persists_everything() {
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(basic_report("write", "0 0 * * * *"))
            .await
            .unwrap();

        // Pre-seed a source atom we'll cite.
        let source = db
            .create_atom(
                CreateAtomRequest {
                    content: "source".into(),
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let atom_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let provenance = ReportFinding {
            finding_atom_id: atom_id.clone(),
            report_id: Some(r.id.clone()),
            run_id: Some("run-1".into()),
            report_name_snapshot: r.name.clone(),
            created_at: now.clone(),
        };
        let citations = vec![ReportFindingCitation {
            finding_atom_id: atom_id.clone(),
            cited_atom_id: source.atom.id.clone(),
            position: 1,
            excerpt: "src excerpt".into(),
        }];
        let req = CreateAtomRequest {
            content: "# Finding\nProse with [1].".into(),
            ..Default::default()
        };
        let written = db
            .storage
            .write_finding_transactionally_sync(&req, &atom_id, &now, &provenance, &citations)
            .await
            .unwrap();
        assert_eq!(written.atom.id, atom_id);
        assert_eq!(written.atom.kind, models::AtomKind::Report);

        // Provenance reads back.
        let prov = db
            .storage
            .get_finding_provenance_sync(&atom_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prov.report_id.as_deref(), Some(r.id.as_str()));
        assert_eq!(prov.run_id.as_deref(), Some("run-1"));

        // Findings listing.
        let findings = db.list_findings_for_report(&r.id, 10).await.unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].0.finding_atom_id, atom_id);

        // Regression: finding atoms must ship `tagging_status = 'skipped'`
        // so the auto-tag pipeline ignores them. Otherwise the LLM
        // tagger runs on agent prose and creates runaway categories.
        assert_eq!(written.atom.tagging_status, "skipped");
    }

    #[tokio::test]
    async fn run_report_first_failure_leaves_last_run_at_null() {
        // Regression: previously the failure branch passed
        // `last_run_at.as_deref().unwrap_or("")` which wrote an empty
        // string. Subsequent ticks parsed `Some("")` as RFC3339, failed,
        // and `is_due` returned false, wedging the retry path. Fix:
        // failure stamping doesn't touch `last_run_at`.
        //
        // We force a failure by inserting a pending row that's already
        // running with an expired lease, then calling run_report: the
        // claim succeeds, scope resolves to empty for the empty DB,
        // which routes to RunInner::Empty — but that's success, not
        // failure. To exercise the failure path we have to make the
        // *runner* itself error out. The cleanest synthetic failure
        // path is to manually call update_report_cache_sync the way
        // the runner does on the failure branch and confirm last_run_at
        // remains None.
        let (db, _temp) = create_test_db().await;
        // Use a once-a-second cron so the next assertion about
        // `is_due` after a failure is deterministic regardless of when
        // in the minute/hour this test runs.
        let r = db
            .create_report(basic_report("first-fail", "* * * * * *"))
            .await
            .unwrap();
        assert!(r.last_run_at.is_none());
        // Simulate the runner's failure-stamping call: no last_run_at,
        // just last_error.
        db.storage
            .update_report_cache_sync(&r.id, None, None, Some(Some("transient blow-up")))
            .await
            .unwrap();
        let after = db.get_report(&r.id).await.unwrap().unwrap();
        assert!(
            after.last_run_at.is_none(),
            "last_run_at must remain None after a first-run failure; got {:?}",
            after.last_run_at
        );
        assert_eq!(after.last_error.as_deref(), Some("transient blow-up"));
        // And `is_due` still recognises the report as runnable. Before
        // the fix this would compute `Some("")` for last_run_at, fail
        // RFC3339 parsing in schedule::is_due, and return false —
        // wedging every retry forever.
        let due_at = chrono::DateTime::parse_from_rfc3339(&after.created_at)
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::seconds(2);
        assert!(crate::reports::schedule::is_due(&after, due_at));
    }

    #[tokio::test]
    async fn run_report_recorded_failure_does_not_advance_cache_for_retry() {
        // Companion to the test above: a success then a failure leaves
        // `last_run_at` at the success timestamp, not bumped or cleared.
        let (db, _temp) = create_test_db().await;
        let r = db
            .create_report(basic_report("recorded", "0 0 * * * *"))
            .await
            .unwrap();
        db.storage
            .update_report_cache_sync(&r.id, Some("2026-05-20T10:00:00Z"), None, None)
            .await
            .unwrap();
        db.storage
            .update_report_cache_sync(&r.id, None, None, Some(Some("blip")))
            .await
            .unwrap();
        let after = db.get_report(&r.id).await.unwrap().unwrap();
        assert_eq!(after.last_run_at.as_deref(), Some("2026-05-20T10:00:00Z"));
        assert_eq!(after.last_error.as_deref(), Some("blip"));
    }

    // ==================== Phase-3 briefing collapse ====================
    //
    // Seed + migration helpers. Each test starts with `create_test_db` (which
    // is post-V22 — the briefings tables already dropped on a fresh DB), and
    // those exercising the historical-data path stamp the legacy `briefings`
    // and `briefing_citations` tables back in via raw SQL before running.

    use crate::reports::seed::{migrate_briefings_to_findings, seed_default_briefing_report};

    /// Recreate the legacy `briefings` / `briefing_citations` tables on a
    /// fresh SQLite DB. Phase-3 seeds a flagged DB so the migration runs
    /// against pre-existing data; this helper simulates that pre-existing
    /// state for tests.
    fn restore_legacy_briefings_tables(db: &AtomicCore) {
        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
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
            "#,
        )
        .unwrap();
    }

    /// Insert a legacy briefing row + its citations directly.
    fn insert_legacy_briefing(
        db: &AtomicCore,
        id: &str,
        content: &str,
        created_at: &str,
        atom_count: i32,
        citations: &[(i32, &str, &str)],
    ) {
        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO briefings (id, content, created_at, atom_count, last_run_at) \
             VALUES (?1, ?2, ?3, ?4, ?3)",
            rusqlite::params![id, content, created_at, atom_count],
        )
        .unwrap();
        for (idx, atom_id, excerpt) in citations {
            conn.execute(
                "INSERT INTO briefing_citations (id, briefing_id, citation_index, atom_id, excerpt) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    id,
                    idx,
                    atom_id,
                    excerpt
                ],
            )
            .unwrap();
        }
    }

    async fn briefing_settings_with(db: &AtomicCore, kv: &[(&str, &str)]) {
        for (k, v) in kv {
            db.storage.set_setting_sync(k, v).await.unwrap();
        }
    }

    #[tokio::test]
    async fn seed_default_briefing_report_idempotent() {
        // First call seeds; second call sees `dashboard.featured_report_id`
        // pointing at an extant report and returns without creating another.
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let after_first = db.list_reports().await.unwrap();
        assert_eq!(after_first.len(), 1);
        let id1 = after_first[0].id.clone();

        seed_default_briefing_report(&db).await.unwrap();
        let after_second = db.list_reports().await.unwrap();
        assert_eq!(after_second.len(), 1, "second seed must not duplicate");
        assert_eq!(after_second[0].id, id1, "same row, not replaced");
    }

    #[tokio::test]
    async fn seed_does_not_recreate_after_user_clears_featured_pointer() {
        // Reproduces the P2 review bug: previously, the idempotency check
        // was anchored on `dashboard.featured_report_id` existing. Clearing
        // the pointer (a legitimate user action) caused the next seed call
        // to create a duplicate Daily Briefing.
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let initial = db.list_reports().await.unwrap();
        assert_eq!(initial.len(), 1);

        // User clears the featured pointer (or deletes the featured
        // report). The seed flag is still set; subsequent seed call
        // must respect it.
        db.set_featured_report_id(None).await.unwrap();

        seed_default_briefing_report(&db).await.unwrap();
        let after = db.list_reports().await.unwrap();
        assert_eq!(
            after.len(),
            1,
            "no duplicate after user clears the featured pointer"
        );
        // Pointer stays cleared — the seed must not silently re-feature.
        assert_eq!(db.get_featured_report_id().await.unwrap(), None);
    }

    #[tokio::test]
    async fn seed_does_not_recreate_after_user_deletes_seeded_report() {
        // Companion case to the above: if the user explicitly deletes the
        // seeded report (which auto-clears the featured pointer), the
        // next seed call must not bring it back.
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let r = &db.list_reports().await.unwrap()[0];
        let id = r.id.clone();

        db.delete_report(&id).await.unwrap();
        assert_eq!(db.list_reports().await.unwrap().len(), 0);

        seed_default_briefing_report(&db).await.unwrap();
        assert_eq!(
            db.list_reports().await.unwrap().len(),
            0,
            "deleted seed stays deleted across reboots"
        );
    }

    #[tokio::test]
    async fn seed_migrates_pre_flag_dbs_without_reseeding() {
        // For DBs seeded before the `default_briefing_seeded` flag existed,
        // the featured pointer is the only signal that seeding has
        // happened. The seed function detects this on first run, marks the
        // flag, and returns without creating a duplicate.
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let initial_id = db.list_reports().await.unwrap()[0].id.clone();

        // Simulate the pre-flag world by removing the flag (the report
        // and the featured pointer both still exist).
        db.storage
            .delete_setting_sync("reports.default_briefing_seeded")
            .await
            .unwrap();

        seed_default_briefing_report(&db).await.unwrap();
        let after = db.list_reports().await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, initial_id, "same row, not replaced");

        // And the flag is now set so subsequent boots take the fast path.
        let flag = db
            .storage
            .get_setting_sync("reports.default_briefing_seeded")
            .await
            .unwrap();
        assert_eq!(flag.as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn seed_pulls_research_prompt_from_briefing_prompt_setting() {
        let (db, _temp) = create_test_db().await;
        briefing_settings_with(&db, &[("briefing_prompt", "look for tensions")]).await;
        seed_default_briefing_report(&db).await.unwrap();
        let r = &db.list_reports().await.unwrap()[0];
        assert_eq!(r.research_prompt, "look for tensions");
        // Legacy key is cleared so the report row is the new source of truth.
        let cleared = db
            .storage
            .get_setting_sync("briefing_prompt")
            .await
            .unwrap();
        assert!(cleared.is_none() || cleared.as_deref() == Some(""));
    }

    #[tokio::test]
    async fn seed_does_not_create_system_tags() {
        // The Reports page is the canonical surface for findings; the
        // `kind='report'` discriminator already does the segregation a
        // system tag would. An earlier draft created `Reports` and
        // `Reports/Briefings` system tags as the seeded report's default
        // output tags — see git history — but they only duplicated the
        // discriminator's work while introducing a name-collision footgun
        // against user-owned tags. Lock in that no auto-tag pollution
        // happens at seed time, and the seeded report's output tags stay
        // empty (matching user-created reports' default).
        fn flatten(tree: &[TagWithCount]) -> Vec<&TagWithCount> {
            let mut out: Vec<&TagWithCount> = Vec::new();
            fn walk<'a>(node: &'a TagWithCount, out: &mut Vec<&'a TagWithCount>) {
                out.push(node);
                for c in &node.children {
                    walk(c, out);
                }
            }
            for n in tree {
                walk(n, &mut out);
            }
            out
        }

        let (db, _temp) = create_test_db().await;
        let pre_tree = db.get_all_tags().await.unwrap();
        let pre_count = flatten(&pre_tree).len();
        seed_default_briefing_report(&db).await.unwrap();
        // Run twice — idempotency contract is preserved separately by
        // the seed flag; we also want to confirm the no-tag promise
        // holds across re-runs.
        seed_default_briefing_report(&db).await.unwrap();
        let post_tree = db.get_all_tags().await.unwrap();
        let post = flatten(&post_tree);
        assert_eq!(
            post.len(),
            pre_count,
            "seed must not create any tags (saw {:?})",
            post.iter().map(|t| &t.tag.name).collect::<Vec<_>>()
        );

        let reports = db.list_reports().await.unwrap();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].output_atom_tags.is_empty(),
            "seeded report's output tags must be empty"
        );
    }

    #[tokio::test]
    async fn seed_sets_dashboard_featured_report_id() {
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let id = db.get_featured_report_id().await.unwrap();
        assert!(id.is_some(), "featured report id is set after seed");
        let report = db.list_reports().await.unwrap();
        assert_eq!(id.as_deref(), Some(report[0].id.as_str()));
    }

    #[tokio::test]
    async fn featured_report_id_cleared_when_report_deleted() {
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let id = db.get_featured_report_id().await.unwrap().unwrap();
        db.delete_report(&id).await.unwrap();
        let after = db.get_featured_report_id().await.unwrap();
        assert!(after.is_none(), "deleting the report clears the pointer");
    }

    #[tokio::test]
    async fn migrate_briefings_to_findings_writes_atoms_with_kind_report() {
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();

        // Need a captured atom to anchor a citation, then a legacy briefing
        // referencing it.
        let cited = create_test_atom(&db, "anchor content").await;
        restore_legacy_briefings_tables(&db);
        insert_legacy_briefing(
            &db,
            "b1",
            "Yesterday's roundup [1].",
            "2026-05-19T10:00:00Z",
            1,
            &[(1, &cited.atom.id, "anchor content")],
        );

        let migrated = migrate_briefings_to_findings(&db).await.unwrap();
        assert_eq!(migrated, 1);

        let featured = db.get_featured_report_id().await.unwrap().unwrap();
        let findings = db.list_findings_for_report(&featured, 10).await.unwrap();
        assert_eq!(findings.len(), 1);
        let (_, atom) = &findings[0];
        assert_eq!(atom.atom.kind, AtomKind::Report);
        assert_eq!(atom.atom.tagging_status, "skipped");
        assert_eq!(atom.atom.content, "Yesterday's roundup [1].");
        // `created_at` preserved.
        assert_eq!(atom.atom.created_at, "2026-05-19T10:00:00Z");
    }

    #[tokio::test]
    async fn migrate_briefings_to_findings_preserves_citations() {
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let a = create_test_atom(&db, "atom one").await;
        let b = create_test_atom(&db, "atom two").await;
        restore_legacy_briefings_tables(&db);
        insert_legacy_briefing(
            &db,
            "b1",
            "Two atoms today [1] and [2].",
            "2026-05-19T10:00:00Z",
            2,
            &[
                (1, &a.atom.id, "atom one excerpt"),
                (2, &b.atom.id, "atom two excerpt"),
            ],
        );

        migrate_briefings_to_findings(&db).await.unwrap();
        let featured = db.get_featured_report_id().await.unwrap().unwrap();
        let findings = db.list_findings_for_report(&featured, 10).await.unwrap();
        let finding_atom_id = &findings[0].1.atom.id;
        let citations = db
            .list_citations_for_finding(finding_atom_id)
            .await
            .unwrap();
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].position, 1);
        assert_eq!(citations[0].cited_atom_id, a.atom.id);
        assert_eq!(citations[0].excerpt, "atom one excerpt");
        assert_eq!(citations[1].position, 2);
        assert_eq!(citations[1].cited_atom_id, b.atom.id);
    }

    #[tokio::test]
    async fn migrate_briefings_to_findings_resumes_after_partial_crash() {
        // Simulate a crash mid-migration: pre-write one of two legacy
        // briefings into a finding atom under the stable
        // `legacy-briefing-{id}` key (as if a prior boot wrote it but
        // crashed before flipping the migration flag). The migration must
        // skip that row on restart and migrate only the second — no
        // duplicates, no fresh-UUID rewrites.
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let anchor = create_test_atom(&db, "anchor").await;
        restore_legacy_briefings_tables(&db);
        insert_legacy_briefing(
            &db,
            "b-first",
            "first [1].",
            "2026-05-18T10:00:00Z",
            1,
            &[(1, &anchor.atom.id, "x")],
        );
        insert_legacy_briefing(
            &db,
            "b-second",
            "second [1].",
            "2026-05-19T10:00:00Z",
            1,
            &[(1, &anchor.atom.id, "y")],
        );

        // Pre-stamp the first briefing as if a prior run had completed
        // its write but crashed before the migration flag flipped.
        {
            let sqlite = db.storage.as_sqlite().unwrap();
            let conn = sqlite.db.conn.lock().unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO atoms (id, content, created_at, updated_at, embedding_status, tagging_status, title, snippet, kind) \
                 VALUES ('legacy-briefing-b-first', 'first [1].', ?1, ?1, 'pending', 'skipped', '', '', 'report')",
                rusqlite::params![now],
            )
            .unwrap();
        }

        let migrated = migrate_briefings_to_findings(&db).await.unwrap();
        // Only the second briefing should be written — the first was
        // already present at its stable id.
        assert_eq!(migrated, 1);

        let featured = db.get_featured_report_id().await.unwrap().unwrap();
        let findings = db.list_findings_for_report(&featured, 10).await.unwrap();
        // One finding written this run + the pre-stamped atom (which the
        // migration left alone). The pre-stamped atom has no
        // report_findings row, so list_findings_for_report still returns
        // exactly the freshly-migrated one.
        assert_eq!(findings.len(), 1);
        assert!(findings[0]
            .1
            .atom
            .id
            .starts_with("legacy-briefing-b-second"));
    }

    #[tokio::test]
    async fn migrate_briefings_to_findings_idempotent() {
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let a = create_test_atom(&db, "anchor").await;
        restore_legacy_briefings_tables(&db);
        insert_legacy_briefing(
            &db,
            "b1",
            "one [1].",
            "2026-05-19T10:00:00Z",
            1,
            &[(1, &a.atom.id, "x")],
        );

        let first = migrate_briefings_to_findings(&db).await.unwrap();
        assert_eq!(first, 1);
        let second = migrate_briefings_to_findings(&db).await.unwrap();
        assert_eq!(second, 0, "flag prevents re-run");
    }

    #[tokio::test]
    async fn migrate_carries_over_last_run_at() {
        // Critique #1: the seeded report's `last_run_at` must inherit from
        // `task.daily_briefing.last_run` so the first reports-loop tick
        // doesn't reprocess weeks of already-briefed atoms.
        let (db, _temp) = create_test_db().await;
        db.storage
            .set_setting_sync("task.daily_briefing.last_run", "2026-05-15T10:00:00Z")
            .await
            .unwrap();
        seed_default_briefing_report(&db).await.unwrap();
        let r = &db.list_reports().await.unwrap()[0];
        assert_eq!(
            r.last_run_at.as_deref(),
            Some("2026-05-15T10:00:00+00:00"),
            "seeded report carries last_run forward (as ISO-8601)"
        );
    }

    #[tokio::test]
    async fn keyword_search_filters_by_kind() {
        // Storage-level test: the new `kinds` parameter on
        // `keyword_search_sync` actually constrains results. Two atoms
        // with the same searchable token, one stamped `kind = 'report'`;
        // a `KindFilter::only(Captured)` search must return only the
        // captured atom and a `KindFilter::All` must return both.
        let (db, _temp) = create_test_db().await;
        let captured = create_test_atom(&db, "elephants march east").await;
        let finding = create_test_atom(&db, "elephants march west").await;
        stamp_report_kind(&db, &finding.atom.id);

        let captured_only = db
            .storage
            .keyword_search_sync(
                "elephants",
                10,
                None,
                None,
                &models::KindFilter::only(models::AtomKind::Captured),
            )
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> = captured_only
            .iter()
            .map(|r| r.atom.atom.id.clone())
            .collect();
        assert!(ids.contains(&captured.atom.id), "captured atom present");
        assert!(!ids.contains(&finding.atom.id), "finding excluded");

        let all = db
            .storage
            .keyword_search_sync("elephants", 10, None, None, &models::KindFilter::All)
            .await
            .unwrap();
        let all_ids: std::collections::HashSet<_> =
            all.iter().map(|r| r.atom.atom.id.clone()).collect();
        assert!(
            all_ids.contains(&captured.atom.id) && all_ids.contains(&finding.atom.id),
            "KindFilter::All returns both kinds"
        );
    }

    #[tokio::test]
    async fn featured_report_id_is_per_database() {
        // Critique #12: the per-DB pointer must isolate. Two independent
        // DBs each get their own seed → distinct `featured_report_id` →
        // setting one must not bleed into the other. The actual multi-DB
        // server holds a shared registry, but the relevant invariant is
        // that the value goes through `core.storage()` (per-DB settings
        // table), not `core.set_setting()` (registry-routed).
        let (db_a, _ta) = create_test_db().await;
        let (db_b, _tb) = create_test_db().await;
        seed_default_briefing_report(&db_a).await.unwrap();
        seed_default_briefing_report(&db_b).await.unwrap();
        let id_a = db_a.get_featured_report_id().await.unwrap().unwrap();
        let id_b = db_b.get_featured_report_id().await.unwrap().unwrap();
        assert_ne!(id_a, id_b, "each DB has its own report id");

        // Clearing on A leaves B untouched.
        db_a.set_featured_report_id(None).await.unwrap();
        assert!(db_a.get_featured_report_id().await.unwrap().is_none());
        assert_eq!(
            db_b.get_featured_report_id().await.unwrap(),
            Some(id_b),
            "DB B's pointer is undisturbed by writes to DB A"
        );
    }

    #[tokio::test]
    async fn end_to_end_seed_then_empty_scope_run() {
        // Fresh DB → seed → manual run. The since-last-run window is set to
        // "now" by the seed (well, None), the scope is empty (no atoms), so
        // run_report short-circuits with EmptyScope and advances last_run_at.
        let (db, _temp) = create_test_db().await;
        seed_default_briefing_report(&db).await.unwrap();
        let id = db.get_featured_report_id().await.unwrap().unwrap();
        let report = db.get_report(&id).await.unwrap().unwrap();
        let outcome = run_report(&db, &report, models::TaskRunTrigger::Manual)
            .await
            .unwrap();
        assert!(matches!(outcome, RunOutcome::EmptyScope { .. }));
        let r = db.get_report(&id).await.unwrap().unwrap();
        assert!(
            r.last_run_at.is_some(),
            "empty-scope success still advances last_run_at"
        );
    }
}
