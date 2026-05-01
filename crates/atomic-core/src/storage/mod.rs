//! Storage abstraction layer for atomic-core.
//!
//! This module defines the trait hierarchy for database backends and provides
//! the default SQLite implementation. Alternative backends (e.g., Postgres)
//! can be added by implementing the `Storage` supertrait.

pub mod postgres;
pub mod sqlite;
pub mod traits;

pub use sqlite::SqliteStorage;
pub use traits::*;

#[cfg(feature = "postgres")]
pub use postgres::PostgresStorage;

use crate::error::AtomicCoreError;

/// Runtime-dispatched storage backend.
///
/// AtomicCore holds this enum to support both SQLite and Postgres at runtime.
/// For SQLite, sync helper methods are called directly.
/// For Postgres, async trait methods are bridged to sync via `block_on`.
#[derive(Clone)]
pub enum StorageBackend {
    Sqlite(SqliteStorage),
    #[cfg(feature = "postgres")]
    Postgres(PostgresStorage),
}

impl StorageBackend {
    /// Get the underlying SqliteStorage, if this is a SQLite backend.
    /// Used for operations not yet abstracted behind the storage trait
    /// (e.g., embedding pipeline internals that directly use `Arc<Database>`).
    pub(crate) fn as_sqlite(&self) -> Option<&SqliteStorage> {
        match self {
            StorageBackend::Sqlite(s) => Some(s),
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => None,
        }
    }

    /// Get the underlying PostgresStorage, if this is a Postgres backend.
    #[cfg(feature = "postgres")]
    pub(crate) fn as_postgres(&self) -> Option<&PostgresStorage> {
        match self {
            StorageBackend::Sqlite(_) => None,
            StorageBackend::Postgres(s) => Some(s),
        }
    }

    /// Run storage-specific optimization (SQLite PRAGMA optimize, Postgres no-op).
    pub(crate) fn optimize(&self) {
        match self {
            StorageBackend::Sqlite(s) => s.db.optimize(),
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => {} // Postgres handles this automatically
        }
    }

    /// Get pipeline status (embedding counts + failed atoms).
    pub(crate) async fn get_pipeline_status(
        &self,
    ) -> Result<crate::models::PipelineStatus, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.get_pipeline_status_sync())
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(s) => s.get_pipeline_status_impl().await,
        }
    }

    // ---- Hand-written dispatches for methods with consumed-value args ----
    // These can't go through the `dispatch!` macro because the macro's
    // `ReborrowArg` pattern produces a reference, but these sync methods
    // take owned values.

    pub(crate) async fn get_canvas_level_sync(
        &self,
        parent_id: Option<&str>,
        children_hint: Option<Vec<String>>,
    ) -> Result<CanvasLevel, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                let parent_id = parent_id.map(|s| s.to_string());
                tokio::task::spawn_blocking(move || {
                    s.get_canvas_level_sync(parent_id.as_deref(), children_hint)
                })
                .await
                .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(s) => {
                <PostgresStorage as ClusterStore>::get_canvas_level(s, parent_id, children_hint)
                    .await
            }
        }
    }

    pub(crate) async fn enrich_clusters_with_tags_sync(
        &self,
        clusters: Vec<AtomCluster>,
    ) -> Result<Vec<AtomCluster>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.enrich_clusters_with_tags_sync(clusters))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(s) => {
                <PostgresStorage as ClusterStore>::enrich_clusters_with_tags(s, clusters).await
            }
        }
    }

    /// Get the database path (for display).
    pub(crate) fn storage_path(&self) -> &std::path::Path {
        match self {
            StorageBackend::Sqlite(s) => &s.db.db_path,
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => std::path::Path::new("postgres"),
        }
    }
}

impl StorageBackend {
    // ==================== Health dispatch methods ====================
    // These are not part of the Storage trait — health is an internal concern.
    // Postgres returns an error for all health operations (not yet supported).

    pub(crate) async fn health_check_data_sync(
        &self,
    ) -> Result<crate::storage::sqlite::health::HealthRawData, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.health_check_data_impl())
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Err(AtomicCoreError::DatabaseOperation(
                "health checks not yet supported on Postgres storage".to_string(),
            )),
        }
    }

    pub(crate) async fn store_health_report_sync(
        &self,
        report: &crate::health::audit::StoredHealthReport,
    ) -> Result<(), AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                let report = report.clone();
                tokio::task::spawn_blocking(move || s.store_health_report_impl(&report))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(()),
        }
    }

    pub(crate) async fn get_latest_health_report_sync(
        &self,
    ) -> Result<Option<crate::health::HealthReport>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.get_latest_health_report_impl())
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(None),
        }
    }

    pub(crate) async fn get_health_reports_sync(
        &self,
        limit: i32,
    ) -> Result<Vec<crate::health::audit::StoredHealthReport>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.get_health_reports_impl(limit))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(vec![]),
        }
    }

    pub(crate) async fn log_fix_action_sync(
        &self,
        log: &crate::health::audit::HealthFixLog,
    ) -> Result<(), AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                let log = log.clone();
                tokio::task::spawn_blocking(move || s.log_fix_action_impl(&log))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(()),
        }
    }

    pub(crate) async fn get_fix_log_sync(
        &self,
        fix_id: &str,
    ) -> Result<Option<crate::health::audit::HealthFixLog>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                let fix_id = fix_id.to_string();
                tokio::task::spawn_blocking(move || s.get_fix_log_impl(&fix_id))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(None),
        }
    }

    pub(crate) async fn get_recent_fixes_sync(
        &self,
        limit: i32,
    ) -> Result<Vec<crate::health::audit::HealthFixLog>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.get_recent_fixes_impl(limit))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(vec![]),
        }
    }

    pub(crate) async fn mark_fix_undone_sync(&self, fix_id: &str) -> Result<(), AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                let fix_id = fix_id.to_string();
                tokio::task::spawn_blocking(move || s.mark_fix_undone_impl(&fix_id))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(()),
        }
    }

    pub(crate) async fn reset_skipped_untagged_to_pending_sync(
        &self,
    ) -> Result<i32, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || {
                    s.reset_skipped_untagged_to_pending_impl()
                })
                .await
                .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(0),
        }
    }

    // ==================== Link resolution dispatch ====================

    pub(crate) async fn get_link_candidate_atoms_sync(
        &self,
    ) -> Result<Vec<(String, String, Option<String>)>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.get_link_candidate_atoms_impl())
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(vec![]),
        }
    }

    pub(crate) async fn find_atoms_by_source_urls_sync(
        &self,
        urls: Vec<String>,
    ) -> Result<std::collections::HashMap<String, String>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.find_atoms_by_source_urls_impl(&urls))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(std::collections::HashMap::new()),
        }
    }

    pub(crate) async fn find_atom_by_wikilink_name_sync(
        &self,
        name: String,
        vault_prefix: String,
    ) -> Result<Option<(String, String)>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || {
                    s.find_atom_by_wikilink_name_impl(&name, &vault_prefix)
                })
                .await
                .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(None),
        }
    }
    pub(crate) async fn count_chunk_hash_occurrences_sync(
        &self,
        hashes: &[String],
    ) -> Result<std::collections::HashMap<String, i64>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                let hashes = hashes.to_vec();
                tokio::task::spawn_blocking(move || s.count_chunk_hash_occurrences_impl(&hashes))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(std::collections::HashMap::new()),
        }
    }

    pub(crate) async fn delete_vec_chunks_by_ids_sync(
        &self,
        chunk_ids: &[String],
    ) -> Result<(), AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                let chunk_ids = chunk_ids.to_vec();
                tokio::task::spawn_blocking(move || s.delete_vec_chunks_by_ids_impl(&chunk_ids))
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(()),
        }
    }

    pub(crate) async fn backfill_content_hashes_sync(&self) -> Result<usize, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.backfill_content_hashes_impl())
                    .await
                    .map_err(join_err)?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_) => Ok(0),
        }
    }
}

// ==================== Async dispatch methods ====================
//
// Each method dispatches to either the SqliteStorage sync helper
// or the PostgresStorage async trait method.
// SQLite: sync call wrapped in `spawn_blocking` so it doesn't tie up the
// async executor thread under concurrent server load.
// Postgres: native async — the sqlx future runs on the caller's runtime.

use crate::compaction::{CompactionResult, TagMerge};
use crate::models::*;
use crate::{CreateAtomRequest, ListAtomsParams, UpdateAtomRequest};
use std::collections::{HashMap, HashSet};

/// Maps a `spawn_blocking` JoinError into `AtomicCoreError`.
fn join_err(e: tokio::task::JoinError) -> AtomicCoreError {
    AtomicCoreError::DatabaseOperation(format!("spawn_blocking join: {e}"))
}

/// Convert a (possibly borrowed) argument into an owned form that can be
/// moved into a `spawn_blocking` closure. Paired with [`ReborrowArg`] which
/// converts the owned value back to the form the sync method expects.
pub(crate) trait SpawnArg {
    type Owned: Send + 'static;
    fn into_spawn_arg(self) -> Self::Owned;
}

/// Given an owned value produced by [`SpawnArg::into_spawn_arg`], produce the
/// borrowed (or copied) form the underlying sync method signature expects.
pub(crate) trait ReborrowArg<'a> {
    type Out;
    fn reborrow_arg(&'a self) -> Self::Out;
}

// ---- SpawnArg impls ----

impl SpawnArg for &str {
    type Owned = String;
    fn into_spawn_arg(self) -> String {
        self.to_string()
    }
}

impl<T: Clone + Send + Sync + 'static> SpawnArg for &[T] {
    type Owned = Vec<T>;
    fn into_spawn_arg(self) -> Vec<T> {
        self.to_vec()
    }
}

// Blanket for `&Struct` where Struct is sized (structs, not str/[T]).
impl<T: Clone + Send + Sync + 'static> SpawnArg for &T {
    type Owned = T;
    fn into_spawn_arg(self) -> T {
        self.clone()
    }
}

impl SpawnArg for Option<&str> {
    type Owned = Option<String>;
    fn into_spawn_arg(self) -> Option<String> {
        self.map(|s| s.to_string())
    }
}

impl<T: Clone + Send + Sync + 'static> SpawnArg for Option<&[T]> {
    type Owned = Option<Vec<T>>;
    fn into_spawn_arg(self) -> Option<Vec<T>> {
        self.map(|s| s.to_vec())
    }
}

// Copy scalars pass through.
macro_rules! impl_spawn_arg_copy {
    ($($ty:ty),* $(,)?) => {
        $(
            impl SpawnArg for $ty {
                type Owned = $ty;
                fn into_spawn_arg(self) -> $ty { self }
            }
        )*
    };
}
impl_spawn_arg_copy!(i32, usize, f32, bool, Option<i32>, Option<bool>);

// ---- ReborrowArg impls ----

impl<'a> ReborrowArg<'a> for String {
    type Out = &'a str;
    fn reborrow_arg(&'a self) -> &'a str {
        self.as_str()
    }
}

impl<'a, T: 'a> ReborrowArg<'a> for Vec<T> {
    type Out = &'a [T];
    fn reborrow_arg(&'a self) -> &'a [T] {
        self.as_slice()
    }
}

impl<'a> ReborrowArg<'a> for Option<String> {
    type Out = Option<&'a str>;
    fn reborrow_arg(&'a self) -> Option<&'a str> {
        self.as_deref()
    }
}

impl<'a, T: 'a> ReborrowArg<'a> for Option<Vec<T>> {
    type Out = Option<&'a [T]>;
    fn reborrow_arg(&'a self) -> Option<&'a [T]> {
        self.as_deref()
    }
}

macro_rules! impl_reborrow_copy {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<'a> ReborrowArg<'a> for $ty {
                type Out = $ty;
                fn reborrow_arg(&'a self) -> $ty { *self }
            }
        )*
    };
}
impl_reborrow_copy!(i32, usize, f32, bool, Option<i32>, Option<bool>);

// Struct types: sync method takes `&Struct`, owned is `Struct`, reborrow is `&Struct`.
macro_rules! impl_reborrow_struct {
    ($($ty:path),* $(,)?) => {
        $(
            impl<'a> ReborrowArg<'a> for $ty {
                type Out = &'a $ty;
                fn reborrow_arg(&'a self) -> &'a $ty { self }
            }
        )*
    };
}
impl_reborrow_struct!(
    CreateAtomRequest,
    UpdateAtomRequest,
    ListAtomsParams,
    WikiArticle,
    WikiProposal,
    crate::briefing::Briefing,
);

/// Macro to generate async dispatch methods. For each method:
/// - SQLite: owns args via `SpawnArg`, runs the sync call on tokio's blocking pool,
///   reborrows args inside via `ReborrowArg`.
/// - Postgres: calls the async trait method directly (native async).
macro_rules! dispatch {
    (
        $(
            fn $name:ident(&self $(, $arg:ident: $argty:ty)*) -> $ret:ty
                => sqlite: $sqlite_method:ident, pg_trait: $trait_name:path, pg_method: $pg_method:ident;
        )*
    ) => {
        impl StorageBackend {
            $(
                pub(crate) async fn $name(&self $(, $arg: $argty)*) -> $ret {
                    match self {
                        StorageBackend::Sqlite(s) => {
                            let s = s.clone();
                            $(let $arg = SpawnArg::into_spawn_arg($arg);)*
                            tokio::task::spawn_blocking(move || {
                                s.$sqlite_method($(ReborrowArg::reborrow_arg(&$arg)),*)
                            })
                            .await
                            .map_err(join_err)?
                        }
                        #[cfg(feature = "postgres")]
                        StorageBackend::Postgres(s) => {
                            <PostgresStorage as $trait_name>::$pg_method(s $(, $arg)*).await
                        }
                    }
                }
            )*
        }
    };
}

dispatch! {
    // ---- AtomStore ----
    fn count_atoms_impl(&self) -> Result<i32, AtomicCoreError>
        => sqlite: count_atoms_impl, pg_trait: AtomStore, pg_method: count_atoms;
    fn get_all_atoms_impl(&self) -> Result<Vec<AtomWithTags>, AtomicCoreError>
        => sqlite: get_all_atoms_impl, pg_trait: AtomStore, pg_method: get_all_atoms;
    fn get_atom_impl(&self, id: &str) -> Result<Option<AtomWithTags>, AtomicCoreError>
        => sqlite: get_atom_impl, pg_trait: AtomStore, pg_method: get_atom;
    fn insert_atom_impl(&self, id: &str, request: &CreateAtomRequest, created_at: &str) -> Result<AtomWithTags, AtomicCoreError>
        => sqlite: insert_atom_impl, pg_trait: AtomStore, pg_method: insert_atom;
    fn insert_atoms_bulk_impl(&self, atoms: &[(String, CreateAtomRequest, String)]) -> Result<Vec<AtomWithTags>, AtomicCoreError>
        => sqlite: insert_atoms_bulk_impl, pg_trait: AtomStore, pg_method: insert_atoms_bulk;
    fn update_atom_impl(&self, id: &str, request: &UpdateAtomRequest, updated_at: &str) -> Result<AtomWithTags, AtomicCoreError>
        => sqlite: update_atom_impl, pg_trait: AtomStore, pg_method: update_atom;
    fn update_atom_content_only_impl(&self, id: &str, request: &UpdateAtomRequest, updated_at: &str) -> Result<AtomWithTags, AtomicCoreError>
        => sqlite: update_atom_content_only_impl, pg_trait: AtomStore, pg_method: update_atom_content_only;
    fn delete_atom_impl(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_atom_impl, pg_trait: AtomStore, pg_method: delete_atom;
    fn get_atoms_by_tag_impl(&self, tag_id: &str) -> Result<Vec<AtomWithTags>, AtomicCoreError>
        => sqlite: get_atoms_by_tag_impl, pg_trait: AtomStore, pg_method: get_atoms_by_tag;
    fn get_atom_links_impl(&self, atom_id: &str) -> Result<Vec<AtomLink>, AtomicCoreError>
        => sqlite: get_atom_links_impl, pg_trait: AtomStore, pg_method: get_atom_links;
    fn suggest_atom_links_impl(&self, query: &str, limit: i32) -> Result<Vec<AtomLinkSuggestion>, AtomicCoreError>
        => sqlite: suggest_atom_links_impl, pg_trait: AtomStore, pg_method: suggest_atom_links;
    fn list_atoms_impl(&self, params: &ListAtomsParams) -> Result<PaginatedAtoms, AtomicCoreError>
        => sqlite: list_atoms_impl, pg_trait: AtomStore, pg_method: list_atoms;
    fn get_source_list_impl(&self) -> Result<Vec<SourceInfo>, AtomicCoreError>
        => sqlite: get_source_list_impl, pg_trait: AtomStore, pg_method: get_source_list;
    fn get_embedding_status_impl(&self, atom_id: &str) -> Result<String, AtomicCoreError>
        => sqlite: get_embedding_status_impl, pg_trait: AtomStore, pg_method: get_embedding_status;
    fn get_tagging_status_impl(&self, atom_id: &str) -> Result<String, AtomicCoreError>
        => sqlite: get_tagging_status_impl, pg_trait: AtomStore, pg_method: get_tagging_status;
    fn get_atom_positions_impl(&self) -> Result<Vec<AtomPosition>, AtomicCoreError>
        => sqlite: get_atom_positions_impl, pg_trait: AtomStore, pg_method: get_atom_positions;
    fn save_atom_positions_impl(&self, positions: &[AtomPosition]) -> Result<(), AtomicCoreError>
        => sqlite: save_atom_positions_impl, pg_trait: AtomStore, pg_method: save_atom_positions;
    fn get_atoms_with_embeddings_impl(&self) -> Result<Vec<AtomWithEmbedding>, AtomicCoreError>
        => sqlite: get_atoms_with_embeddings_impl, pg_trait: AtomStore, pg_method: get_atoms_with_embeddings;
    fn get_atom_tag_ids_impl(&self, atom_id: &str) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: get_atom_tag_ids_impl, pg_trait: AtomStore, pg_method: get_atom_tag_ids;
    fn get_tag_ids_for_atoms_batch_impl(&self, atom_ids: &[String]) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: get_tag_ids_for_atoms_batch_impl, pg_trait: AtomStore, pg_method: get_tag_ids_for_atoms_batch;
    fn get_atom_content_impl(&self, atom_id: &str) -> Result<Option<String>, AtomicCoreError>
        => sqlite: get_atom_content_impl, pg_trait: AtomStore, pg_method: get_atom_content;
    fn get_atom_contents_batch_impl(&self, atom_ids: &[String]) -> Result<Vec<(String, String)>, AtomicCoreError>
        => sqlite: get_atom_contents_batch_impl, pg_trait: AtomStore, pg_method: get_atom_contents_batch;
    fn check_existing_source_urls_sync(&self, urls: &[String]) -> Result<HashSet<String>, AtomicCoreError>
        => sqlite: check_existing_source_urls_sync, pg_trait: AtomStore, pg_method: check_existing_source_urls;
    fn source_url_exists_sync(&self, url: &str) -> Result<bool, AtomicCoreError>
        => sqlite: source_url_exists_sync, pg_trait: AtomStore, pg_method: source_url_exists;
    fn get_atom_by_source_url_sync(&self, url: &str) -> Result<Option<AtomWithTags>, AtomicCoreError>
        => sqlite: get_atom_by_source_url_sync, pg_trait: AtomStore, pg_method: get_atom_by_source_url;
    fn count_pending_embeddings_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: count_pending_embeddings_sync, pg_trait: AtomStore, pg_method: count_pending_embeddings;
    fn get_all_embedding_pairs_sync(&self) -> Result<Vec<(String, Vec<f32>)>, AtomicCoreError>
        => sqlite: get_all_embedding_pairs_sync, pg_trait: AtomStore, pg_method: get_all_embedding_pairs;
    fn get_all_atom_tag_ids_sync(&self) -> Result<std::collections::HashMap<String, Vec<String>>, AtomicCoreError>
        => sqlite: get_all_atom_tag_ids_sync, pg_trait: AtomStore, pg_method: get_all_atom_tag_ids;
    fn get_canvas_atom_metadata_light_sync(&self) -> Result<Vec<(String, String, Option<String>, i32, Option<String>)>, AtomicCoreError>
        => sqlite: get_canvas_atom_metadata_light_sync, pg_trait: AtomStore, pg_method: get_canvas_atom_metadata_light;

    // ---- TagStore ----
    fn get_all_tags_impl(&self) -> Result<Vec<TagWithCount>, AtomicCoreError>
        => sqlite: get_all_tags_impl, pg_trait: TagStore, pg_method: get_all_tags;
    fn get_all_tags_filtered_impl(&self, min_count: i32) -> Result<Vec<TagWithCount>, AtomicCoreError>
        => sqlite: get_all_tags_filtered_impl, pg_trait: TagStore, pg_method: get_all_tags_filtered;
    fn get_tag_children_impl(&self, parent_id: &str, min_count: i32, limit: i32, offset: i32) -> Result<PaginatedTagChildren, AtomicCoreError>
        => sqlite: get_tag_children_impl, pg_trait: TagStore, pg_method: get_tag_children;
    fn create_tag_impl(&self, name: &str, parent_id: Option<&str>) -> Result<Tag, AtomicCoreError>
        => sqlite: create_tag_impl, pg_trait: TagStore, pg_method: create_tag;
    fn update_tag_impl(&self, id: &str, name: &str, parent_id: Option<&str>) -> Result<Tag, AtomicCoreError>
        => sqlite: update_tag_impl, pg_trait: TagStore, pg_method: update_tag;
    fn delete_tag_impl(&self, id: &str, recursive: bool) -> Result<(), AtomicCoreError>
        => sqlite: delete_tag_impl, pg_trait: TagStore, pg_method: delete_tag;
    fn set_tag_autotag_target_impl(&self, id: &str, value: bool) -> Result<(), AtomicCoreError>
        => sqlite: set_tag_autotag_target_impl, pg_trait: TagStore, pg_method: set_tag_autotag_target;
    fn set_tag_autotag_description_impl(&self, id: &str, description: &str) -> Result<(), AtomicCoreError>
        => sqlite: set_tag_autotag_description_impl, pg_trait: TagStore, pg_method: set_tag_autotag_description;
    fn configure_autotag_targets_impl(&self, keep_default_names: &[String], add_custom_names: &[String]) -> Result<Vec<Tag>, AtomicCoreError>
        => sqlite: configure_autotag_targets_impl, pg_trait: TagStore, pg_method: configure_autotag_targets;
    fn get_related_tags_impl(&self, tag_id: &str, limit: usize) -> Result<Vec<RelatedTag>, AtomicCoreError>
        => sqlite: get_related_tags_impl, pg_trait: TagStore, pg_method: get_related_tags;
    fn get_tags_for_compaction_impl(&self) -> Result<String, AtomicCoreError>
        => sqlite: get_tags_for_compaction_impl, pg_trait: TagStore, pg_method: get_tags_for_compaction;
    fn apply_tag_merges_impl(&self, merges: &[TagMerge]) -> Result<CompactionResult, AtomicCoreError>
        => sqlite: apply_tag_merges_impl, pg_trait: TagStore, pg_method: apply_tag_merges;
    fn get_or_create_tag_impl(&self, name: &str, parent_name: Option<&str>) -> Result<String, AtomicCoreError>
        => sqlite: get_or_create_tag_impl, pg_trait: TagStore, pg_method: get_or_create_tag;
    fn link_tags_to_atom_impl(&self, atom_id: &str, tag_ids: &[String]) -> Result<(), AtomicCoreError>
        => sqlite: link_tags_to_atom_impl, pg_trait: TagStore, pg_method: link_tags_to_atom;
    fn get_tag_tree_for_llm_impl(&self) -> Result<String, AtomicCoreError>
        => sqlite: get_tag_tree_for_llm_impl, pg_trait: TagStore, pg_method: get_tag_tree_for_llm;
    fn compute_tag_centroids_batch_impl(&self, tag_ids: &[String]) -> Result<(), AtomicCoreError>
        => sqlite: compute_tag_centroids_batch_impl, pg_trait: TagStore, pg_method: compute_tag_centroids_batch;
    fn get_tag_hierarchy_impl(&self, tag_id: &str) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: get_tag_hierarchy_impl, pg_trait: TagStore, pg_method: get_tag_hierarchy;
    fn count_atoms_with_tags_impl(&self, tag_ids: &[String]) -> Result<i32, AtomicCoreError>
        => sqlite: count_atoms_with_tags_impl, pg_trait: TagStore, pg_method: count_atoms_with_tags;

    // ---- ChunkStore ----
    fn set_embedding_status_sync(&self, atom_id: &str, status: &str, error: Option<&str>) -> Result<(), AtomicCoreError>
        => sqlite: set_embedding_status_sync, pg_trait: ChunkStore, pg_method: set_embedding_status;
    fn set_embedding_status_batch_sync(&self, atom_ids: &[String], status: &str, error: Option<&str>) -> Result<(), AtomicCoreError>
        => sqlite: set_embedding_status_batch_sync, pg_trait: ChunkStore, pg_method: set_embedding_status_batch;
    fn set_tagging_status_sync(&self, atom_id: &str, status: &str, error: Option<&str>) -> Result<(), AtomicCoreError>
        => sqlite: set_tagging_status_sync, pg_trait: ChunkStore, pg_method: set_tagging_status;
    fn save_chunks_and_embeddings_sync(&self, atom_id: &str, chunks: &[(String, Vec<f32>)]) -> Result<(), AtomicCoreError>
        => sqlite: save_chunks_and_embeddings_sync, pg_trait: ChunkStore, pg_method: save_chunks_and_embeddings;
    fn get_chunks_for_atoms_sync(&self, atom_ids: &[String]) -> Result<Vec<ExistingAtomChunk>, AtomicCoreError>
        => sqlite: get_chunks_for_atoms_sync, pg_trait: ChunkStore, pg_method: get_chunks_for_atoms;
    fn update_chunk_embeddings_sync(&self, chunks: &[(String, Vec<f32>)]) -> Result<(), AtomicCoreError>
        => sqlite: update_chunk_embeddings_sync, pg_trait: ChunkStore, pg_method: update_chunk_embeddings;
    fn save_chunks_and_embeddings_batch_sync(&self, atoms: &[(String, Vec<(String, Vec<f32>)>)]) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: save_chunks_and_embeddings_batch_sync, pg_trait: ChunkStore, pg_method: save_chunks_and_embeddings_batch;
    fn reset_stuck_processing_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: reset_stuck_processing_sync, pg_trait: ChunkStore, pg_method: reset_stuck_processing;
    fn reset_failed_embeddings_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: reset_failed_embeddings_sync, pg_trait: ChunkStore, pg_method: reset_failed_embeddings;
    fn reset_failed_embedding_statuses_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: reset_failed_embedding_statuses_sync, pg_trait: ChunkStore, pg_method: reset_failed_embedding_statuses;
    fn reset_failed_tagging_statuses_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: reset_failed_tagging_statuses_sync, pg_trait: ChunkStore, pg_method: reset_failed_tagging_statuses;
    fn rebuild_semantic_edges_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: rebuild_semantic_edges_sync, pg_trait: ChunkStore, pg_method: rebuild_semantic_edges;
    fn get_semantic_edges_sync(&self, min_similarity: f32) -> Result<Vec<SemanticEdge>, AtomicCoreError>
        => sqlite: get_semantic_edges_sync, pg_trait: ChunkStore, pg_method: get_semantic_edges;
    fn get_semantic_edges_raw_sync(&self, min_similarity: f32) -> Result<Vec<(String, String, f32)>, AtomicCoreError>
        => sqlite: get_semantic_edges_raw_sync, pg_trait: ChunkStore, pg_method: get_semantic_edges_raw;
    fn get_atom_neighborhood_sync(&self, atom_id: &str, depth: i32, min_similarity: f32) -> Result<NeighborhoodGraph, AtomicCoreError>
        => sqlite: get_atom_neighborhood_sync, pg_trait: ChunkStore, pg_method: get_atom_neighborhood;
    fn get_connection_counts_sync(&self, min_similarity: f32) -> Result<HashMap<String, i32>, AtomicCoreError>
        => sqlite: get_connection_counts_sync, pg_trait: ChunkStore, pg_method: get_connection_counts;
    fn recompute_all_tag_embeddings_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: recompute_all_tag_embeddings_sync, pg_trait: ChunkStore, pg_method: recompute_all_tag_embeddings;
    fn check_vector_extension_sync(&self) -> Result<String, AtomicCoreError>
        => sqlite: check_vector_extension_sync, pg_trait: ChunkStore, pg_method: check_vector_extension;
    fn claim_pending_embeddings_sync(&self, limit: i32) -> Result<Vec<(String, String)>, AtomicCoreError>
        => sqlite: claim_pending_embeddings_sync, pg_trait: ChunkStore, pg_method: claim_pending_embeddings;
    fn claim_pending_embeddings_due_sync(&self, limit: i32, max_updated_at: &str) -> Result<Vec<(String, String)>, AtomicCoreError>
        => sqlite: claim_pending_embeddings_due_sync, pg_trait: ChunkStore, pg_method: claim_pending_embeddings_due;
    fn delete_chunks_batch_sync(&self, atom_ids: &[String]) -> Result<(), AtomicCoreError>
        => sqlite: delete_chunks_batch_sync, pg_trait: ChunkStore, pg_method: delete_chunks_batch;
    fn compute_semantic_edges_for_atom_sync(&self, atom_id: &str, threshold: f32, max_edges: i32) -> Result<i32, AtomicCoreError>
        => sqlite: compute_semantic_edges_for_atom_sync, pg_trait: ChunkStore, pg_method: compute_semantic_edges_for_atom;
    fn compute_semantic_edges_batch_sync(&self, atom_ids: &[String], threshold: f32, max_edges: i32) -> Result<i32, AtomicCoreError>
        => sqlite: compute_semantic_edges_batch_sync, pg_trait: ChunkStore, pg_method: compute_semantic_edges_batch;
    fn rebuild_fts_index_sync(&self) -> Result<(), AtomicCoreError>
        => sqlite: rebuild_fts_index_sync, pg_trait: ChunkStore, pg_method: rebuild_fts_index;
    fn claim_pending_tagging_sync(&self) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: claim_pending_tagging_sync, pg_trait: ChunkStore, pg_method: claim_pending_tagging;
    fn claim_pending_tagging_due_sync(&self, max_updated_at: &str) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: claim_pending_tagging_due_sync, pg_trait: ChunkStore, pg_method: claim_pending_tagging_due;
    fn recreate_vector_index_sync(&self, dimension: usize) -> Result<(), AtomicCoreError>
        => sqlite: recreate_vector_index_sync, pg_trait: ChunkStore, pg_method: recreate_vector_index;
    fn claim_pending_reembedding_sync(&self) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: claim_pending_reembedding_sync, pg_trait: ChunkStore, pg_method: claim_pending_reembedding;
    fn claim_all_for_reembedding_sync(&self) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: claim_all_for_reembedding_sync, pg_trait: ChunkStore, pg_method: claim_all_for_reembedding;
    fn claim_pending_edges_sync(&self, limit: i32) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: claim_pending_edges_sync, pg_trait: ChunkStore, pg_method: claim_pending_edges;
    fn set_edges_status_batch_sync(&self, atom_ids: &[String], status: &str) -> Result<(), AtomicCoreError>
        => sqlite: set_edges_status_batch_sync, pg_trait: ChunkStore, pg_method: set_edges_status_batch;
    fn count_pending_edges_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: count_pending_edges_sync, pg_trait: ChunkStore, pg_method: count_pending_edges;
    fn enqueue_pipeline_jobs_sync(&self, jobs: &[AtomPipelineJobRequest]) -> Result<i32, AtomicCoreError>
        => sqlite: enqueue_pipeline_jobs_sync, pg_trait: ChunkStore, pg_method: enqueue_pipeline_jobs;
    fn enqueue_pipeline_jobs_from_statuses_sync(&self, max_updated_at: Option<&str>) -> Result<i32, AtomicCoreError>
        => sqlite: enqueue_pipeline_jobs_from_statuses_sync, pg_trait: ChunkStore, pg_method: enqueue_pipeline_jobs_from_statuses;
    fn claim_pipeline_jobs_sync(&self, limit: i32, lease_until: &str, now: &str) -> Result<Vec<AtomPipelineJob>, AtomicCoreError>
        => sqlite: claim_pipeline_jobs_sync, pg_trait: ChunkStore, pg_method: claim_pipeline_jobs;
    fn clear_pipeline_jobs_sync(&self, jobs: &[AtomPipelineJob]) -> Result<(), AtomicCoreError>
        => sqlite: clear_pipeline_jobs_sync, pg_trait: ChunkStore, pg_method: clear_pipeline_jobs;
    fn count_pipeline_jobs_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: count_pipeline_jobs_sync, pg_trait: ChunkStore, pg_method: count_pipeline_jobs;

    // ---- SearchStore ----
    fn vector_search_sync(&self, query_embedding: &[f32], limit: i32, threshold: f32, tag_id: Option<&str>, created_after: Option<&str>) -> Result<Vec<SemanticSearchResult>, AtomicCoreError>
        => sqlite: vector_search_sync, pg_trait: SearchStore, pg_method: vector_search;
    fn keyword_search_sync(&self, query: &str, limit: i32, tag_id: Option<&str>, created_after: Option<&str>) -> Result<Vec<SemanticSearchResult>, AtomicCoreError>
        => sqlite: keyword_search_sync, pg_trait: SearchStore, pg_method: keyword_search;
    fn find_similar_sync(&self, atom_id: &str, limit: i32, threshold: f32) -> Result<Vec<SimilarAtomResult>, AtomicCoreError>
        => sqlite: find_similar_sync, pg_trait: SearchStore, pg_method: find_similar;
    fn keyword_search_chunks_sync(&self, query: &str, limit: i32, scope_tag_ids: &[String], created_after: Option<&str>) -> Result<Vec<ChunkSearchResult>, AtomicCoreError>
        => sqlite: keyword_search_chunks_sync, pg_trait: SearchStore, pg_method: keyword_search_chunks;
    fn vector_search_chunks_sync(&self, query_embedding: &[f32], limit: i32, threshold: f32, scope_tag_ids: &[String], created_after: Option<&str>) -> Result<Vec<ChunkSearchResult>, AtomicCoreError>
        => sqlite: vector_search_chunks_sync, pg_trait: SearchStore, pg_method: vector_search_chunks;

    // ---- ChatStore ----
    fn create_conversation_sync(&self, tag_ids: &[String], title: Option<&str>) -> Result<ConversationWithTags, AtomicCoreError>
        => sqlite: create_conversation_sync, pg_trait: ChatStore, pg_method: create_conversation;
    fn get_conversations_sync(&self, filter_tag_id: Option<&str>, limit: i32, offset: i32) -> Result<Vec<ConversationWithTags>, AtomicCoreError>
        => sqlite: get_conversations_sync, pg_trait: ChatStore, pg_method: get_conversations;
    fn get_conversation_sync(&self, conversation_id: &str) -> Result<Option<ConversationWithMessages>, AtomicCoreError>
        => sqlite: get_conversation_sync, pg_trait: ChatStore, pg_method: get_conversation;
    fn update_conversation_sync(&self, id: &str, title: Option<&str>, is_archived: Option<bool>) -> Result<Conversation, AtomicCoreError>
        => sqlite: update_conversation_sync, pg_trait: ChatStore, pg_method: update_conversation;
    fn delete_conversation_sync(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_conversation_sync, pg_trait: ChatStore, pg_method: delete_conversation;
    fn set_conversation_scope_sync(&self, conversation_id: &str, tag_ids: &[String]) -> Result<ConversationWithTags, AtomicCoreError>
        => sqlite: set_conversation_scope_sync, pg_trait: ChatStore, pg_method: set_conversation_scope;
    fn add_tag_to_scope_sync(&self, conversation_id: &str, tag_id: &str) -> Result<ConversationWithTags, AtomicCoreError>
        => sqlite: add_tag_to_scope_sync, pg_trait: ChatStore, pg_method: add_tag_to_scope;
    fn remove_tag_from_scope_sync(&self, conversation_id: &str, tag_id: &str) -> Result<ConversationWithTags, AtomicCoreError>
        => sqlite: remove_tag_from_scope_sync, pg_trait: ChatStore, pg_method: remove_tag_from_scope;
    fn save_message_sync(&self, conversation_id: &str, role: &str, content: &str) -> Result<ChatMessage, AtomicCoreError>
        => sqlite: save_message_sync, pg_trait: ChatStore, pg_method: save_message;
    fn save_tool_calls_sync(&self, message_id: &str, tool_calls: &[ChatToolCall]) -> Result<(), AtomicCoreError>
        => sqlite: save_tool_calls_sync, pg_trait: ChatStore, pg_method: save_tool_calls;
    fn save_citations_sync(&self, message_id: &str, citations: &[ChatCitation]) -> Result<(), AtomicCoreError>
        => sqlite: save_citations_sync, pg_trait: ChatStore, pg_method: save_citations;
    fn get_scope_tag_ids_sync(&self, conversation_id: &str) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: get_scope_tag_ids_sync, pg_trait: ChatStore, pg_method: get_scope_tag_ids;
    fn get_scope_description_sync(&self, tag_ids: &[String]) -> Result<String, AtomicCoreError>
        => sqlite: get_scope_description_sync, pg_trait: ChatStore, pg_method: get_scope_description;

    // ---- WikiStore ----
    fn get_wiki_sync(&self, tag_id: &str) -> Result<Option<WikiArticleWithCitations>, AtomicCoreError>
        => sqlite: get_wiki_sync, pg_trait: WikiStore, pg_method: get_wiki;
    fn get_wiki_status_sync(&self, tag_id: &str) -> Result<WikiArticleStatus, AtomicCoreError>
        => sqlite: get_wiki_status_sync, pg_trait: WikiStore, pg_method: get_wiki_status;
    fn save_wiki_with_links_sync(&self, article: &WikiArticle, citations: &[WikiCitation], links: &[WikiLink]) -> Result<(), AtomicCoreError>
        => sqlite: save_wiki_with_links_sync, pg_trait: WikiStore, pg_method: save_wiki_with_links;
    fn delete_wiki_sync(&self, tag_id: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_wiki_sync, pg_trait: WikiStore, pg_method: delete_wiki;
    fn get_wiki_links_sync(&self, tag_id: &str) -> Result<Vec<WikiLink>, AtomicCoreError>
        => sqlite: get_wiki_links_sync, pg_trait: WikiStore, pg_method: get_wiki_links;
    fn list_wiki_versions_sync(&self, tag_id: &str) -> Result<Vec<WikiVersionSummary>, AtomicCoreError>
        => sqlite: list_wiki_versions_sync, pg_trait: WikiStore, pg_method: list_wiki_versions;
    fn get_wiki_version_sync(&self, version_id: &str) -> Result<Option<WikiArticleVersion>, AtomicCoreError>
        => sqlite: get_wiki_version_sync, pg_trait: WikiStore, pg_method: get_wiki_version;
    fn get_all_wiki_articles_sync(&self) -> Result<Vec<WikiArticleSummary>, AtomicCoreError>
        => sqlite: get_all_wiki_articles_sync, pg_trait: WikiStore, pg_method: get_all_wiki_articles;
    fn get_suggested_wiki_articles_sync(&self, limit: i32) -> Result<Vec<SuggestedArticle>, AtomicCoreError>
        => sqlite: get_suggested_wiki_articles_sync, pg_trait: WikiStore, pg_method: get_suggested_wiki_articles;
    fn get_wiki_source_chunks_sync(&self, tag_id: &str, max_source_tokens: usize) -> Result<(Vec<ChunkWithContext>, i32), AtomicCoreError>
        => sqlite: get_wiki_source_chunks_sync, pg_trait: WikiStore, pg_method: get_wiki_source_chunks;
    fn get_wiki_update_chunks_sync(&self, tag_id: &str, last_update: &str, max_source_tokens: usize) -> Result<Option<(Vec<ChunkWithContext>, i32)>, AtomicCoreError>
        => sqlite: get_wiki_update_chunks_sync, pg_trait: WikiStore, pg_method: get_wiki_update_chunks;
    fn save_wiki_proposal_sync(&self, proposal: &WikiProposal) -> Result<(), AtomicCoreError>
        => sqlite: save_wiki_proposal_sync, pg_trait: WikiStore, pg_method: save_wiki_proposal;
    fn get_wiki_proposal_sync(&self, tag_id: &str) -> Result<Option<WikiProposal>, AtomicCoreError>
        => sqlite: get_wiki_proposal_sync, pg_trait: WikiStore, pg_method: get_wiki_proposal;
    fn delete_wiki_proposal_sync(&self, tag_id: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_wiki_proposal_sync, pg_trait: WikiStore, pg_method: delete_wiki_proposal;

    // ---- BriefingStore ----
    fn list_new_atoms_since_sync(&self, since: &str, limit: i32) -> Result<Vec<AtomWithTags>, AtomicCoreError>
        => sqlite: list_new_atoms_since_sync, pg_trait: BriefingStore, pg_method: list_new_atoms_since;
    fn count_new_atoms_since_sync(&self, since: &str) -> Result<i32, AtomicCoreError>
        => sqlite: count_new_atoms_since_sync, pg_trait: BriefingStore, pg_method: count_new_atoms_since;
    fn insert_briefing_sync(&self, briefing: &crate::briefing::Briefing, citations: &[crate::briefing::BriefingCitation]) -> Result<crate::briefing::BriefingWithCitations, AtomicCoreError>
        => sqlite: insert_briefing_sync, pg_trait: BriefingStore, pg_method: insert_briefing;
    fn get_latest_briefing_sync(&self) -> Result<Option<crate::briefing::BriefingWithCitations>, AtomicCoreError>
        => sqlite: get_latest_briefing_sync, pg_trait: BriefingStore, pg_method: get_latest_briefing;
    fn get_briefing_sync(&self, id: &str) -> Result<Option<crate::briefing::BriefingWithCitations>, AtomicCoreError>
        => sqlite: get_briefing_sync, pg_trait: BriefingStore, pg_method: get_briefing;
    fn list_briefings_sync(&self, limit: i32) -> Result<Vec<crate::briefing::Briefing>, AtomicCoreError>
        => sqlite: list_briefings_sync, pg_trait: BriefingStore, pg_method: list_briefings;

    // ---- FeedStore ----
    fn list_feeds_sync(&self) -> Result<Vec<Feed>, AtomicCoreError>
        => sqlite: list_feeds_sync, pg_trait: FeedStore, pg_method: list_feeds;
    fn get_feed_sync(&self, id: &str) -> Result<Feed, AtomicCoreError>
        => sqlite: get_feed_sync, pg_trait: FeedStore, pg_method: get_feed;
    fn create_feed_sync(&self, url: &str, title: Option<&str>, site_url: Option<&str>, poll_interval: i32, tag_ids: &[String]) -> Result<Feed, AtomicCoreError>
        => sqlite: create_feed_sync, pg_trait: FeedStore, pg_method: create_feed;
    fn update_feed_sync(&self, id: &str, title: Option<&str>, poll_interval: Option<i32>, is_paused: Option<bool>, tag_ids: Option<&[String]>) -> Result<Feed, AtomicCoreError>
        => sqlite: update_feed_sync, pg_trait: FeedStore, pg_method: update_feed;
    fn delete_feed_sync(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_feed_sync, pg_trait: FeedStore, pg_method: delete_feed;
    fn get_due_feeds_sync(&self) -> Result<Vec<Feed>, AtomicCoreError>
        => sqlite: get_due_feeds_sync, pg_trait: FeedStore, pg_method: get_due_feeds;
    fn mark_feed_polled_sync(&self, id: &str, error: Option<&str>) -> Result<(), AtomicCoreError>
        => sqlite: mark_feed_polled_sync, pg_trait: FeedStore, pg_method: mark_feed_polled;
    fn claim_feed_item_sync(&self, feed_id: &str, guid: &str) -> Result<bool, AtomicCoreError>
        => sqlite: claim_feed_item_sync, pg_trait: FeedStore, pg_method: claim_feed_item;
    fn complete_feed_item_sync(&self, feed_id: &str, guid: &str, atom_id: &str) -> Result<(), AtomicCoreError>
        => sqlite: complete_feed_item_sync, pg_trait: FeedStore, pg_method: complete_feed_item;
    fn mark_feed_item_skipped_sync(&self, feed_id: &str, guid: &str, reason: &str) -> Result<(), AtomicCoreError>
        => sqlite: mark_feed_item_skipped_sync, pg_trait: FeedStore, pg_method: mark_feed_item_skipped;
    fn backfill_feed_metadata_sync(&self, id: &str, title: Option<&str>, site_url: Option<&str>) -> Result<(), AtomicCoreError>
        => sqlite: backfill_feed_metadata_sync, pg_trait: FeedStore, pg_method: backfill_feed_metadata;

    // ---- ClusterStore ----
    fn compute_clusters_sync(&self, min_similarity: f32, min_cluster_size: i32) -> Result<Vec<AtomCluster>, AtomicCoreError>
        => sqlite: compute_clusters_sync, pg_trait: ClusterStore, pg_method: compute_clusters;
    fn save_clusters_sync(&self, clusters: &[AtomCluster]) -> Result<(), AtomicCoreError>
        => sqlite: save_clusters_sync, pg_trait: ClusterStore, pg_method: save_clusters;
    fn get_clusters_sync(&self) -> Result<Vec<AtomCluster>, AtomicCoreError>
        => sqlite: get_clusters_sync, pg_trait: ClusterStore, pg_method: get_clusters;

    // ---- DatabaseStore ----
    fn list_databases_sync(&self) -> Result<Vec<crate::registry::DatabaseInfo>, AtomicCoreError>
        => sqlite: list_databases_sync, pg_trait: DatabaseStore, pg_method: list_databases;
    fn create_database_sync(&self, name: &str) -> Result<crate::registry::DatabaseInfo, AtomicCoreError>
        => sqlite: create_database_sync, pg_trait: DatabaseStore, pg_method: create_database;
    fn rename_database_sync(&self, id: &str, name: &str) -> Result<(), AtomicCoreError>
        => sqlite: rename_database_sync, pg_trait: DatabaseStore, pg_method: rename_database;
    fn delete_database_sync(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_database_sync, pg_trait: DatabaseStore, pg_method: delete_database;
    fn get_default_database_id_sync(&self) -> Result<String, AtomicCoreError>
        => sqlite: get_default_database_id_sync, pg_trait: DatabaseStore, pg_method: get_default_database_id;
    fn set_default_database_sync(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: set_default_database_sync, pg_trait: DatabaseStore, pg_method: set_default_database;
    fn purge_database_data_sync(&self, db_id: &str) -> Result<(), AtomicCoreError>
        => sqlite: purge_database_data_sync, pg_trait: DatabaseStore, pg_method: purge_database_data;

    // ---- SettingsStore ----
    fn get_all_settings_sync(&self) -> Result<HashMap<String, String>, AtomicCoreError>
        => sqlite: get_all_settings_sync, pg_trait: SettingsStore, pg_method: get_all_settings;
    fn get_setting_sync(&self, key: &str) -> Result<Option<String>, AtomicCoreError>
        => sqlite: get_setting_sync, pg_trait: SettingsStore, pg_method: get_setting;
    fn set_setting_sync(&self, key: &str, value: &str) -> Result<(), AtomicCoreError>
        => sqlite: set_setting_sync, pg_trait: SettingsStore, pg_method: set_setting;
    fn delete_setting_sync(&self, key: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_setting_sync, pg_trait: SettingsStore, pg_method: delete_setting;

    // ---- TokenStore ----
    fn create_api_token_sync(&self, name: &str) -> Result<(crate::tokens::ApiTokenInfo, String), AtomicCoreError>
        => sqlite: create_api_token_sync, pg_trait: TokenStore, pg_method: create_api_token;
    fn list_api_tokens_sync(&self) -> Result<Vec<crate::tokens::ApiTokenInfo>, AtomicCoreError>
        => sqlite: list_api_tokens_sync, pg_trait: TokenStore, pg_method: list_api_tokens;
    fn verify_api_token_sync(&self, raw_token: &str) -> Result<Option<crate::tokens::ApiTokenInfo>, AtomicCoreError>
        => sqlite: verify_api_token_sync, pg_trait: TokenStore, pg_method: verify_api_token;
    fn revoke_api_token_sync(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: revoke_api_token_sync, pg_trait: TokenStore, pg_method: revoke_api_token;
    fn update_token_last_used_sync(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: update_token_last_used_sync, pg_trait: TokenStore, pg_method: update_token_last_used;
    fn migrate_legacy_token_sync(&self) -> Result<bool, AtomicCoreError>
        => sqlite: migrate_legacy_token_sync, pg_trait: TokenStore, pg_method: migrate_legacy_token;
    fn ensure_default_token_sync(&self) -> Result<Option<(crate::tokens::ApiTokenInfo, String)>, AtomicCoreError>
        => sqlite: ensure_default_token_sync, pg_trait: TokenStore, pg_method: ensure_default_token;
}
