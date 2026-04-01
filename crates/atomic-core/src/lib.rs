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
//!         source_url: None,
//!         published_at: None,
//!         tag_ids: vec![],
//!     },
//!     |event| match event {
//!         EmbeddingEvent::EmbeddingComplete { atom_id } => println!("Done: {}", atom_id),
//!         _ => {}
//!     },
//! )?;
//! ```

pub mod agent;
pub mod canvas_level;
pub mod chunking;
pub mod chat;
pub mod clustering;
pub mod compaction;
pub mod db;
pub mod embedding;
pub mod error;
pub mod executor;
pub mod extraction;
pub mod ingest;
pub mod import;
pub mod manager;
pub mod models;
pub mod projection;
pub mod providers;
pub mod registry;
pub mod search;
pub mod storage;
pub mod settings;
pub mod tokens;
pub mod wiki;

// Re-exports for convenience
pub use agent::ChatEvent;
pub use db::Database;
pub use embedding::EmbeddingEvent;
pub use error::AtomicCoreError;
pub use models::*;
pub use providers::{ProviderConfig, ProviderType};
pub use search::{SearchMode, SearchOptions};
pub use tokens::ApiTokenInfo;
pub use import::{ImportProgress, ImportResult};
pub use ingest::{IngestionEvent, IngestionRequest, IngestionResult, FeedPollResult};
pub use manager::DatabaseManager;
pub use registry::{DatabaseInfo, OAuthCodeInfo, Registry};

use chrono::Utc;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// Request to create a new atom
#[derive(Debug, Clone)]
pub struct CreateAtomRequest {
    pub content: String,
    pub source_url: Option<String>,
    pub published_at: Option<String>,
    pub tag_ids: Vec<String>,
}

/// Request to update an existing atom
#[derive(Debug, Clone)]
pub struct UpdateAtomRequest {
    pub content: String,
    pub source_url: Option<String>,
    pub published_at: Option<String>,
    pub tag_ids: Option<Vec<String>>,
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
}

impl AtomicCore {
    /// Open an existing database (unencrypted)
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        let db = Arc::new(Database::open(db_path)?);
        let storage = storage::StorageBackend::Sqlite(storage::SqliteStorage::new(db));
        Ok(Self { storage, registry: None })
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
        let db = Database::open_for_server(db_path)?;
        Self::seed_and_backfill(db, registry)
    }

    /// Open for server with optional encryption and registry.
    pub fn open_for_server_encrypted(
        db_path: impl AsRef<Path>,
        passphrase: Option<String>,
        registry: Option<Arc<registry::Registry>>,
    ) -> Result<Self, AtomicCoreError> {
        let db = Database::open_for_server_encrypted(db_path, passphrase)?;
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
    pub fn open_postgres(
        database_url: &str,
        db_id: &str,
        registry: Option<Arc<registry::Registry>>,
    ) -> Result<Self, AtomicCoreError> {
        use storage::PostgresStorage;

        let pg_storage = PostgresStorage::connect(database_url, db_id)?;
        pg_storage.initialize_sync()?;

        let storage = storage::StorageBackend::Postgres(pg_storage);

        // Seed default category tags if tags table is empty
        let all_tags = storage.get_all_tags_impl()?;
        if all_tags.is_empty() {
            for category in &["Topics", "People", "Locations", "Organizations", "Events"] {
                storage.create_tag_impl(category, None)?;
            }
            tracing::info!("Seeded default category tags in Postgres");
        }

        // Seed default settings if no registry and settings table is empty.
        // When a registry exists, settings live there (not in the data DB).
        if registry.is_none() {
            let existing = storage.get_all_settings_sync()?;
            if existing.is_empty() {
                for (key, value) in settings::DEFAULT_SETTINGS {
                    storage.set_setting_sync(key, value)?;
                }
                tracing::info!("Seeded default settings in Postgres");
            }
        }

        Ok(Self { storage, registry })
    }

    /// Create an AtomicCore from an existing PostgresStorage (for multi-db in Postgres mode).
    #[cfg(feature = "postgres")]
    pub fn from_postgres_storage(pg: storage::PostgresStorage) -> Self {
        Self {
            storage: storage::StorageBackend::Postgres(pg),
            registry: None,
        }
    }

    /// Open an existing database or create a new one (unencrypted)
    pub fn open_or_create(db_path: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        let db = Database::open_or_create(db_path)?;
        Self::seed_and_backfill(db, None)
    }

    /// Open or create with optional encryption
    pub fn open_or_create_encrypted(
        db_path: impl AsRef<Path>,
        passphrase: Option<String>,
    ) -> Result<Self, AtomicCoreError> {
        let db = Database::open_or_create_encrypted(db_path, passphrase)?;
        Self::seed_and_backfill(db, None)
    }

    /// Shared initialization: seed default tags, reconcile vec dimension, backfill centroids.
    fn seed_and_backfill(db: Database, registry: Option<Arc<registry::Registry>>) -> Result<Self, AtomicCoreError> {
        // Seed default category tags if tags table is empty
        {
            let conn = db.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            let tag_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM tags", [], |row| row.get(0))
                .unwrap_or(0);
            if tag_count == 0 {
                let now = Utc::now().to_rfc3339();
                for category in &["Topics", "People", "Locations", "Organizations", "Events"] {
                    let id = Uuid::new_v4().to_string();
                    conn.execute(
                        "INSERT OR IGNORE INTO tags (id, name, parent_id, created_at) VALUES (?1, ?2, NULL, ?3)",
                        rusqlite::params![&id, category, &now],
                    )?;
                }
            }
        }

        // Reconcile vec_chunks dimension with the configured embedding model.
        // Only for empty databases (no atoms yet) — e.g. newly created databases
        // whose migration hardcodes float[1536] but the user's model differs.
        if let Some(ref reg) = registry {
            let conn = db.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
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
            let conn = db.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            let has_embeddings: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM atom_chunks WHERE embedding IS NOT NULL)",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            let has_centroids: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM tag_embeddings)",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);

            if has_embeddings && !has_centroids {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT at.tag_id
                     FROM atom_tags at
                     INNER JOIN atom_chunks ac ON at.atom_id = ac.atom_id
                     WHERE ac.embedding IS NOT NULL",
                ).map_err(|e| AtomicCoreError::Database(e))?;

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
        Ok(Self { storage, registry })
    }

    /// Get settings map for passing to background tasks when registry is present.
    /// Returns Some if registry is available, None if settings should be read from data db.
    fn settings_for_background(&self) -> Option<HashMap<String, String>> {
        self.registry.as_ref().and_then(|reg| reg.get_all_settings().ok())
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

    // ==================== Settings ====================

    /// Get all settings, reading from registry if available.
    pub fn get_settings(
        &self,
    ) -> Result<std::collections::HashMap<String, String>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.get_all_settings();
        }
        self.storage.get_all_settings_sync()
    }

    /// Get all settings as a HashMap. Internal helper used by embedding/agent code.
    pub fn get_settings_map(&self) -> Result<HashMap<String, String>, AtomicCoreError> {
        self.get_settings()
    }

    /// Set a setting value.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.set_setting(key, value);
        }
        self.storage.set_setting_sync(key, value)
    }

    // ==================== API Token Operations ====================

    /// Create a new named API token. Returns metadata + the raw token (shown once).
    pub fn create_api_token(
        &self,
        name: &str,
    ) -> Result<(tokens::ApiTokenInfo, String), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.create_api_token(name);
        }
        self.storage.create_api_token_sync(name)
    }

    /// List all API tokens (metadata only, never includes raw token values).
    pub fn list_api_tokens(&self) -> Result<Vec<tokens::ApiTokenInfo>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.list_api_tokens();
        }
        self.storage.list_api_tokens_sync()
    }

    /// Verify a raw API token. Returns token info if valid and not revoked.
    pub fn verify_api_token(
        &self,
        raw_token: &str,
    ) -> Result<Option<tokens::ApiTokenInfo>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.verify_api_token(raw_token);
        }
        self.storage.verify_api_token_sync(raw_token)
    }

    /// Revoke an API token by ID.
    pub fn revoke_api_token(&self, id: &str) -> Result<(), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.revoke_api_token(id);
        }
        self.storage.revoke_api_token_sync(id)
    }

    /// Update the last_used_at timestamp for a token.
    pub fn update_token_last_used(&self, id: &str) -> Result<(), AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.update_token_last_used(id);
        }
        self.storage.update_token_last_used_sync(id)
    }

    /// Migrate legacy server_auth_token from settings to api_tokens table.
    pub fn migrate_legacy_token(&self) -> Result<bool, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.migrate_legacy_token();
        }
        self.storage.migrate_legacy_token_sync()
    }

    /// Ensure at least one API token exists. Creates a "default" token if none exist.
    pub fn ensure_default_token(
        &self,
    ) -> Result<Option<(tokens::ApiTokenInfo, String)>, AtomicCoreError> {
        if let Some(ref reg) = self.registry {
            return reg.ensure_default_token();
        }
        self.storage.ensure_default_token_sync()
    }

    // ==================== Atom Operations ====================

    /// Count total atoms in this database.
    pub fn count_atoms(&self) -> Result<i32, AtomicCoreError> {
        self.storage.count_atoms_impl()
    }

    /// Get all atoms with their tags
    pub fn get_all_atoms(&self) -> Result<Vec<AtomWithTags>, AtomicCoreError> {
        self.storage.get_all_atoms_impl()
    }

    /// Get a single atom by ID
    pub fn get_atom(&self, id: &str) -> Result<Option<AtomWithTags>, AtomicCoreError> {
        self.storage.get_atom_impl(id)
    }

    /// Create a new atom and trigger embedding generation
    ///
    /// The `on_event` callback will be invoked with progress events during
    /// embedding generation and tag extraction (which happens asynchronously).
    pub fn create_atom<F>(
        &self,
        request: CreateAtomRequest,
        on_event: F,
    ) -> Result<AtomWithTags, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let content = request.content.clone();

        let atom_with_tags = self.storage.insert_atom_impl(&id, &request, &now)?;

        // Spawn embedding task (non-blocking)
        embedding::spawn_embedding_task_single_with_settings(
            self.storage.clone(),
            id,
            content,
            on_event,
            self.settings_for_background(),
        );

        Ok(atom_with_tags)
    }

    /// Create multiple atoms in a single transaction and trigger batch embedding.
    ///
    /// All atoms are inserted in one transaction for efficiency. After commit,
    /// a single batch embedding task is spawned for all atoms.
    /// Atoms with a `source_url` that already exists in the database are skipped.
    /// Cap: 1000 atoms per call.
    pub fn create_atoms_bulk<F>(
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

        // Dedup: build set of existing source_urls
        let source_urls: Vec<String> = requests
            .iter()
            .filter_map(|r| r.source_url.clone())
            .collect();
        let existing_urls = self.storage.check_existing_source_urls_sync(&source_urls)?;

        // Filter requests, skipping duplicates
        let mut atoms_to_insert: Vec<(String, CreateAtomRequest, String)> = Vec::with_capacity(requests.len());
        for request in requests {
            if let Some(ref url) = request.source_url {
                if existing_urls.contains(url) {
                    skipped += 1;
                    continue;
                }
            }
            let id = Uuid::new_v4().to_string();
            atoms_to_insert.push((id, request, now.clone()));
        }

        // Bulk insert via storage
        let atoms_with_tags = self.storage.insert_atoms_bulk_impl(&atoms_to_insert)?;

        // Build embedding pairs from inserted atoms
        let embedding_pairs: Vec<(String, String)> = atoms_with_tags
            .iter()
            .map(|awt| (awt.atom.id.clone(), awt.atom.content.clone()))
            .collect();

        // Spawn batch embedding (same pattern as import_obsidian_vault)
        if !embedding_pairs.is_empty() {
            for (atom_id, _) in &embedding_pairs {
                self.storage.set_embedding_status_sync(atom_id, "processing").ok();
            }

            let storage_clone = self.storage.clone();
            let bg_settings = self.settings_for_background();
            executor::spawn(async move {
                match bg_settings {
                    Some(s) => embedding::process_embedding_batch_with_settings(storage_clone, embedding_pairs, false, on_event, s).await,
                    None => embedding::process_embedding_batch(storage_clone, embedding_pairs, false, on_event).await,
                };
            });
        }

        let count = atoms_with_tags.len();
        Ok(BulkCreateResult {
            atoms: atoms_with_tags,
            count,
            skipped,
        })
    }

    /// Update an existing atom and trigger re-embedding
    pub fn update_atom<F>(
        &self,
        id: &str,
        request: UpdateAtomRequest,
        on_event: F,
    ) -> Result<AtomWithTags, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let now = Utc::now().to_rfc3339();
        let content = request.content.clone();

        let atom_with_tags = self.storage.update_atom_impl(id, &request, &now)?;

        // Spawn embedding task (non-blocking)
        embedding::spawn_embedding_task_single_with_settings(
            self.storage.clone(),
            id.to_string(),
            content,
            on_event,
            self.settings_for_background(),
        );

        Ok(atom_with_tags)
    }

    /// Delete an atom
    pub fn delete_atom(&self, id: &str) -> Result<(), AtomicCoreError> {
        self.storage.delete_atom_impl(id)
    }

    /// Get atoms by tag (includes atoms with descendant tags)
    pub fn get_atoms_by_tag(&self, tag_id: &str) -> Result<Vec<AtomWithTags>, AtomicCoreError> {
        self.storage.get_atoms_by_tag_impl(tag_id)
    }

    /// List atoms with pagination, filtering, sorting, and summaries (no full content).
    /// This is the primary frontend-facing method for loading atom lists.
    ///
    /// Supports cursor-based (keyset) pagination: when `cursor` and `cursor_id`
    /// are provided, the query seeks directly to that position, giving O(limit)
    /// performance regardless of page depth. Falls back to OFFSET when no cursor is given.
    pub fn list_atoms(
        &self,
        params: &ListAtomsParams,
    ) -> Result<PaginatedAtoms, AtomicCoreError> {
        self.storage.list_atoms_impl(params)
    }

    /// Get a list of distinct source values with counts (for filter dropdowns).
    pub fn get_source_list(&self) -> Result<Vec<SourceInfo>, AtomicCoreError> {
        self.storage.get_source_list_impl()
    }

    // ==================== Tag Operations ====================

    /// Get all tags with counts (hierarchical tree), no filtering
    pub fn get_all_tags(&self) -> Result<Vec<TagWithCount>, AtomicCoreError> {
        self.storage.get_all_tags_impl()
    }

    /// Get tags with counts, pruning leaf nodes below `min_count`.
    /// Sorted by atom_count descending at every level.
    pub fn get_all_tags_filtered(&self, min_count: i32) -> Result<Vec<TagWithCount>, AtomicCoreError> {
        self.storage.get_all_tags_filtered_impl(min_count)
    }

    /// Get direct children of a specific tag with pagination.
    /// Returns direct children only (with denormalized atom counts); grandchildren
    /// are loaded lazily via subsequent calls.
    pub fn get_tag_children(
        &self,
        parent_id: &str,
        min_count: i32,
        limit: i32,
        offset: i32,
    ) -> Result<PaginatedTagChildren, AtomicCoreError> {
        self.storage.get_tag_children_impl(parent_id, min_count, limit, offset)
    }

    /// Load all tags and their direct counts from the database.
    /// Reads the denormalized atom_count column instead of scanning atom_tags.
    /// Create a new tag
    pub fn create_tag(
        &self,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<Tag, AtomicCoreError> {
        self.storage.create_tag_impl(name, parent_id)
    }

    /// Update a tag
    pub fn update_tag(
        &self,
        id: &str,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<Tag, AtomicCoreError> {
        self.storage.update_tag_impl(id, name, parent_id)
    }

    /// Delete a tag
    pub fn delete_tag(&self, id: &str, recursive: bool) -> Result<(), AtomicCoreError> {
        self.storage.delete_tag_impl(id, recursive)
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
                self.settings_for_background(),
            )
            .await
            .map_err(|e| AtomicCoreError::Search(e));
        }

        // Postgres path: use storage dispatch methods directly
        let settings = self.get_settings()?;
        let config = providers::ProviderConfig::from_settings(&settings);
        let tag_id = options.scope_tag_ids.first().map(|s| s.as_str());

        match options.mode {
            search::SearchMode::Keyword => {
                self.storage.keyword_search_sync(&options.query, options.limit, tag_id)
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
                self.storage.vector_search_sync(
                    &embeddings[0],
                    options.limit,
                    options.threshold,
                    tag_id,
                )
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

                let keyword_results = self.storage.keyword_search_sync(
                    &options.query,
                    options.limit * 2,
                    tag_id,
                )?;

                let semantic_results = if !embeddings.is_empty() && !embeddings[0].is_empty() {
                    self.storage.vector_search_sync(
                        &embeddings[0],
                        options.limit * 2,
                        options.threshold,
                        tag_id,
                    )?
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

    /// Find atoms similar to a given atom
    pub fn find_similar(
        &self,
        atom_id: &str,
        limit: i32,
        threshold: f32,
    ) -> Result<Vec<SimilarAtomResult>, AtomicCoreError> {
        self.storage.find_similar_sync(atom_id, limit, threshold)
    }

    // ==================== Wiki Operations ====================

    /// Build a WikiStrategyContext from current settings.
    fn build_wiki_strategy_context(
        &self,
        tag_id: &str,
        tag_name: &str,
    ) -> Result<(wiki::WikiStrategy, wiki::WikiStrategyContext), AtomicCoreError> {
        const MAX_CROSS_LINK_TAGS: usize = 50;
        let settings_map = self.get_settings()?;
        let config = ProviderConfig::from_settings(&settings_map);
        let model = match config.provider_type {
            ProviderType::Ollama => config.llm_model().to_string(),
            ProviderType::OpenAICompat => config.llm_model().to_string(),
            ProviderType::OpenRouter => settings_map
                .get("wiki_model")
                .cloned()
                .unwrap_or_else(|| "anthropic/claude-sonnet-4.5".to_string()),
        };
        let strategy = wiki::WikiStrategy::from_string(
            settings_map.get("wiki_strategy").map(|s| s.as_str()).unwrap_or("centroid"),
        );
        let related = self.storage.get_related_tags_impl(tag_id, MAX_CROSS_LINK_TAGS)
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
        };
        Ok((strategy, ctx))
    }

    /// Generate a wiki article for a tag
    pub async fn generate_wiki(
        &self,
        tag_id: &str,
        tag_name: &str,
    ) -> Result<WikiArticleWithCitations, AtomicCoreError> {
        tracing::info!(tag_name, tag_id, "[wiki] Generating article");

        let (strategy, ctx) = self.build_wiki_strategy_context(tag_id, tag_name)?;

        let result = wiki::strategy_generate(&strategy, &ctx)
            .await
            .map_err(|e| AtomicCoreError::Wiki(e))?;

        // Extract wiki links from generated content
        let wiki_links = wiki::extract_wiki_links(
            &result.article.id,
            &result.article.content,
            &ctx.linkable_article_names,
        );
        tracing::info!(wiki_links = wiki_links.len(), citations = result.citations.len(), "[wiki] Extracted links and citations");

        // Save to database
        self.storage.save_wiki_with_links_sync(&result.article, &result.citations, &wiki_links)?;

        tracing::info!("[wiki] Article saved successfully");
        Ok(result)
    }

    /// Update an existing wiki article with new content
    pub async fn update_wiki(
        &self,
        tag_id: &str,
        tag_name: &str,
    ) -> Result<WikiArticleWithCitations, AtomicCoreError> {
        tracing::info!(tag_name, tag_id, "[wiki] Updating article");

        let existing = self.get_wiki(tag_id)?
            .ok_or_else(|| AtomicCoreError::Wiki("No existing article to update".to_string()))?;

        let (strategy, ctx) = self.build_wiki_strategy_context(tag_id, tag_name)?;

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
        self.storage.save_wiki_with_links_sync(&result.article, &result.citations, &wiki_links)?;

        tracing::info!("[wiki] Article updated successfully");
        Ok(result)
    }

    /// Get an existing wiki article
    pub fn get_wiki(&self, tag_id: &str) -> Result<Option<WikiArticleWithCitations>, AtomicCoreError> {
        self.storage.get_wiki_sync(tag_id)
    }

    /// Get wiki article status (for checking if update is needed)
    pub fn get_wiki_status(&self, tag_id: &str) -> Result<WikiArticleStatus, AtomicCoreError> {
        self.storage.get_wiki_status_sync(tag_id)
    }

    /// Delete a wiki article
    pub fn delete_wiki(&self, tag_id: &str) -> Result<(), AtomicCoreError> {
        self.storage.delete_wiki_sync(tag_id)
    }

    /// Get tags related to a given tag by semantic connectivity
    pub fn get_related_tags(&self, tag_id: &str, limit: usize) -> Result<Vec<RelatedTag>, AtomicCoreError> {
        self.storage.get_related_tags_impl(tag_id, limit)
    }

    /// Get wiki links (outgoing cross-references) for an article
    pub fn get_wiki_links(&self, tag_id: &str) -> Result<Vec<WikiLink>, AtomicCoreError> {
        self.storage.get_wiki_links_sync(tag_id)
    }

    /// List version history for a wiki article
    pub fn list_wiki_versions(&self, tag_id: &str) -> Result<Vec<WikiVersionSummary>, AtomicCoreError> {
        self.storage.list_wiki_versions_sync(tag_id)
    }

    /// Get a specific wiki article version
    pub fn get_wiki_version(&self, version_id: &str) -> Result<Option<WikiArticleVersion>, AtomicCoreError> {
        self.storage.get_wiki_version_sync(version_id)
    }

    // ==================== Embedding Management ====================

    /// Process all pending embeddings
    pub fn process_pending_embeddings<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        match self.settings_for_background() {
            Some(s) => embedding::process_pending_embeddings_with_settings(self.storage.clone(), on_event, s)
                .map_err(|e| AtomicCoreError::Embedding(e)),
            None => embedding::process_pending_embeddings(self.storage.clone(), on_event)
                .map_err(|e| AtomicCoreError::Embedding(e)),
        }
    }

    /// Reset atoms stuck in 'processing' state back to 'pending'
    pub fn reset_stuck_processing(&self) -> Result<i32, AtomicCoreError> {
        self.storage.reset_stuck_processing_sync()
    }

    /// Retry embedding for a specific atom
    pub fn retry_embedding<F>(&self, atom_id: &str, on_event: F) -> Result<(), AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + 'static,
    {
        let content = self.storage.get_atom_content_impl(atom_id)?
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Atom {} not found", atom_id)))?;

        embedding::spawn_embedding_task_single_with_settings(
            self.storage.clone(),
            atom_id.to_string(),
            content,
            on_event,
            self.settings_for_background(),
        );

        Ok(())
    }

    /// Re-embed all atoms in the database
    pub fn reembed_all_atoms<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let atoms = self.storage.claim_all_for_reembedding_sync()?;
        let count = atoms.len() as i32;

        if count > 0 {
            let storage_clone = self.storage.clone();
            let bg_settings = self.settings_for_background();
            executor::spawn(async move {
                match bg_settings {
                    Some(s) => embedding::process_embedding_batch_with_settings(
                        storage_clone,
                        atoms,
                        false,
                        on_event,
                        s,
                    ).await,
                    None => embedding::process_embedding_batch(
                        storage_clone,
                        atoms,
                        false,
                        on_event,
                    ).await,
                };
            });
        }

        Ok(count)
    }

    /// Retry tagging for a specific atom
    pub fn retry_tagging<F>(&self, atom_id: &str, on_event: F) -> Result<(), AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        // Verify atom exists
        self.storage.get_atom_content_impl(atom_id)?
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Atom {} not found", atom_id)))?;
        // Reset tagging status to pending
        self.storage.set_tagging_status_sync(atom_id, "pending")?;

        let storage = self.storage.clone();
        let atom_id = atom_id.to_string();
        let bg_settings = self.settings_for_background();
        executor::spawn(async move {
            let settings = bg_settings.unwrap_or_default();
            embedding::process_tagging_batch_with_settings(storage, vec![atom_id], on_event, settings).await;
        });

        Ok(())
    }

    // ==================== Clustering ====================

    /// Compute atom clusters based on semantic similarity
    pub fn compute_clusters(
        &self,
        min_similarity: f32,
        min_cluster_size: i32,
    ) -> Result<Vec<AtomCluster>, AtomicCoreError> {
        self.storage.compute_clusters_sync(min_similarity, min_cluster_size)
    }

    /// Save cluster assignments to the database
    pub fn save_clusters(&self, clusters: &[AtomCluster]) -> Result<(), AtomicCoreError> {
        self.storage.save_clusters_sync(clusters)
    }

    /// Get connection counts for hub identification
    pub fn get_connection_counts(
        &self,
        min_similarity: f32,
    ) -> Result<std::collections::HashMap<String, i32>, AtomicCoreError> {
        self.storage.get_connection_counts_sync(min_similarity)
    }

    // ==================== Compaction ====================

    /// Get all tags formatted for LLM analysis
    pub fn get_tags_for_compaction(&self) -> Result<String, AtomicCoreError> {
        self.storage.get_tags_for_compaction_impl()
    }

    /// Apply tag merge operations
    pub fn apply_tag_merges(
        &self,
        merges: &[compaction::TagMerge],
    ) -> Result<compaction::CompactionResult, AtomicCoreError> {
        self.storage.apply_tag_merges_impl(merges)
    }

    // ==================== Chat Operations ====================

    /// Create a new conversation
    pub fn create_conversation(
        &self,
        tag_ids: &[String],
        title: Option<&str>,
    ) -> Result<ConversationWithTags, AtomicCoreError> {
        self.storage.create_conversation_sync(tag_ids, title)
    }

    /// Get all conversations, optionally filtered by tag
    pub fn get_conversations(
        &self,
        filter_tag_id: Option<&str>,
        limit: i32,
        offset: i32,
    ) -> Result<Vec<ConversationWithTags>, AtomicCoreError> {
        self.storage.get_conversations_sync(filter_tag_id, limit, offset)
    }

    /// Get a single conversation with all messages
    pub fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationWithMessages>, AtomicCoreError> {
        self.storage.get_conversation_sync(conversation_id)
    }

    /// Update a conversation (title, archive status)
    pub fn update_conversation(
        &self,
        id: &str,
        title: Option<&str>,
        is_archived: Option<bool>,
    ) -> Result<Conversation, AtomicCoreError> {
        self.storage.update_conversation_sync(id, title, is_archived)
    }

    /// Delete a conversation
    pub fn delete_conversation(&self, id: &str) -> Result<(), AtomicCoreError> {
        self.storage.delete_conversation_sync(id)
    }

    /// Set conversation scope (replace all tags)
    pub fn set_conversation_scope(
        &self,
        conversation_id: &str,
        tag_ids: &[String],
    ) -> Result<ConversationWithTags, AtomicCoreError> {
        self.storage.set_conversation_scope_sync(conversation_id, tag_ids)
    }

    /// Add a single tag to conversation scope
    pub fn add_tag_to_scope(
        &self,
        conversation_id: &str,
        tag_id: &str,
    ) -> Result<ConversationWithTags, AtomicCoreError> {
        self.storage.add_tag_to_scope_sync(conversation_id, tag_id)
    }

    /// Remove a single tag from conversation scope
    pub fn remove_tag_from_scope(
        &self,
        conversation_id: &str,
        tag_id: &str,
    ) -> Result<ConversationWithTags, AtomicCoreError> {
        self.storage.remove_tag_from_scope_sync(conversation_id, tag_id)
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
        F: Fn(ChatEvent) + Send + Sync,
    {
        agent::send_chat_message_with_settings(
            self.storage.clone(),
            conversation_id,
            content,
            on_event,
            self.settings_for_background(),
        )
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e))
    }

    // ==================== Canvas Operations ====================

    /// Get all stored atom positions
    pub fn get_atom_positions(&self) -> Result<Vec<AtomPosition>, AtomicCoreError> {
        self.storage.get_atom_positions_impl()
    }

    /// Bulk save/update atom positions after simulation completes
    pub fn save_atom_positions(&self, positions: &[AtomPosition]) -> Result<(), AtomicCoreError> {
        self.storage.save_atom_positions_impl(positions)
    }

    /// Get atoms with their average embedding vector for similarity calculations
    pub fn get_atoms_with_embeddings(&self) -> Result<Vec<AtomWithEmbedding>, AtomicCoreError> {
        self.storage.get_atoms_with_embeddings_impl()
    }

    /// Compute PCA 2D projection of all atom embeddings and return positioned atoms,
    /// top-K edges per atom, and cluster centroid labels.
    /// Pure read operation — does not persist positions to the database.
    /// Works with both SQLite and Postgres backends via storage dispatch.
    pub fn compute_and_get_canvas_data(&self) -> Result<GlobalCanvasData, AtomicCoreError> {
        // Load all average embeddings via storage abstraction
        let embeddings = self.storage.get_all_embedding_pairs_sync()?;
        if embeddings.is_empty() {
            return Ok(GlobalCanvasData { atoms: vec![], edges: vec![], clusters: vec![] });
        }

        // Run PCA projection (pure math, backend-agnostic)
        let projected = projection::compute_2d_projection(&embeddings);

        // Build position lookup
        let position_map: std::collections::HashMap<String, (f64, f64)> = projected.iter()
            .map(|(id, x, y)| (id.clone(), (*x, *y)))
            .collect();

        // Load atom metadata and merge with projected positions
        let atoms_with_embeddings = self.storage.get_atoms_with_embeddings_impl()?;
        let mut atom_tag_map = self.storage.get_all_atom_tag_ids_sync()?;

        let atoms: Vec<CanvasAtomPosition> = atoms_with_embeddings.iter()
            .filter_map(|awe| {
                let (x, y) = position_map.get(&awe.atom.atom.id)?;
                let (title, _) = extract_title_and_snippet(&awe.atom.atom.content, 60);
                let primary_tag = awe.atom.tags.first().map(|t| t.name.clone());
                let tag_ids = atom_tag_map.remove(&awe.atom.atom.id).unwrap_or_default();
                Some(CanvasAtomPosition {
                    atom_id: awe.atom.atom.id.clone(),
                    x: *x,
                    y: *y,
                    title,
                    primary_tag,
                    tag_count: awe.atom.tags.len() as i32,
                    tag_ids,
                })
            })
            .collect();

        // Load top-2 semantic edges per atom
        let edges = self.storage.get_top_k_canvas_edges_sync(2)?;

        // Compute cluster centroids
        let cluster_data = self.storage.compute_clusters_sync(0.5, 3)?;
        let clusters = Self::build_cluster_centroids(&cluster_data, &position_map);

        Ok(GlobalCanvasData { atoms, edges, clusters })
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
    pub fn get_semantic_edges(&self, min_similarity: f32) -> Result<Vec<SemanticEdge>, AtomicCoreError> {
        self.storage.get_semantic_edges_sync(min_similarity)
    }

    /// Get neighborhood graph for an atom (for local graph view)
    pub fn get_atom_neighborhood(
        &self,
        atom_id: &str,
        depth: i32,
        min_similarity: f32,
    ) -> Result<NeighborhoodGraph, AtomicCoreError> {
        self.storage.get_atom_neighborhood_sync(atom_id, depth, min_similarity)
    }

    /// Rebuild semantic edges for all atoms with embeddings
    pub fn rebuild_semantic_edges(&self) -> Result<i32, AtomicCoreError> {
        self.storage.rebuild_semantic_edges_sync()
    }

    // ==================== Hierarchical Canvas ====================

    /// Get a single level of the hierarchical canvas view.
    ///
    /// - `parent_id = None`: root level showing tag categories
    /// - `parent_id = Some(tag_id)`: children of that tag (sub-tags or atoms)
    /// - `children_hint`: for SemanticCluster drill-down, the list of child IDs to display
    pub fn get_canvas_level(
        &self,
        parent_id: Option<&str>,
        children_hint: Option<Vec<String>>,
    ) -> Result<CanvasLevel, AtomicCoreError> {
        self.storage.get_canvas_level_sync(parent_id, children_hint)
    }

    // ==================== Embedding Status ====================

    /// Get the embedding status for a specific atom
    pub fn get_embedding_status(&self, atom_id: &str) -> Result<String, AtomicCoreError> {
        self.storage.get_embedding_status_impl(atom_id)
    }

    /// Process pending tag extraction for atoms with complete embeddings
    pub fn process_pending_tagging<F>(&self, on_event: F) -> Result<i32, AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let pending_atoms = self.storage.claim_pending_tagging_sync()?;

        let count = pending_atoms.len() as i32;

        if count > 0 {
            let storage = self.storage.clone();
            let bg_settings = self.settings_for_background();
            executor::spawn(async move {
                match bg_settings {
                    Some(s) => embedding::process_tagging_batch_with_settings(storage, pending_atoms, on_event, s).await,
                    None => embedding::process_tagging_batch(storage, pending_atoms, on_event).await,
                };
            });
        }

        Ok(count)
    }

    // ==================== Cluster Cache ====================

    /// Get cached clusters, computing if missing
    pub fn get_clusters(&self) -> Result<Vec<AtomCluster>, AtomicCoreError> {
        self.storage.get_clusters_sync()
    }

    // ==================== Settings with Re-embed ====================

    /// Set a setting, handling embedding dimension changes.
    /// Returns (dimension_changed, pending_reembed_count).
    pub fn set_setting_with_reembed<F>(
        &self,
        key: &str,
        value: &str,
        on_event: F,
    ) -> Result<(bool, i32), AtomicCoreError>
    where
        F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
    {
        let dimension_affecting_keys = ["provider", "embedding_model", "ollama_embedding_model", "openai_compat_embedding_model", "openai_compat_embedding_dimension"];
        let mut dimension_changed = false;

        if dimension_affecting_keys.contains(&key) {
            // Use registry settings if available for dimension calculation
            let current_settings = self.get_settings()?;
            let current_config = ProviderConfig::from_settings(&current_settings);
            let current_dim = current_config.embedding_dimension();

            let mut new_settings = current_settings;
            new_settings.insert(key.to_string(), value.to_string());
            let new_config = ProviderConfig::from_settings(&new_settings);
            let new_dim = new_config.embedding_dimension();

            if current_dim != new_dim {
                tracing::info!(
                    current_dim,
                    new_dim,
                    key,
                    "Embedding dimension changing due to setting change - recreating vec_chunks"
                );
                self.storage.recreate_vector_index_sync(new_dim)?;
                dimension_changed = true;
            }
        }

        // Write to registry if present, otherwise to storage
        if let Some(ref reg) = self.registry {
            reg.set_setting(key, value)?;
        } else {
            self.storage.set_setting_sync(key, value)?;
        }

        let mut pending_count = 0i32;
        if dimension_changed {
            pending_count = self.storage.count_pending_embeddings_sync()?;

            if pending_count > 0 {
                let pending_atoms = self.storage.claim_pending_reembedding_sync()?;

                let storage_clone = self.storage.clone();
                let bg_settings = self.settings_for_background();
                executor::spawn(async move {
                    match bg_settings {
                        Some(s) => embedding::process_embedding_batch_with_settings(
                            storage_clone,
                            pending_atoms,
                            true,
                            on_event,
                            s,
                        ).await,
                        None => embedding::process_embedding_batch(
                            storage_clone,
                            pending_atoms,
                            true, // skip tagging - re-embedding only
                            on_event,
                        ).await,
                    };
                });
            }
        }

        Ok((dimension_changed, pending_count))
    }

    // ==================== Utility Operations ====================

    /// Check sqlite-vec version
    pub fn check_sqlite_vec(&self) -> Result<String, AtomicCoreError> {
        self.storage.check_vector_extension_sync()
    }

    /// Verify that the current provider is properly configured
    pub fn verify_provider_configured(&self) -> Result<bool, AtomicCoreError> {
        let settings_map = self.get_settings()?;
        let config = ProviderConfig::from_settings(&settings_map);

        match config.provider_type {
            ProviderType::OpenRouter => {
                Ok(config.openrouter_api_key.as_ref().map_or(false, |k| !k.is_empty()))
            }
            ProviderType::Ollama => Ok(!config.ollama_host.is_empty()),
            ProviderType::OpenAICompat => Ok(!config.openai_compat_base_url.is_empty()),
        }
    }

    /// Get all wiki articles (summaries for list view)
    pub fn get_all_wiki_articles(&self) -> Result<Vec<WikiArticleSummary>, AtomicCoreError> {
        self.storage.get_all_wiki_articles_sync()
    }

    /// Get cached model capabilities from the settings table.
    pub fn get_cached_capabilities(&self) -> Result<Option<providers::models::ModelCapabilitiesCache>, AtomicCoreError> {
        let json = self.storage.get_setting_sync("model_capabilities_cache")?;
        match json {
            Some(j) => {
                let cache: providers::models::ModelCapabilitiesCache = serde_json::from_str(&j)
                    .map_err(|e| AtomicCoreError::Configuration(format!("Failed to parse capabilities cache: {}", e)))?;
                Ok(Some(cache))
            }
            None => Ok(None),
        }
    }

    /// Save model capabilities cache to the settings table.
    pub fn save_capabilities_cache(&self, cache: &providers::models::ModelCapabilitiesCache) -> Result<(), AtomicCoreError> {
        let json = serde_json::to_string(cache)
            .map_err(|e| AtomicCoreError::Configuration(format!("Failed to serialize capabilities cache: {}", e)))?;
        self.storage.set_setting_sync("model_capabilities_cache", &json)
    }

    // ==================== Import Operations ====================

    /// Import an Obsidian vault into the knowledge base.
    ///
    /// Discovers markdown files, parses notes, creates atoms with hierarchical tags,
    /// and triggers embedding generation. Progress is reported via `on_progress` and
    /// embedding events via `on_event`.
    pub fn import_obsidian_vault<F, P>(
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
            if self.storage.source_url_exists_sync(&note.source_url)? {
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
            match self.storage.insert_atom_impl(
                &atom_id,
                &CreateAtomRequest {
                    content: note.content.clone(),
                    source_url: Some(note.source_url.clone()),
                    published_at: None,
                    tag_ids: vec![],
                },
                &note.created_at,
            ) {
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
            let conn = sqlite.db.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            let mut folder_tag_ids: Vec<String> = Vec::new();
            for htag in &note.folder_tags {
                let parent_id = if htag.parent_path.is_empty() {
                    None
                } else {
                    let parent_index = htag.parent_path.len() - 1;
                    folder_tag_ids.get(parent_index).map(|s| s.as_str())
                };

                if let Some(tag_id) = get_or_create_tag(
                    &conn,
                    &mut tag_cache,
                    &htag.name,
                    parent_id,
                    &mut stats,
                ) {
                    folder_tag_ids.push(tag_id.clone());
                    if let Err(e) = conn.execute(
                        "INSERT OR IGNORE INTO atom_tags (atom_id, tag_id) VALUES (?1, ?2)",
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
                        "INSERT OR IGNORE INTO atom_tags (atom_id, tag_id) VALUES (?1, ?2)",
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
            for (atom_id, _) in &imported_atoms {
                self.storage.set_embedding_status_sync(atom_id, "processing").ok();
            }

            let storage_clone = self.storage.clone();
            let bg_settings = self.settings_for_background();
            executor::spawn(async move {
                match bg_settings {
                    Some(s) => embedding::process_embedding_batch_with_settings(storage_clone, imported_atoms, false, on_event, s).await,
                    None => embedding::process_embedding_batch(storage_clone, imported_atoms, false, on_event).await,
                };
            });
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
        if self.storage.source_url_exists_sync(&request.url)? {
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
        let atom = self.create_atom(
            CreateAtomRequest {
                content: resolved.markdown,
                source_url: Some(request.url.clone()),
                published_at: request.published_at,
                tag_ids: request.tag_ids,
            },
            on_embed,
        )?;

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

        let parsed = ingest::rss::parse_feed(&feed_data)
            .map_err(|e| AtomicCoreError::Ingestion(e))?;

        let feed = self.storage.create_feed_sync(
            &request.url,
            parsed.title.as_deref(),
            parsed.site_url.as_deref(),
            request.poll_interval,
            &request.tag_ids,
        )?;

        // Poll immediately after creation
        let core = self.clone();
        let feed_id = feed.id.clone();
        executor::spawn(async move {
            let _ = core.poll_feed(&feed_id, on_ingest, on_embed).await;
        });

        Ok(feed)
    }

    /// List all feeds.
    pub fn list_feeds(&self) -> Result<Vec<Feed>, AtomicCoreError> {
        self.storage.list_feeds_sync()
    }

    /// Get a single feed by ID.
    pub fn get_feed(&self, id: &str) -> Result<Feed, AtomicCoreError> {
        self.storage.get_feed_sync(id)
    }

    /// Update a feed's settings.
    pub fn update_feed(&self, id: &str, request: UpdateFeedRequest) -> Result<Feed, AtomicCoreError> {
        self.storage.update_feed_sync(
            id,
            None, // title not in UpdateFeedRequest
            request.poll_interval,
            request.is_paused,
            request.tag_ids.as_deref(),
        )
    }

    /// Delete a feed. Does NOT delete atoms created from this feed.
    pub fn delete_feed(&self, id: &str) -> Result<(), AtomicCoreError> {
        self.storage.delete_feed_sync(id)
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
        let feed = self.get_feed(feed_id)?;

        // Fetch feed XML — use shared HTTP client with proper User-Agent
        let feed_data = ingest::fetch::fetch_bytes(&feed.url)
            .await
            .map_err(|e| {
                let err = format!("Cannot fetch feed: {}", e);
                self.update_feed_error(feed_id, &err);
                on_ingest(ingest::IngestionEvent::FeedPollFailed {
                    feed_id: feed_id.to_string(),
                    error: err.clone(),
                });
                AtomicCoreError::Ingestion(err)
            })?;

        let parsed = ingest::rss::parse_feed(&feed_data).map_err(|e| {
            self.update_feed_error(feed_id, &e);
            on_ingest(ingest::IngestionEvent::FeedPollFailed {
                feed_id: feed_id.to_string(),
                error: e.clone(),
            });
            AtomicCoreError::Ingestion(e)
        })?;

        let mut new_items = 0i32;
        let mut skipped = 0i32;
        let mut errors = 0i32;

        for item in &parsed.items {
            // Claim the GUID atomically — if another poll already claimed it, skip.
            if !self.claim_feed_item(feed_id, &item.guid)? {
                continue;
            }

            let link = match &item.link {
                Some(l) => l.clone(),
                None => {
                    self.mark_feed_item_skipped(feed_id, &item.guid, "No link in feed item")?;
                    skipped += 1;
                    continue;
                }
            };

            let request_id = Uuid::new_v4().to_string();
            match ingest::resolve_url(&link, &request_id, &on_ingest).await {
                Ok(resolved) => {
                    match self.create_atom(
                        CreateAtomRequest {
                            content: resolved.markdown,
                            source_url: Some(link),
                            published_at: item.published_at.clone(),
                            tag_ids: feed.tag_ids.clone(),
                        },
                        on_embed.clone(),
                    ) {
                        Ok(atom) => {
                            self.complete_feed_item(feed_id, &item.guid, &atom.atom.id)?;
                            new_items += 1;
                        }
                        Err(e) => {
                            self.mark_feed_item_skipped(feed_id, &item.guid, &e.to_string())?;
                            errors += 1;
                        }
                    }
                }
                Err(reason) => {
                    self.mark_feed_item_skipped(feed_id, &item.guid, &reason)?;
                    skipped += 1;
                }
            }
        }

        // Update feed metadata
        self.storage.mark_feed_polled_sync(feed_id, None)?;
        // Backfill title/site_url from feed data if not already set
        if parsed.title.is_some() || parsed.site_url.is_some() {
            self.storage.backfill_feed_metadata_sync(
                feed_id,
                parsed.title.as_deref(),
                parsed.site_url.as_deref(),
            )?;
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
        let due_feed_ids: Vec<String> = match self.storage.get_due_feeds_sync() {
            Ok(feeds) => feeds.into_iter().map(|f| f.id).collect(),
            Err(_) => return vec![],
        };

        let mut results = Vec::new();
        for feed_id in due_feed_ids {
            match self.poll_feed(&feed_id, on_ingest.clone(), on_embed.clone()).await {
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
    fn claim_feed_item(&self, feed_id: &str, guid: &str) -> Result<bool, AtomicCoreError> {
        self.storage.claim_feed_item_sync(feed_id, guid)
    }

    /// Mark a claimed feed item as successfully ingested with its atom_id.
    fn complete_feed_item(&self, feed_id: &str, guid: &str, atom_id: &str) -> Result<(), AtomicCoreError> {
        self.storage.complete_feed_item_sync(feed_id, guid, atom_id)
    }

    /// Mark a claimed feed item as skipped with a reason.
    fn mark_feed_item_skipped(&self, feed_id: &str, guid: &str, reason: &str) -> Result<(), AtomicCoreError> {
        self.storage.mark_feed_item_skipped_sync(feed_id, guid, reason)
    }

    /// Helper: update a feed's last_error field.
    fn update_feed_error(&self, feed_id: &str, error: &str) {
        let _ = self.storage.mark_feed_polled_sync(feed_id, Some(error));
    }

    /// Get suggested wiki articles (tags without articles, ranked by demand)
    pub fn get_suggested_wiki_articles(&self, limit: i32) -> Result<Vec<SuggestedArticle>, AtomicCoreError> {
        self.storage.get_suggested_wiki_articles_sync(limit)
    }

    /// Recompute centroid embeddings for all tags that have atoms with embeddings.
    /// Useful for backfilling after this feature is added to an existing database.
    pub fn recompute_all_tag_embeddings(&self) -> Result<i32, AtomicCoreError> {
        self.storage.recompute_all_tag_embeddings_sync()
    }
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

    let params: Vec<&dyn rusqlite::ToSql> = atom_ids
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();

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
        let results = stmt.query_map([atom_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        results
    };

    if !center_tags.is_empty() {
        let placeholders: String = center_tags.iter().map(|_| "?").collect::<Vec<_>>().join(",");
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
    let atom_lookup: HashMap<String, Atom> = atom_rows.into_iter().map(|a| (a.id.clone(), a)).collect();

    // Batch fetch tags for all atoms
    let tag_map = get_atom_tags_map_for_ids(conn, &atom_ids)?;

    let mut atoms = Vec::new();
    for aid in &atom_ids {
        if let Some(atom) = atom_lookup.get(aid) {
            let tags = tag_map.get(aid).cloned().unwrap_or_default();
            let depth = *atom_depths.get(aid).unwrap_or(&0);
            atoms.push(NeighborhoodAtom {
                atom: AtomWithTags { atom: atom.clone(), tags },
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
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f32>(2)?))
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
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i32>(2)?))
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
            let (key_a, key_b) = if id_a < id_b { (id_a, id_b) } else { (id_b, id_a) };
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
    (trimmed.starts_with("http://") || trimmed.starts_with("https://")) && !trimmed.contains(' ') // bare URL
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
pub(crate) const ATOM_COLUMNS: &str = "id, content, title, snippet, source_url, source, published_at, created_at, updated_at, COALESCE(embedding_status, 'pending'), COALESCE(tagging_status, 'pending')";

/// Same columns but table-aliased for JOINs.
pub(crate) const ATOM_COLUMNS_A: &str = "a.id, a.content, a.title, a.snippet, a.source_url, a.source, a.published_at, a.created_at, a.updated_at, COALESCE(a.embedding_status, 'pending'), COALESCE(a.tagging_status, 'pending')";

/// Parse an Atom from a row selected with ATOM_COLUMNS.
pub(crate) fn atom_from_row(row: &rusqlite::Row) -> rusqlite::Result<Atom> {
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
    })
}

/// Get tags for a specific atom
pub(crate) fn get_tags_for_atom(conn: &Connection, atom_id: &str) -> Result<Vec<Tag>, AtomicCoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT t.id, t.name, t.parent_id, t.created_at
             FROM tags t
             INNER JOIN atom_tags at ON t.id = at.tag_id
             WHERE at.atom_id = ?1",
        )
        ?;

    let tags = stmt
        .query_map([atom_id], |row| {
            Ok(Tag {
                id: row.get(0)?,
                name: row.get(1)?,
                parent_id: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        ?
        .collect::<Result<Vec<_>, _>>()
        ?;

    Ok(tags)
}

/// Bulk fetch all atom-tag relationships in a single query.
/// Returns a map from atom_id to Vec<Tag>.
pub(crate) fn get_all_atom_tags_map(conn: &Connection) -> Result<std::collections::HashMap<String, Vec<Tag>>, AtomicCoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at
             FROM atom_tags at
             INNER JOIN tags t ON at.tag_id = t.id",
        )?;

    let mut map: std::collections::HashMap<String, Vec<Tag>> = std::collections::HashMap::new();

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Tag {
                    id: row.get(1)?,
                    name: row.get(2)?,
                    parent_id: row.get(3)?,
                    created_at: row.get(4)?,
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
pub(crate) fn get_atom_tags_map_for_ids(conn: &Connection, atom_ids: &[String]) -> Result<std::collections::HashMap<String, Vec<Tag>>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let placeholders = atom_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT at.atom_id, t.id, t.name, t.parent_id, t.created_at
         FROM atom_tags at
         INNER JOIN tags t ON at.tag_id = t.id
         WHERE at.atom_id IN ({})",
        placeholders
    );

    let mut stmt = conn.prepare(&query)?;

    let mut map: std::collections::HashMap<String, Vec<Tag>> = std::collections::HashMap::new();

    let rows = stmt
        .query_map(rusqlite::params_from_iter(atom_ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                Tag {
                    id: row.get(1)?,
                    name: row.get(2)?,
                    parent_id: row.get(3)?,
                    created_at: row.get(4)?,
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
                let child_nodes =
                    build_subtree(Some(&tag.id), children_by_parent, direct_counts, min_count, false);
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

    /// Test utility: Create a test database
    fn create_test_db() -> (AtomicCore, NamedTempFile) {
        let temp_file = NamedTempFile::new().unwrap();
        let db = AtomicCore::open_or_create(temp_file.path()).unwrap();
        (db, temp_file)
    }

    /// Get a seeded category tag by name (e.g., "Topics")
    fn get_seeded_tag(db: &AtomicCore, name: &str) -> Tag {
        let sqlite = db.storage.as_sqlite().unwrap();
        let conn = sqlite.db.conn.lock().unwrap();
        let (id, tag_name, parent_id, created_at): (String, String, Option<String>, String) = conn
            .query_row(
                "SELECT id, name, parent_id, created_at FROM tags WHERE LOWER(name) = LOWER(?1)",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        Tag { id, name: tag_name, parent_id, created_at }
    }

    /// Test utility: Create a test atom
    fn create_test_atom(db: &AtomicCore, content: &str) -> AtomWithTags {
        db.create_atom(
            CreateAtomRequest {
                content: content.to_string(),
                source_url: None,
                published_at: None,
                tag_ids: vec![],
            },
            |_| {}, // no-op callback
        )
        .unwrap()
    }

    // ==================== Atom CRUD Tests ====================

    #[test]
    fn test_create_atom_returns_atom() {
        let (db, _temp) = create_test_db();

        let atom = create_test_atom(&db, "Test content for atom");

        assert!(!atom.atom.id.is_empty());
        assert_eq!(atom.atom.content, "Test content for atom");
        assert_eq!(atom.atom.embedding_status, "pending");
        assert!(atom.tags.is_empty());
    }

    #[test]
    fn test_get_atom_by_id() {
        let (db, _temp) = create_test_db();

        let created = create_test_atom(&db, "Content to retrieve");
        let retrieved = db.get_atom(&created.atom.id).unwrap();

        assert!(retrieved.is_some());
        let atom = retrieved.unwrap();
        assert_eq!(atom.atom.id, created.atom.id);
        assert_eq!(atom.atom.content, "Content to retrieve");
    }

    #[test]
    fn test_get_atom_not_found() {
        let (db, _temp) = create_test_db();

        let result = db.get_atom("nonexistent-id-12345").unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn test_get_all_atoms() {
        let (db, _temp) = create_test_db();

        // Create multiple atoms
        create_test_atom(&db, "First atom");
        create_test_atom(&db, "Second atom");
        create_test_atom(&db, "Third atom");

        let all_atoms = db.get_all_atoms().unwrap();

        assert_eq!(all_atoms.len(), 3);
    }

    #[test]
    fn test_delete_atom() {
        let (db, _temp) = create_test_db();

        let atom = create_test_atom(&db, "Atom to delete");
        let atom_id = atom.atom.id.clone();

        // Verify it exists
        assert!(db.get_atom(&atom_id).unwrap().is_some());

        // Delete it
        db.delete_atom(&atom_id).unwrap();

        // Verify it's gone
        assert!(db.get_atom(&atom_id).unwrap().is_none());
    }

    // ==================== Tag CRUD Tests ====================

    #[test]
    fn test_create_tag_root() {
        let (db, _temp) = create_test_db();

        let tag = db.create_tag("CustomRoot", None).unwrap();

        assert!(!tag.id.is_empty());
        assert_eq!(tag.name, "CustomRoot");
        assert!(tag.parent_id.is_none());
    }

    #[test]
    fn test_seeded_category_tags_exist() {
        let (db, _temp) = create_test_db();
        let all_tags = db.get_all_tags().unwrap();
        let names: Vec<&str> = all_tags.iter().map(|t| t.tag.name.as_str()).collect();
        assert!(names.contains(&"Topics"));
        assert!(names.contains(&"People"));
        assert!(names.contains(&"Locations"));
        assert!(names.contains(&"Organizations"));
        assert!(names.contains(&"Events"));
    }

    #[test]
    fn test_create_tag_with_parent() {
        let (db, _temp) = create_test_db();

        // Use seeded parent tag
        let parent = get_seeded_tag(&db, "Topics");

        // Create child tag
        let child = db.create_tag("AI", Some(&parent.id)).unwrap();

        assert_eq!(child.name, "AI");
        assert_eq!(child.parent_id, Some(parent.id));
    }

    #[test]
    fn test_get_all_tags_hierarchical() {
        let (db, _temp) = create_test_db();

        // Use seeded Topics, add hierarchy: Topics -> AI -> Machine Learning
        let topics = get_seeded_tag(&db, "Topics");
        let ai = db.create_tag("AI", Some(&topics.id)).unwrap();
        let _ml = db.create_tag("Machine Learning", Some(&ai.id)).unwrap();

        let all_tags = db.get_all_tags().unwrap();

        // Should have 6 seeded root tags; find Topics and check its children
        let topics_node = all_tags.iter().find(|t| t.tag.name == "Topics").unwrap();
        assert_eq!(topics_node.children.len(), 1);
        assert_eq!(topics_node.children[0].tag.name, "AI");
        assert_eq!(topics_node.children[0].children.len(), 1);
        assert_eq!(topics_node.children[0].children[0].tag.name, "Machine Learning");
    }

    #[test]
    fn test_delete_tag() {
        let (db, _temp) = create_test_db();

        let tag = db.create_tag("ToDelete", None).unwrap();
        let tag_id = tag.id.clone();

        // Verify it exists in get_all_tags
        let tags_before = db.get_all_tags().unwrap();
        assert!(tags_before.iter().any(|t| t.tag.id == tag_id));

        // Delete it
        db.delete_tag(&tag_id, false).unwrap();

        // Verify it's gone
        let tags_after = db.get_all_tags().unwrap();
        assert!(!tags_after.iter().any(|t| t.tag.id == tag_id));
    }

    // ==================== Atom-Tag Relationship Tests ====================

    #[test]
    fn test_create_atom_with_tags() {
        let (db, _temp) = create_test_db();

        // Create tags first
        let tag1 = db.create_tag("Tag1", None).unwrap();
        let tag2 = db.create_tag("Tag2", None).unwrap();

        // Create atom with tags
        let atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "Tagged content".to_string(),
                    source_url: None,
                    published_at: None,
                    tag_ids: vec![tag1.id.clone(), tag2.id.clone()],
                },
                |_| {},
            )
            .unwrap();

        // Verify tags are attached
        assert_eq!(atom.tags.len(), 2);
        let tag_names: Vec<&str> = atom.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(tag_names.contains(&"Tag1"));
        assert!(tag_names.contains(&"Tag2"));
    }

    #[test]
    fn test_get_atoms_by_tag_includes_descendants() {
        let (db, _temp) = create_test_db();

        // Use seeded Topics, add child: Topics -> AI
        let topics = get_seeded_tag(&db, "Topics");
        let ai = db.create_tag("AI", Some(&topics.id)).unwrap();

        // Create atom tagged with AI (child)
        let atom = db
            .create_atom(
                CreateAtomRequest {
                    content: "AI content".to_string(),
                    source_url: None,
                    published_at: None,
                    tag_ids: vec![ai.id.clone()],
                },
                |_| {},
            )
            .unwrap();

        // Query by parent tag (Topics) should include atoms tagged with AI
        let atoms = db.get_atoms_by_tag(&topics.id).unwrap();

        assert_eq!(atoms.len(), 1);
        assert_eq!(atoms[0].atom.id, atom.atom.id);
    }

    #[test]
    fn test_atom_tag_counts() {
        let (db, _temp) = create_test_db();

        // Use seeded parent tag
        let topics = get_seeded_tag(&db, "Topics");

        // Create 3 atoms with this tag
        for i in 0..3 {
            db.create_atom(
                CreateAtomRequest {
                    content: format!("Atom {}", i),
                    source_url: None,
                    published_at: None,
                    tag_ids: vec![topics.id.clone()],
                },
                |_| {},
            )
            .unwrap();
        }

        // Get tags and check count
        let all_tags = db.get_all_tags().unwrap();
        let topics_tag = all_tags.iter().find(|t| t.tag.name == "Topics").unwrap();

        assert_eq!(topics_tag.atom_count, 3);
    }

    #[test]
    fn test_strip_inline_markdown() {
        // Backslash escapes
        assert_eq!(strip_inline_markdown(r"U\.S\. policy"), "U.S. policy");
        // Bold and italic
        assert_eq!(strip_inline_markdown("**bold** and *italic*"), "bold and italic");
        // Links: keep text, drop URL
        assert_eq!(strip_inline_markdown("[click here](https://example.com)"), "click here");
        // Images: drop entirely
        assert_eq!(strip_inline_markdown("before ![alt](img.png) after"), "before  after");
        // Inline code
        assert_eq!(strip_inline_markdown("use `foo()` here"), "use foo() here");
        // Mixed
        assert_eq!(
            strip_inline_markdown(r"The **U\.S\.** has [a link](http://x.com)"),
            "The U.S. has a link"
        );
    }

}
