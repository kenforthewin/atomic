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
pub use postgres::{PgPoolConfig, PostgresStorage};

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

// Nested-optional encoding for partial updates that distinguish "leave
// unchanged" (outer None) from "clear" (Some(None)) from "set" (Some(Some)).
impl SpawnArg for Option<Option<&str>> {
    type Owned = Option<Option<String>>;
    fn into_spawn_arg(self) -> Option<Option<String>> {
        self.map(|inner| inner.map(|s| s.to_string()))
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

impl<'a> ReborrowArg<'a> for Option<Option<String>> {
    type Out = Option<Option<&'a str>>;
    fn reborrow_arg(&'a self) -> Option<Option<&'a str>> {
        self.as_ref().map(|inner| inner.as_deref())
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
    crate::models::KindFilter,
    crate::models::TaskRun,
    crate::models::CreateReportRequest,
    crate::models::UpdateReportRequest,
    crate::models::ReportFinding,
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
                // Some dispatch entries are dormant in non-test builds —
                // they exist to round out the storage surface for upcoming
                // phases (e.g. task_runs history before the reports UI
                // ships). Suppress the noise; missing-impl regressions
                // would still fail to compile.
                #[allow(dead_code)]
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
    fn update_atom_if_unchanged_impl(&self, id: &str, request: &UpdateAtomRequest, updated_at: &str, expected_updated_at: &str) -> Result<AtomWithTags, AtomicCoreError>
        => sqlite: update_atom_if_unchanged_impl, pg_trait: AtomStore, pg_method: update_atom_if_unchanged;
    fn update_atom_content_only_impl(&self, id: &str, request: &UpdateAtomRequest, updated_at: &str) -> Result<AtomWithTags, AtomicCoreError>
        => sqlite: update_atom_content_only_impl, pg_trait: AtomStore, pg_method: update_atom_content_only;
    fn delete_atom_impl(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_atom_impl, pg_trait: AtomStore, pg_method: delete_atom;
    fn get_atoms_by_tag_impl(&self, tag_id: &str, kinds: &crate::models::KindFilter) -> Result<Vec<AtomWithTags>, AtomicCoreError>
        => sqlite: get_atoms_by_tag_impl, pg_trait: AtomStore, pg_method: get_atoms_by_tag;
    fn get_atom_links_impl(&self, atom_id: &str) -> Result<Vec<AtomLink>, AtomicCoreError>
        => sqlite: get_atom_links_impl, pg_trait: AtomStore, pg_method: get_atom_links;
    fn suggest_atom_links_impl(&self, query: &str, limit: i32) -> Result<Vec<AtomLinkSuggestion>, AtomicCoreError>
        => sqlite: suggest_atom_links_impl, pg_trait: AtomStore, pg_method: suggest_atom_links;
    fn list_atoms_impl(&self, params: &ListAtomsParams, kinds: &crate::models::KindFilter) -> Result<PaginatedAtoms, AtomicCoreError>
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
    fn get_atoms_with_embeddings_impl(&self, kinds: &crate::models::KindFilter) -> Result<Vec<AtomWithEmbedding>, AtomicCoreError>
        => sqlite: get_atoms_with_embeddings_impl, pg_trait: AtomStore, pg_method: get_atoms_with_embeddings;
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
    fn get_all_embedding_pairs_sync(&self) -> Result<Vec<(String, Vec<f32>)>, AtomicCoreError>
        => sqlite: get_all_embedding_pairs_sync, pg_trait: AtomStore, pg_method: get_all_embedding_pairs;
    fn get_all_atom_tag_ids_sync(&self) -> Result<std::collections::HashMap<String, Vec<String>>, AtomicCoreError>
        => sqlite: get_all_atom_tag_ids_sync, pg_trait: AtomStore, pg_method: get_all_atom_tag_ids;
    fn get_canvas_atom_metadata_light_sync(&self, kinds: &crate::models::KindFilter) -> Result<Vec<(String, String, Option<String>, i32, Option<String>)>, AtomicCoreError>
        => sqlite: get_canvas_atom_metadata_light_sync, pg_trait: AtomStore, pg_method: get_canvas_atom_metadata_light;
    fn list_atoms_for_report_scope_sync(&self, tag_ids: &[String], since: Option<&str>, kinds: &crate::models::KindFilter, limit: Option<i32>) -> Result<Vec<AtomWithTags>, AtomicCoreError>
        => sqlite: list_atoms_for_report_scope_sync, pg_trait: AtomStore, pg_method: list_atoms_for_report_scope;
    fn count_atoms_for_report_scope_sync(&self, tag_ids: &[String], since: Option<&str>, kinds: &crate::models::KindFilter) -> Result<i32, AtomicCoreError>
        => sqlite: count_atoms_for_report_scope_sync, pg_trait: AtomStore, pg_method: count_atoms_for_report_scope;

    // ---- TagStore ----
    fn get_all_tags_impl(&self) -> Result<Vec<TagWithCount>, AtomicCoreError>
        => sqlite: get_all_tags_impl, pg_trait: TagStore, pg_method: get_all_tags;
    fn get_all_tags_filtered_impl(&self, min_count: i32) -> Result<Vec<TagWithCount>, AtomicCoreError>
        => sqlite: get_all_tags_filtered_impl, pg_trait: TagStore, pg_method: get_all_tags_filtered;
    fn get_tag_children_impl(&self, parent_id: &str, min_count: i32, limit: i32, offset: i32) -> Result<PaginatedTagChildren, AtomicCoreError>
        => sqlite: get_tag_children_impl, pg_trait: TagStore, pg_method: get_tag_children;
    fn get_tag_sync(&self, id: &str) -> Result<Option<Tag>, AtomicCoreError>
        => sqlite: get_tag_impl, pg_trait: TagStore, pg_method: get_tag;
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
    fn get_or_create_tag_with_parent_id(&self, name: &str, parent_id: Option<&str>) -> Result<(String, bool), AtomicCoreError>
        => sqlite: get_or_create_tag_with_parent_id_impl, pg_trait: TagStore, pg_method: get_or_create_tag_with_parent_id;
    fn link_tags_to_atom_with_source(&self, atom_id: &str, tag_ids: &[String], source: &str) -> Result<(), AtomicCoreError>
        => sqlite: link_tags_to_atom_with_source_impl, pg_trait: TagStore, pg_method: link_tags_to_atom_with_source;
    fn get_tag_tree_for_llm_impl(&self) -> Result<String, AtomicCoreError>
        => sqlite: get_tag_tree_for_llm_impl, pg_trait: TagStore, pg_method: get_tag_tree_for_llm;
    fn compute_tag_centroids_batch_impl(&self, tag_ids: &[String]) -> Result<(), AtomicCoreError>
        => sqlite: compute_tag_centroids_batch_impl, pg_trait: TagStore, pg_method: compute_tag_centroids_batch;
    fn get_tag_hierarchy_impl(&self, tag_id: &str) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: get_tag_hierarchy_impl, pg_trait: TagStore, pg_method: get_tag_hierarchy;
    fn count_atoms_with_tags_impl(&self, tag_ids: &[String], kinds: &crate::models::KindFilter) -> Result<i32, AtomicCoreError>
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
    fn delete_chunks_batch_sync(&self, atom_ids: &[String]) -> Result<(), AtomicCoreError>
        => sqlite: delete_chunks_batch_sync, pg_trait: ChunkStore, pg_method: delete_chunks_batch;
    fn compute_semantic_edges_batch_sync(&self, atom_ids: &[String], threshold: f32, max_edges: i32) -> Result<i32, AtomicCoreError>
        => sqlite: compute_semantic_edges_batch_sync, pg_trait: ChunkStore, pg_method: compute_semantic_edges_batch;
    fn rebuild_fts_index_sync(&self) -> Result<(), AtomicCoreError>
        => sqlite: rebuild_fts_index_sync, pg_trait: ChunkStore, pg_method: rebuild_fts_index;
    fn recreate_vector_index_sync(&self, dimension: usize) -> Result<(), AtomicCoreError>
        => sqlite: recreate_vector_index_sync, pg_trait: ChunkStore, pg_method: recreate_vector_index;
    fn claim_pending_reembedding_sync(&self) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: claim_pending_reembedding_sync, pg_trait: ChunkStore, pg_method: claim_pending_reembedding;
    fn claim_all_for_reembedding_sync(&self) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: claim_all_for_reembedding_sync, pg_trait: ChunkStore, pg_method: claim_all_for_reembedding;
    fn claim_all_for_retagging_sync(&self) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: claim_all_for_retagging_sync, pg_trait: ChunkStore, pg_method: claim_all_for_retagging;
    fn delete_auto_tags_without_wiki_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: delete_auto_tags_without_wiki_sync, pg_trait: ChunkStore, pg_method: delete_auto_tags_without_wiki;
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
    fn count_due_pipeline_jobs_sync(&self, now: &str) -> Result<i32, AtomicCoreError>
        => sqlite: count_due_pipeline_jobs_sync, pg_trait: ChunkStore, pg_method: count_due_pipeline_jobs;
    fn rearm_pipeline_jobs_sync(&self, reason: &str, now: &str) -> Result<u64, AtomicCoreError>
        => sqlite: rearm_pipeline_jobs_sync, pg_trait: ChunkStore, pg_method: rearm_pipeline_jobs;

    // ---- SearchStore ----
    fn vector_search_sync(&self, query_embedding: &[f32], limit: i32, threshold: f32, tag_id: Option<&str>, created_after: Option<&str>, kinds: &crate::models::KindFilter) -> Result<Vec<SemanticSearchResult>, AtomicCoreError>
        => sqlite: vector_search_sync, pg_trait: SearchStore, pg_method: vector_search;
    fn keyword_search_sync(&self, query: &str, limit: i32, tag_id: Option<&str>, created_after: Option<&str>, kinds: &crate::models::KindFilter) -> Result<Vec<SemanticSearchResult>, AtomicCoreError>
        => sqlite: keyword_search_sync, pg_trait: SearchStore, pg_method: keyword_search;
    fn find_similar_sync(&self, atom_id: &str, limit: i32, threshold: f32) -> Result<Vec<SimilarAtomResult>, AtomicCoreError>
        => sqlite: find_similar_sync, pg_trait: SearchStore, pg_method: find_similar;
    fn keyword_search_chunks_sync(&self, query: &str, limit: i32, scope_tag_ids: &[String], created_after: Option<&str>, kinds: &crate::models::KindFilter) -> Result<Vec<ChunkSearchResult>, AtomicCoreError>
        => sqlite: keyword_search_chunks_sync, pg_trait: SearchStore, pg_method: keyword_search_chunks;
    fn vector_search_chunks_sync(&self, query_embedding: &[f32], limit: i32, threshold: f32, scope_tag_ids: &[String], created_after: Option<&str>, kinds: &crate::models::KindFilter) -> Result<Vec<ChunkSearchResult>, AtomicCoreError>
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
    fn advance_wiki_baseline_sync(&self, tag_id: &str, max_current_count: Option<i32>) -> Result<bool, AtomicCoreError>
        => sqlite: advance_wiki_baseline_sync, pg_trait: WikiStore, pg_method: advance_wiki_baseline;

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
    fn set_feed_error_sync(&self, id: &str, error: &str) -> Result<(), AtomicCoreError>
        => sqlite: set_feed_error_sync, pg_trait: FeedStore, pg_method: set_feed_error;
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

    // ---- SettingsStore ----
    // Scoped (per-DB) tier: task.{id}.* scheduler state, seed flags,
    // per-DB overrides.
    fn get_all_settings_sync(&self) -> Result<HashMap<String, String>, AtomicCoreError>
        => sqlite: get_all_settings_sync, pg_trait: SettingsStore, pg_method: get_all_settings;
    fn get_setting_sync(&self, key: &str) -> Result<Option<String>, AtomicCoreError>
        => sqlite: get_setting_sync, pg_trait: SettingsStore, pg_method: get_setting;
    fn set_setting_sync(&self, key: &str, value: &str) -> Result<(), AtomicCoreError>
        => sqlite: set_setting_sync, pg_trait: SettingsStore, pg_method: set_setting;
    fn delete_setting_sync(&self, key: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_setting_sync, pg_trait: SettingsStore, pg_method: delete_setting;
    // Global (registry-role) tier: provider/model config and other
    // deployment-wide settings. On SQLite a data DB's own table *is* its
    // only settings table (registry.db handles the global role physically),
    // so the global accessors map to the same sync helpers; Postgres routes
    // them to the '_global' sentinel db_id.
    fn get_global_settings_sync(&self) -> Result<HashMap<String, String>, AtomicCoreError>
        => sqlite: get_all_settings_sync, pg_trait: SettingsStore, pg_method: get_global_settings;
    fn get_global_setting_sync(&self, key: &str) -> Result<Option<String>, AtomicCoreError>
        => sqlite: get_setting_sync, pg_trait: SettingsStore, pg_method: get_global_setting;
    fn set_global_setting_sync(&self, key: &str, value: &str) -> Result<(), AtomicCoreError>
        => sqlite: set_setting_sync, pg_trait: SettingsStore, pg_method: set_global_setting;
    fn delete_global_setting_sync(&self, key: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_setting_sync, pg_trait: SettingsStore, pg_method: delete_global_setting;

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

    // ---- TaskRunStore ----
    fn insert_task_run_sync(&self, run: &crate::models::TaskRun) -> Result<(), AtomicCoreError>
        => sqlite: insert_task_run_sync, pg_trait: TaskRunStore, pg_method: insert_task_run;
    fn try_insert_task_run_sync(&self, run: &crate::models::TaskRun) -> Result<bool, AtomicCoreError>
        => sqlite: try_insert_task_run_sync, pg_trait: TaskRunStore, pg_method: try_insert_task_run;
    fn get_task_run_sync(&self, id: &str) -> Result<Option<crate::models::TaskRun>, AtomicCoreError>
        => sqlite: get_task_run_sync, pg_trait: TaskRunStore, pg_method: get_task_run;
    fn find_runnable_task_run_sync(&self, task_id: &str, subject_id: Option<&str>, now: &str) -> Result<Option<crate::models::TaskRun>, AtomicCoreError>
        => sqlite: find_runnable_task_run_sync, pg_trait: TaskRunStore, pg_method: find_runnable_task_run;
    fn list_runnable_task_runs_sync(&self, task_id: &str, now: &str) -> Result<Vec<crate::models::TaskRun>, AtomicCoreError>
        => sqlite: list_runnable_task_runs_sync, pg_trait: TaskRunStore, pg_method: list_runnable_task_runs;
    fn count_active_task_runs_sync(&self) -> Result<i32, AtomicCoreError>
        => sqlite: count_active_task_runs_sync, pg_trait: TaskRunStore, pg_method: count_active_task_runs;
    fn find_active_task_run_sync(&self, task_id: &str, subject_id: Option<&str>) -> Result<Option<crate::models::TaskRun>, AtomicCoreError>
        => sqlite: find_active_task_run_sync, pg_trait: TaskRunStore, pg_method: find_active_task_run;
    fn claim_pending_task_run_sync(&self, id: &str, now: &str, lease_until: &str) -> Result<bool, AtomicCoreError>
        => sqlite: claim_pending_task_run_sync, pg_trait: TaskRunStore, pg_method: claim_pending_task_run;
    fn reclaim_expired_task_run_sync(&self, id: &str, now: &str, lease_until: &str) -> Result<bool, AtomicCoreError>
        => sqlite: reclaim_expired_task_run_sync, pg_trait: TaskRunStore, pg_method: reclaim_expired_task_run;
    fn heartbeat_task_run_sync(&self, id: &str, expected_lease: &str, new_lease_until: &str) -> Result<bool, AtomicCoreError>
        => sqlite: heartbeat_task_run_sync, pg_trait: TaskRunStore, pg_method: heartbeat_task_run;
    fn complete_task_run_sync(&self, id: &str, expected_lease: &str, result_id: Option<&str>, finished_at: &str) -> Result<bool, AtomicCoreError>
        => sqlite: complete_task_run_sync, pg_trait: TaskRunStore, pg_method: complete_task_run;
    fn fail_task_run_retry_sync(&self, id: &str, expected_lease: &str, last_error: &str, now: &str, next_attempt_at: &str) -> Result<bool, AtomicCoreError>
        => sqlite: fail_task_run_retry_sync, pg_trait: TaskRunStore, pg_method: fail_task_run_retry;
    fn fail_task_run_abandon_sync(&self, id: &str, expected_lease: &str, last_error: &str, finished_at: &str) -> Result<bool, AtomicCoreError>
        => sqlite: fail_task_run_abandon_sync, pg_trait: TaskRunStore, pg_method: fail_task_run_abandon;
    fn defer_task_run_sync(&self, id: &str, expected_lease: &str, last_error: &str, now: &str, next_attempt_at: &str) -> Result<bool, AtomicCoreError>
        => sqlite: defer_task_run_sync, pg_trait: TaskRunStore, pg_method: defer_task_run;
    fn list_waiting_task_runs_sync(&self, now: &str) -> Result<Vec<crate::models::TaskRun>, AtomicCoreError>
        => sqlite: list_waiting_task_runs_sync, pg_trait: TaskRunStore, pg_method: list_waiting_task_runs;
    fn rearm_task_runs_sync(&self, ids: &[String], now: &str) -> Result<u64, AtomicCoreError>
        => sqlite: rearm_task_runs_sync, pg_trait: TaskRunStore, pg_method: rearm_task_runs;
    fn settle_task_runs_moot_sync(&self, task_id: &str, subject_id: &str, finished_at: &str) -> Result<u64, AtomicCoreError>
        => sqlite: settle_task_runs_moot_sync, pg_trait: TaskRunStore, pg_method: settle_task_runs_moot;
    fn list_recent_task_runs_sync(&self, task_id: &str, subject_id: Option<&str>, limit: i32) -> Result<Vec<crate::models::TaskRun>, AtomicCoreError>
        => sqlite: list_recent_task_runs_sync, pg_trait: TaskRunStore, pg_method: list_recent_task_runs;
    fn gc_task_runs_sync(&self, keep_per_subject: i32, age_cutoff: &str, failed_cutoff: &str, batch_size: i32) -> Result<u64, AtomicCoreError>
        => sqlite: gc_task_runs_sync, pg_trait: TaskRunStore, pg_method: gc_task_runs;

    // ---- ReportStore ----
    fn list_reports_sync(&self) -> Result<Vec<crate::models::Report>, AtomicCoreError>
        => sqlite: list_reports_sync, pg_trait: ReportStore, pg_method: list_reports;
    fn list_enabled_reports_sync(&self) -> Result<Vec<crate::models::Report>, AtomicCoreError>
        => sqlite: list_enabled_reports_sync, pg_trait: ReportStore, pg_method: list_enabled_reports;
    fn get_report_sync(&self, id: &str) -> Result<Option<crate::models::Report>, AtomicCoreError>
        => sqlite: get_report_sync, pg_trait: ReportStore, pg_method: get_report;
    fn insert_report_sync(&self, request: &crate::models::CreateReportRequest) -> Result<crate::models::Report, AtomicCoreError>
        => sqlite: insert_report_sync, pg_trait: ReportStore, pg_method: insert_report;
    fn update_report_sync(&self, id: &str, request: &crate::models::UpdateReportRequest) -> Result<crate::models::Report, AtomicCoreError>
        => sqlite: update_report_sync, pg_trait: ReportStore, pg_method: update_report;
    fn set_report_enabled_sync(&self, id: &str, enabled: bool) -> Result<(), AtomicCoreError>
        => sqlite: set_report_enabled_sync, pg_trait: ReportStore, pg_method: set_report_enabled;
    fn delete_report_sync(&self, id: &str) -> Result<(), AtomicCoreError>
        => sqlite: delete_report_sync, pg_trait: ReportStore, pg_method: delete_report;
    fn update_report_cache_sync(&self, id: &str, last_run_at: Option<&str>, last_finding_atom_id: Option<Option<&str>>, last_error: Option<Option<&str>>) -> Result<(), AtomicCoreError>
        => sqlite: update_report_cache_sync, pg_trait: ReportStore, pg_method: update_report_cache;
    fn list_findings_for_report_sync(&self, report_id: &str, limit: i32) -> Result<Vec<(crate::models::ReportFinding, crate::models::AtomWithTags)>, AtomicCoreError>
        => sqlite: list_findings_for_report_sync, pg_trait: ReportStore, pg_method: list_findings_for_report;
    fn get_finding_provenance_sync(&self, finding_atom_id: &str) -> Result<Option<crate::models::ReportFinding>, AtomicCoreError>
        => sqlite: get_finding_provenance_sync, pg_trait: ReportStore, pg_method: get_finding_provenance;
    fn list_finding_atom_ids_for_report_sync(&self, report_id: &str) -> Result<Vec<String>, AtomicCoreError>
        => sqlite: list_finding_atom_ids_for_report_sync, pg_trait: ReportStore, pg_method: list_finding_atom_ids_for_report;
    fn list_citations_for_finding_sync(&self, finding_atom_id: &str) -> Result<Vec<crate::models::ReportFindingCitation>, AtomicCoreError>
        => sqlite: list_citations_for_finding_sync, pg_trait: ReportStore, pg_method: list_citations_for_finding;
    fn write_finding_transactionally_sync(&self, atom_request: &CreateAtomRequest, atom_id: &str, atom_created_at: &str, provenance: &crate::models::ReportFinding, citations: &[crate::models::ReportFindingCitation]) -> Result<AtomWithTags, AtomicCoreError>
        => sqlite: write_finding_transactionally_sync, pg_trait: ReportStore, pg_method: write_finding_transactionally;

    // ---- LegacyBriefingsMigrationStore (phase-3 collapse) ----
    fn fetch_legacy_briefings_sync(&self) -> Result<Vec<crate::reports::seed::LegacyBriefingRow>, AtomicCoreError>
        => sqlite: fetch_legacy_briefings_sync, pg_trait: crate::storage::traits::LegacyBriefingsMigrationStore, pg_method: fetch_legacy_briefings;
    fn drop_legacy_briefing_tables_sync(&self) -> Result<(), AtomicCoreError>
        => sqlite: drop_legacy_briefing_tables_sync, pg_trait: crate::storage::traits::LegacyBriefingsMigrationStore, pg_method: drop_legacy_briefing_tables;
}

#[cfg(feature = "postgres")]
impl StorageBackend {
    pub(crate) async fn list_databases_sync(
        &self,
    ) -> Result<Vec<crate::registry::DatabaseInfo>, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => <SqliteStorage as DatabaseStore>::list_databases(s).await,
            StorageBackend::Postgres(s) => {
                <PostgresStorage as DatabaseStore>::list_databases(s).await
            }
        }
    }

    pub(crate) async fn create_database_sync(
        &self,
        name: &str,
    ) -> Result<crate::registry::DatabaseInfo, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                <SqliteStorage as DatabaseStore>::create_database(s, name).await
            }
            StorageBackend::Postgres(s) => {
                <PostgresStorage as DatabaseStore>::create_database(s, name).await
            }
        }
    }

    pub(crate) async fn rename_database_sync(
        &self,
        id: &str,
        name: &str,
    ) -> Result<(), AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                <SqliteStorage as DatabaseStore>::rename_database(s, id, name).await
            }
            StorageBackend::Postgres(s) => {
                <PostgresStorage as DatabaseStore>::rename_database(s, id, name).await
            }
        }
    }

    pub(crate) async fn delete_database_sync(&self, id: &str) -> Result<(), AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                <SqliteStorage as DatabaseStore>::delete_database(s, id).await
            }
            StorageBackend::Postgres(s) => {
                <PostgresStorage as DatabaseStore>::delete_database(s, id).await
            }
        }
    }

    pub(crate) async fn get_default_database_id_sync(&self) -> Result<String, AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                <SqliteStorage as DatabaseStore>::get_default_database_id(s).await
            }
            StorageBackend::Postgres(s) => {
                <PostgresStorage as DatabaseStore>::get_default_database_id(s).await
            }
        }
    }

    pub(crate) async fn set_default_database_sync(&self, id: &str) -> Result<(), AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                <SqliteStorage as DatabaseStore>::set_default_database(s, id).await
            }
            StorageBackend::Postgres(s) => {
                <PostgresStorage as DatabaseStore>::set_default_database(s, id).await
            }
        }
    }

    pub(crate) async fn purge_database_data_sync(
        &self,
        db_id: &str,
    ) -> Result<(), AtomicCoreError> {
        match self {
            StorageBackend::Sqlite(s) => {
                <SqliteStorage as DatabaseStore>::purge_database_data(s, db_id).await
            }
            StorageBackend::Postgres(s) => {
                <PostgresStorage as DatabaseStore>::purge_database_data(s, db_id).await
            }
        }
    }
}
