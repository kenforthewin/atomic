//! Storage trait definitions for atomic-core.
//!
//! These traits define the storage abstraction layer. All database operations
//! go through these traits, allowing different backends (SQLite, Postgres, etc.)
//! to be plugged in.
//!
//! All trait methods are async to support both sync backends (SQLite via
//! spawn_blocking) and natively async backends (Postgres via sqlx).

use async_trait::async_trait;

use crate::compaction::{CompactionResult, TagMerge};
use crate::error::AtomicCoreError;
use crate::models::AtomCluster;
use crate::models::*;
use crate::{CreateAtomRequest, ListAtomsParams, UpdateAtomRequest};

/// Result type alias for storage operations.
pub type StorageResult<T> = Result<T, AtomicCoreError>;

// ==================== Atom Storage ====================

/// Storage operations for atoms (the fundamental unit of the knowledge base).
#[async_trait]
pub trait AtomStore: Send + Sync {
    /// Get all atoms with their tags.
    async fn get_all_atoms(&self) -> StorageResult<Vec<AtomWithTags>>;

    /// Count total atoms in this database.
    async fn count_atoms(&self) -> StorageResult<i32>;

    /// Get a single atom by ID with its tags.
    async fn get_atom(&self, id: &str) -> StorageResult<Option<AtomWithTags>>;

    /// Insert a new atom into the database. Returns the created atom with tags.
    /// Does NOT trigger embedding — that's handled by AtomicCore.
    async fn insert_atom(
        &self,
        id: &str,
        request: &CreateAtomRequest,
        created_at: &str,
    ) -> StorageResult<AtomWithTags>;

    /// Insert multiple atoms in a single transaction. Returns the created atoms.
    async fn insert_atoms_bulk(
        &self,
        atoms: &[(String, CreateAtomRequest, String)], // (id, request, created_at)
    ) -> StorageResult<Vec<AtomWithTags>>;

    /// Update an existing atom. Returns the updated atom with tags.
    async fn update_atom(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
    ) -> StorageResult<AtomWithTags>;

    /// Update an atom only if it has not changed since the caller read it.
    async fn update_atom_if_unchanged(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
        expected_updated_at: &str,
    ) -> StorageResult<AtomWithTags>;

    /// Update atom content/metadata without resetting embedding status.
    /// Used by auto-save during inline editing. Defaults to regular update_atom.
    async fn update_atom_content_only(
        &self,
        id: &str,
        request: &UpdateAtomRequest,
        updated_at: &str,
    ) -> StorageResult<AtomWithTags> {
        self.update_atom(id, request, updated_at).await
    }

    /// Delete an atom and all associated data (tags, chunks, embeddings, edges).
    async fn delete_atom(&self, id: &str) -> StorageResult<()>;

    /// Get all atoms with a specific tag (including descendants of that tag).
    /// `kinds` restricts which atom kinds are included — see
    /// [`crate::models::KindFilter`].
    async fn get_atoms_by_tag(
        &self,
        tag_id: &str,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<AtomWithTags>>;

    /// Get materialized markdown wiki-links emitted by a source atom.
    async fn get_atom_links(&self, atom_id: &str) -> StorageResult<Vec<AtomLink>>;

    /// Suggest recent atoms or title matches for editor link completion.
    async fn suggest_atom_links(
        &self,
        query: &str,
        limit: i32,
    ) -> StorageResult<Vec<AtomLinkSuggestion>>;

    /// List atoms with pagination, filtering, and sorting. `kinds` restricts
    /// which atom kinds appear in the results and the total count.
    async fn list_atoms(
        &self,
        params: &ListAtomsParams,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<PaginatedAtoms>;

    /// Get all unique sources with atom counts.
    async fn get_source_list(&self) -> StorageResult<Vec<SourceInfo>>;

    /// Get embedding status for a specific atom.
    async fn get_embedding_status(&self, atom_id: &str) -> StorageResult<String>;

    /// Get tagging status for a specific atom.
    async fn get_tagging_status(&self, atom_id: &str) -> StorageResult<String>;

    /// Get all atom canvas positions.
    async fn get_atom_positions(&self) -> StorageResult<Vec<AtomPosition>>;

    /// Save atom canvas positions (replaces all).
    async fn save_atom_positions(&self, positions: &[AtomPosition]) -> StorageResult<()>;

    /// Get all atoms with their average embedding vectors. `kinds` restricts
    /// which atom kinds are included.
    async fn get_atoms_with_embeddings(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<AtomWithEmbedding>>;

    /// Get just the tag IDs for an atom (lightweight, no full atom fetch).
    async fn get_atom_tag_ids(&self, atom_id: &str) -> StorageResult<Vec<String>>;

    /// Get distinct tag IDs for a batch of atoms in a single query.
    async fn get_tag_ids_for_atoms_batch(&self, atom_ids: &[String]) -> StorageResult<Vec<String>> {
        let mut all = Vec::new();
        for id in atom_ids {
            all.extend(self.get_atom_tag_ids(id).await?);
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// Get just the content for an atom (lightweight, for embedding pipeline).
    async fn get_atom_content(&self, atom_id: &str) -> StorageResult<Option<String>>;

    /// Get content for multiple atoms in a single query.
    async fn get_atom_contents_batch(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<(String, String)>>;

    /// Check which source URLs already exist in the database.
    /// Returns the set of URLs that are already present.
    async fn check_existing_source_urls(
        &self,
        urls: &[String],
    ) -> StorageResult<std::collections::HashSet<String>>;

    /// Check if a specific source URL already exists.
    async fn source_url_exists(&self, url: &str) -> StorageResult<bool>;

    /// Get an atom by its source URL. Returns None if not found.
    async fn get_atom_by_source_url(&self, url: &str) -> StorageResult<Option<AtomWithTags>>;

    /// Count atoms with pending embedding status.
    async fn count_pending_embeddings(&self) -> StorageResult<i32>;

    /// Get all average embeddings as (atom_id, embedding) pairs for PCA projection.
    async fn get_all_embedding_pairs(&self) -> StorageResult<Vec<(String, Vec<f32>)>>;

    /// Get semantic edges for canvas visualization, keeping at least top-K per atom.
    /// An edge is kept if either endpoint has fewer than top_k edges so far,
    /// which guarantees every atom gets its strongest connections but allows
    /// hubs to exceed top_k.
    async fn get_top_k_canvas_edges(&self, top_k: usize) -> StorageResult<Vec<CanvasEdgeData>>;

    /// Get all atom-to-tag-id mappings in batch.
    async fn get_all_atom_tag_ids(
        &self,
    ) -> StorageResult<std::collections::HashMap<String, Vec<String>>>;

    /// Get atom metadata for canvas display (title, primary tag, tag count) by position.
    async fn get_canvas_atom_metadata(&self) -> StorageResult<Vec<CanvasAtomPosition>>;

    /// Lightweight canvas metadata: (atom_id, title, primary_tag_name, tag_count, source_url).
    async fn get_canvas_atom_metadata_light(
        &self,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<(String, String, Option<String>, i32, Option<String>)>>;

    /// Resolve the source-scope atom set for a report run in one query.
    ///
    /// - `tag_ids` empty: no tag filter (every atom in the per-DB store).
    /// - `tag_ids` non-empty: recursive subtree expansion so a top-level
    ///   tag implicitly includes its descendants.
    /// - `since` `Some`: filter `atoms.created_at > since`. None = no time
    ///   bound. Caller pre-resolves "since_last_run" and ISO-8601
    ///   durations into an RFC3339 timestamp.
    /// - `kinds`: standard kind filter (same `Only(vec![Captured])`
    ///   default as every other context-assembly query).
    /// - `limit`: optional cap. None = unlimited; pre-cap counts are
    ///   reported separately via [`count_atoms_for_report_scope`].
    ///
    /// Returns newest-first, joined with tag rows so the agent prompt has
    /// the same shape as the daily briefing today.
    async fn list_atoms_for_report_scope(
        &self,
        tag_ids: &[String],
        since: Option<&str>,
        kinds: &crate::models::KindFilter,
        limit: Option<i32>,
    ) -> StorageResult<Vec<AtomWithTags>>;

    /// Pre-cap count for the same scope query used by
    /// [`list_atoms_for_report_scope`]. Reported back to the agent so it
    /// knows whether the visible list is truncated.
    async fn count_atoms_for_report_scope(
        &self,
        tag_ids: &[String],
        since: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<i32>;
}

// ==================== Tag Storage ====================

/// Storage operations for tags (hierarchical organizational units).
#[async_trait]
pub trait TagStore: Send + Sync {
    /// Get all tags with atom counts, organized hierarchically.
    async fn get_all_tags(&self) -> StorageResult<Vec<TagWithCount>>;

    /// Get all tags filtered by minimum atom count.
    async fn get_all_tags_filtered(&self, min_count: i32) -> StorageResult<Vec<TagWithCount>>;

    /// Get children of a tag with pagination.
    async fn get_tag_children(
        &self,
        parent_id: &str,
        min_count: i32,
        limit: i32,
        offset: i32,
    ) -> StorageResult<PaginatedTagChildren>;

    /// Create a new tag.
    async fn create_tag(&self, name: &str, parent_id: Option<&str>) -> StorageResult<Tag>;

    /// Update a tag's name and/or parent.
    async fn update_tag(&self, id: &str, name: &str, parent_id: Option<&str>)
        -> StorageResult<Tag>;

    /// Delete a tag. If recursive, also deletes child tags.
    async fn delete_tag(&self, id: &str, recursive: bool) -> StorageResult<()>;

    /// Mark or unmark a tag as a candidate for AI auto-tagging to extend with sub-tags.
    async fn set_tag_autotag_target(&self, id: &str, value: bool) -> StorageResult<()>;

    /// Set optional guidance used when this tag is an auto-tag target.
    async fn set_tag_autotag_description(&self, id: &str, description: &str) -> StorageResult<()>;

    /// Apply a full auto-tag-target configuration in a single transaction.
    /// See `AtomicCore::configure_autotag_targets` for semantics.
    async fn configure_autotag_targets(
        &self,
        keep_default_names: &[String],
        add_custom_names: &[String],
    ) -> StorageResult<Vec<Tag>>;

    /// Get tags semantically related to a given tag (via centroid similarity).
    async fn get_related_tags(&self, tag_id: &str, limit: usize) -> StorageResult<Vec<RelatedTag>>;

    /// Read all tags formatted for compaction LLM input.
    async fn get_tags_for_compaction(&self) -> StorageResult<String>;

    /// Apply tag merge operations (merge source tags into targets).
    async fn apply_tag_merges(&self, merges: &[TagMerge]) -> StorageResult<CompactionResult>;

    /// Get or create a tag by name, optionally under a parent name.
    /// Returns the tag ID.
    async fn get_or_create_tag(
        &self,
        name: &str,
        parent_name: Option<&str>,
    ) -> StorageResult<String>;

    /// Link tags to an atom (ignores duplicates).
    async fn link_tags_to_atom(&self, atom_id: &str, tag_ids: &[String]) -> StorageResult<()>;

    /// Get the tag tree formatted as JSON for LLM tag extraction.
    async fn get_tag_tree_for_llm(&self) -> StorageResult<String>;

    /// Compute tag centroid embeddings for a batch of tags from their atoms' embeddings.
    async fn compute_tag_centroids_batch(&self, tag_ids: &[String]) -> StorageResult<()>;

    /// Clean up orphaned parent tags (parents with no children and no atoms).
    async fn cleanup_orphaned_parents(&self, tag_id: &str) -> StorageResult<()>;

    /// Get all tag IDs in a hierarchy (the tag itself + all descendants).
    /// Uses a recursive traversal of the tag parent_id tree.
    async fn get_tag_hierarchy(&self, tag_id: &str) -> StorageResult<Vec<String>>;

    /// Count distinct atoms that have any of the given tags.
    async fn count_atoms_with_tags(
        &self,
        tag_ids: &[String],
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<i32>;
}

// ==================== Chunk/Embedding Storage ====================

/// Storage operations for chunks, embeddings, and semantic edges.
#[async_trait]
pub trait ChunkStore: Send + Sync {
    /// Get atoms with pending embedding status (limit batch size).
    async fn get_pending_embeddings(&self, limit: i32) -> StorageResult<Vec<(String, String)>>; // (atom_id, content)

    /// Mark an atom's embedding status (pending, processing, complete, failed).
    async fn set_embedding_status(
        &self,
        atom_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()>;

    /// Mark embedding status for multiple atoms in a single operation.
    async fn set_embedding_status_batch(
        &self,
        atom_ids: &[String],
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()> {
        for id in atom_ids {
            self.set_embedding_status(id, status, error).await?;
        }
        Ok(())
    }

    /// Mark an atom's tagging status.
    async fn set_tagging_status(
        &self,
        atom_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> StorageResult<()>;

    /// Save chunks and their embeddings for an atom (replaces existing).
    async fn save_chunks_and_embeddings(
        &self,
        atom_id: &str,
        chunks: &[(String, Vec<f32>)], // (chunk_content, embedding)
    ) -> StorageResult<()>;

    /// Load existing chunks for atoms so embed-only jobs can recalculate
    /// embeddings without rechunking unchanged content.
    async fn get_chunks_for_atoms(
        &self,
        atom_ids: &[String],
    ) -> StorageResult<Vec<ExistingAtomChunk>>;

    /// Update embeddings for existing chunks, preserving chunk ids/content.
    async fn update_chunk_embeddings(&self, chunks: &[(String, Vec<f32>)]) -> StorageResult<()>;

    /// Save chunks and embeddings for multiple atoms in a single transaction.
    async fn save_chunks_and_embeddings_batch(
        &self,
        atoms: &[(String, Vec<(String, Vec<f32>)>)],
    ) -> StorageResult<Vec<String>> {
        let mut succeeded = Vec::new();
        for (atom_id, chunks) in atoms {
            if self
                .save_chunks_and_embeddings(atom_id, chunks)
                .await
                .is_ok()
            {
                succeeded.push(atom_id.clone());
            }
        }
        Ok(succeeded)
    }

    /// Delete all chunks and embeddings for an atom.
    async fn delete_chunks(&self, atom_id: &str) -> StorageResult<()>;

    /// Reset atoms stuck in 'processing' status back to 'pending'.
    async fn reset_stuck_processing(&self) -> StorageResult<i32>;

    /// Reset failed embedding atoms back to pending (for auto-retry on config fix).
    async fn reset_failed_embeddings(&self) -> StorageResult<i32>;

    /// Reset only failed embedding atoms back to pending.
    async fn reset_failed_embedding_statuses(&self) -> StorageResult<i32>;

    /// Reset only failed tagging atoms back to pending when embeddings are complete.
    async fn reset_failed_tagging_statuses(&self) -> StorageResult<i32>;

    /// Rebuild semantic edges between all atoms with embeddings.
    async fn rebuild_semantic_edges(&self) -> StorageResult<i32>;

    /// Get semantic edges above a similarity threshold.
    async fn get_semantic_edges(&self, min_similarity: f32) -> StorageResult<Vec<SemanticEdge>>;

    /// Lightweight edge triples (source, target, score) sorted by score DESC.
    async fn get_semantic_edges_raw(
        &self,
        min_similarity: f32,
    ) -> StorageResult<Vec<(String, String, f32)>> {
        // Default: extract from full edges
        let edges = self.get_semantic_edges(min_similarity).await?;
        Ok(edges
            .into_iter()
            .map(|e| (e.source_atom_id, e.target_atom_id, e.similarity_score))
            .collect())
    }

    /// Get the local neighborhood graph around an atom.
    async fn get_atom_neighborhood(
        &self,
        atom_id: &str,
        depth: i32,
        min_similarity: f32,
    ) -> StorageResult<NeighborhoodGraph>;

    /// Get connection counts for all atoms (tag connections + semantic edges).
    async fn get_connection_counts(
        &self,
        min_similarity: f32,
    ) -> StorageResult<std::collections::HashMap<String, i32>>;

    /// Save tag centroid embedding.
    async fn save_tag_centroid(&self, tag_id: &str, embedding: &[f32]) -> StorageResult<()>;

    /// Recompute all tag centroid embeddings from their atoms' embeddings.
    async fn recompute_all_tag_embeddings(&self) -> StorageResult<i32>;

    /// Check sqlite-vec or equivalent vector extension version.
    async fn check_vector_extension(&self) -> StorageResult<String>;

    /// Atomically claim pending atoms for embedding: sets status to 'processing'
    /// and returns (atom_id, content) pairs. Ensures no double-processing.
    async fn claim_pending_embeddings(&self, limit: i32) -> StorageResult<Vec<(String, String)>>;

    /// Atomically claim pending atoms for embedding only when the atom's
    /// `updated_at` is older than or equal to `max_updated_at` (RFC3339).
    async fn claim_pending_embeddings_due(
        &self,
        limit: i32,
        max_updated_at: &str,
    ) -> StorageResult<Vec<(String, String)>>;

    /// Delete chunks for multiple atoms in batch.
    async fn delete_chunks_batch(&self, atom_ids: &[String]) -> StorageResult<()>;

    /// Compute semantic edges for a single atom against all other embedded atoms.
    async fn compute_semantic_edges_for_atom(
        &self,
        atom_id: &str,
        threshold: f32,
        max_edges: i32,
    ) -> StorageResult<i32>;

    /// Compute semantic edges for a batch of atoms in a single transaction.
    async fn compute_semantic_edges_batch(
        &self,
        atom_ids: &[String],
        threshold: f32,
        max_edges: i32,
    ) -> StorageResult<i32> {
        // Default implementation: process one at a time
        let mut total = 0;
        for atom_id in atom_ids {
            total += self
                .compute_semantic_edges_for_atom(atom_id, threshold, max_edges)
                .await?;
        }
        Ok(total)
    }

    /// Rebuild the full-text search index (SQLite: FTS5 rebuild, Postgres: no-op since tsvector is auto-maintained).
    async fn rebuild_fts_index(&self) -> StorageResult<()>;

    /// Atomically claim atoms that need tagging: sets tagging_status to 'processing'
    /// for atoms with embedding_status='complete' and tagging_status='pending'.
    /// Returns the atom IDs that were claimed.
    async fn claim_pending_tagging(&self) -> StorageResult<Vec<String>>;

    /// Atomically claim pending tagging only when the atom's `updated_at` is
    /// older than or equal to `max_updated_at` (RFC3339).
    async fn claim_pending_tagging_due(&self, max_updated_at: &str) -> StorageResult<Vec<String>>;

    /// Get the current embedding dimension from the vector index.
    /// Returns None if the vector index doesn't exist or dimension can't be determined.
    async fn get_embedding_dimension(&self) -> StorageResult<Option<usize>>;

    /// Recreate vector storage with a new dimension, clear old vectors, and
    /// reset embedding state while preserving chunk ids/content where possible.
    async fn recreate_vector_index(&self, dimension: usize) -> StorageResult<()>;

    /// Claim pending/processing atoms for re-embedding after dimension change.
    /// Sets status to 'processing' and returns atom IDs.
    async fn claim_pending_reembedding(&self) -> StorageResult<Vec<String>>;

    /// Claim ALL atoms for re-embedding regardless of current status.
    /// Sets status to 'processing' and returns atom IDs.
    async fn claim_all_for_reembedding(&self) -> StorageResult<Vec<String>>;

    /// Claim embedding-complete atoms for re-tagging.
    /// Sets `tagging_status` to 'processing' and returns atom IDs.
    async fn claim_all_for_retagging(&self) -> StorageResult<Vec<String>>;

    /// Delete `atom_tags` rows where `source = 'auto'` and the tag has no
    /// wiki article. Returns the number of rows deleted. Used by the
    /// "Re-tag all atoms" flow to clear stale auto-tags before re-extraction
    /// while preserving manual assignments and wiki-backed tags.
    async fn delete_auto_tags_without_wiki(&self) -> StorageResult<i32>;

    /// Atomically claim atoms that need edge computation: sets edges_status to 'processing'
    /// and returns their IDs.
    async fn claim_pending_edges(&self, limit: i32) -> StorageResult<Vec<String>>;

    /// Mark edges_status for a batch of atoms.
    async fn set_edges_status_batch(&self, atom_ids: &[String], status: &str) -> StorageResult<()>;

    /// Count atoms with pending edge computation.
    async fn count_pending_edges(&self) -> StorageResult<i32>;

    /// Upsert atom-level pipeline jobs, coalescing stage flags by atom.
    async fn enqueue_pipeline_jobs(&self, jobs: &[AtomPipelineJobRequest]) -> StorageResult<i32>;

    /// Enqueue jobs from legacy/status-column pending state. Used by startup,
    /// manual retry commands, and the draft scheduler during the queue rollout.
    async fn enqueue_pipeline_jobs_from_statuses(
        &self,
        max_updated_at: Option<&str>,
    ) -> StorageResult<i32>;

    /// Atomically claim due pipeline jobs. Expired leases are claimable again.
    async fn claim_pipeline_jobs(
        &self,
        limit: i32,
        lease_until: &str,
        now: &str,
    ) -> StorageResult<Vec<AtomPipelineJob>>;

    /// Clear claimed pipeline jobs after their requested stages reached terminal
    /// status (success, skipped, or failed). The claimed row snapshot prevents
    /// an older worker from deleting a newer pending job or refreshed lease.
    async fn clear_pipeline_jobs(&self, jobs: &[AtomPipelineJob]) -> StorageResult<()>;

    /// Count active durable pipeline jobs for this database.
    async fn count_pipeline_jobs(&self) -> StorageResult<i32>;
}

// ==================== Search Storage ====================

/// Storage operations for search (semantic, keyword, hybrid).
#[async_trait]
pub trait SearchStore: Send + Sync {
    /// Perform vector similarity search using embeddings.
    /// `created_after` is an optional ISO 8601 cutoff — only atoms created at or after
    /// this timestamp are returned. `kinds` is non-defaulted so every caller
    /// declares whether finding atoms are in scope (the UI path passes
    /// `KindFilter::All`; external tools opt in to `KindFilter::only(Captured)`).
    async fn vector_search(
        &self,
        query_embedding: &[f32],
        limit: i32,
        threshold: f32,
        tag_id: Option<&str>,
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<SemanticSearchResult>>;

    /// Perform keyword search using full-text search.
    /// `created_after` is an optional ISO 8601 cutoff — only atoms created at or after
    /// this timestamp are returned. `kinds` controls the atom-kind filter; see
    /// `vector_search` for the discipline.
    async fn keyword_search(
        &self,
        query: &str,
        limit: i32,
        tag_id: Option<&str>,
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<SemanticSearchResult>>;

    /// Find atoms similar to a given atom.
    async fn find_similar(
        &self,
        atom_id: &str,
        limit: i32,
        threshold: f32,
    ) -> StorageResult<Vec<SimilarAtomResult>>;

    /// Search for chunks (not deduplicated by atom) using keyword search.
    /// Returns individual chunk results with scores. Used by wiki agentic research.
    async fn keyword_search_chunks(
        &self,
        query: &str,
        limit: i32,
        scope_tag_ids: &[String],
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<ChunkSearchResult>>;

    /// Search for chunks using vector similarity.
    /// Returns individual chunk results with scores. Used by wiki agentic research.
    async fn vector_search_chunks(
        &self,
        query_embedding: &[f32],
        limit: i32,
        threshold: f32,
        scope_tag_ids: &[String],
        created_after: Option<&str>,
        kinds: &crate::models::KindFilter,
    ) -> StorageResult<Vec<ChunkSearchResult>>;
}

// ==================== Chat Storage ====================

/// Storage operations for chat conversations and messages.
#[async_trait]
pub trait ChatStore: Send + Sync {
    /// Create a new conversation with optional tag scope.
    async fn create_conversation(
        &self,
        tag_ids: &[String],
        title: Option<&str>,
    ) -> StorageResult<ConversationWithTags>;

    /// List conversations with optional tag filter and pagination.
    async fn get_conversations(
        &self,
        filter_tag_id: Option<&str>,
        limit: i32,
        offset: i32,
    ) -> StorageResult<Vec<ConversationWithTags>>;

    /// Get a conversation with its full message history.
    async fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> StorageResult<Option<ConversationWithMessages>>;

    /// Update conversation metadata.
    async fn update_conversation(
        &self,
        id: &str,
        title: Option<&str>,
        is_archived: Option<bool>,
    ) -> StorageResult<Conversation>;

    /// Delete a conversation and all its messages.
    async fn delete_conversation(&self, id: &str) -> StorageResult<()>;

    /// Set the tag scope for a conversation (replaces existing scope).
    async fn set_conversation_scope(
        &self,
        conversation_id: &str,
        tag_ids: &[String],
    ) -> StorageResult<ConversationWithTags>;

    /// Add a tag to a conversation's scope.
    async fn add_tag_to_scope(
        &self,
        conversation_id: &str,
        tag_id: &str,
    ) -> StorageResult<ConversationWithTags>;

    /// Remove a tag from a conversation's scope.
    async fn remove_tag_from_scope(
        &self,
        conversation_id: &str,
        tag_id: &str,
    ) -> StorageResult<ConversationWithTags>;

    /// Save a chat message (user, assistant, system, or tool).
    async fn save_message(
        &self,
        conversation_id: &str,
        role: &str,
        content: &str,
    ) -> StorageResult<ChatMessage>;

    /// Save tool calls associated with a message.
    async fn save_tool_calls(
        &self,
        message_id: &str,
        tool_calls: &[ChatToolCall],
    ) -> StorageResult<()>;

    /// Save citations for a message.
    async fn save_citations(
        &self,
        message_id: &str,
        citations: &[ChatCitation],
    ) -> StorageResult<()>;

    /// Get the tag IDs that scope a conversation.
    async fn get_scope_tag_ids(&self, conversation_id: &str) -> StorageResult<Vec<String>>;

    /// Get a human-readable scope description for the system prompt.
    async fn get_scope_description(&self, tag_ids: &[String]) -> StorageResult<String>;
}

// ==================== Wiki Storage ====================

/// Storage operations for wiki articles and their metadata.
#[async_trait]
pub trait WikiStore: Send + Sync {
    /// Get a wiki article with its citations for a tag.
    async fn get_wiki(&self, tag_id: &str) -> StorageResult<Option<WikiArticleWithCitations>>;

    /// Get wiki article status (exists, atom count, etc.).
    async fn get_wiki_status(&self, tag_id: &str) -> StorageResult<WikiArticleStatus>;

    /// Save or update a wiki article with citations.
    async fn save_wiki(
        &self,
        tag_id: &str,
        content: &str,
        citations: &[WikiCitation],
        atom_count: i32,
    ) -> StorageResult<WikiArticleWithCitations>;

    /// Save or update a wiki article with citations and cross-reference links.
    /// This is the full-fidelity save used by wiki generation (includes links).
    async fn save_wiki_with_links(
        &self,
        article: &WikiArticle,
        citations: &[WikiCitation],
        links: &[WikiLink],
    ) -> StorageResult<()>;

    /// Delete a wiki article and its citations.
    async fn delete_wiki(&self, tag_id: &str) -> StorageResult<()>;

    /// Get cross-reference links from a wiki article to other wiki articles.
    async fn get_wiki_links(&self, tag_id: &str) -> StorageResult<Vec<WikiLink>>;

    /// List all versions of a wiki article.
    async fn list_wiki_versions(&self, tag_id: &str) -> StorageResult<Vec<WikiVersionSummary>>;

    /// Get a specific wiki article version.
    async fn get_wiki_version(&self, version_id: &str)
        -> StorageResult<Option<WikiArticleVersion>>;

    /// Get all wiki articles (summaries for list view).
    async fn get_all_wiki_articles(&self) -> StorageResult<Vec<WikiArticleSummary>>;

    /// Select chunks for wiki article generation, ranked by centroid similarity.
    ///
    /// Returns (chunks, atom_count) for the tag hierarchy. Uses centroid embedding
    /// for ranked retrieval if available, falls back to insertion order.
    async fn get_wiki_source_chunks(
        &self,
        tag_id: &str,
        max_source_tokens: usize,
    ) -> StorageResult<(Vec<ChunkWithContext>, i32)>;

    /// Select chunks for wiki article update (new atoms since last update).
    ///
    /// Returns None if no new atoms have been added since `last_update`.
    /// Otherwise returns (new_chunks, atom_count). If new atoms exist but no
    /// selectable chunks are available yet, returns an error so callers do not
    /// advance the article baseline before the async chunking pipeline catches up.
    async fn get_wiki_update_chunks(
        &self,
        tag_id: &str,
        last_update: &str,
        max_source_tokens: usize,
    ) -> StorageResult<Option<(Vec<ChunkWithContext>, i32)>>;

    /// Save a wiki proposal (upsert — supersedes any existing proposal for the tag).
    async fn save_wiki_proposal(&self, proposal: &WikiProposal) -> StorageResult<()>;

    /// Get the pending wiki proposal for a tag, if any.
    async fn get_wiki_proposal(&self, tag_id: &str) -> StorageResult<Option<WikiProposal>>;

    /// Delete the pending wiki proposal for a tag (idempotent).
    async fn delete_wiki_proposal(&self, tag_id: &str) -> StorageResult<()>;

    /// Advance the article baseline without changing content: update `atom_count`
    /// to the current tag-hierarchy total and `updated_at` to now. If
    /// `max_current_count` is set and the current total exceeds it, leave the
    /// article unchanged and return `false`.
    async fn advance_wiki_baseline(
        &self,
        tag_id: &str,
        max_current_count: Option<i32>,
    ) -> StorageResult<bool>;
}

// ==================== Feed Storage ====================

/// Storage operations for RSS/Atom feed subscriptions.
#[async_trait]
pub trait FeedStore: Send + Sync {
    /// Create a new feed subscription.
    async fn create_feed(
        &self,
        url: &str,
        title: Option<&str>,
        site_url: Option<&str>,
        poll_interval: i32,
        tag_ids: &[String],
    ) -> StorageResult<Feed>;

    /// List all feed subscriptions.
    async fn list_feeds(&self) -> StorageResult<Vec<Feed>>;

    /// Get a single feed by ID.
    async fn get_feed(&self, id: &str) -> StorageResult<Feed>;

    /// Update a feed subscription.
    async fn update_feed(
        &self,
        id: &str,
        title: Option<&str>,
        poll_interval: Option<i32>,
        is_paused: Option<bool>,
        tag_ids: Option<&[String]>,
    ) -> StorageResult<Feed>;

    /// Delete a feed subscription.
    async fn delete_feed(&self, id: &str) -> StorageResult<()>;

    /// Get feeds that are due for polling.
    async fn get_due_feeds(&self) -> StorageResult<Vec<Feed>>;

    /// Record that a feed was polled (update timestamp and error).
    async fn mark_feed_polled(&self, id: &str, error: Option<&str>) -> StorageResult<()>;

    /// Atomically claim a feed item GUID. Returns true if this call claimed it.
    async fn claim_feed_item(&self, feed_id: &str, guid: &str) -> StorageResult<bool>;

    /// Mark a claimed feed item as successfully ingested with its atom_id.
    async fn complete_feed_item(
        &self,
        feed_id: &str,
        guid: &str,
        atom_id: &str,
    ) -> StorageResult<()>;

    /// Mark a claimed feed item as skipped with a reason.
    async fn mark_feed_item_skipped(
        &self,
        feed_id: &str,
        guid: &str,
        reason: &str,
    ) -> StorageResult<()>;

    /// Backfill feed metadata (title, site_url) using COALESCE to avoid overwriting existing values.
    async fn backfill_feed_metadata(
        &self,
        id: &str,
        title: Option<&str>,
        site_url: Option<&str>,
    ) -> StorageResult<()>;
}

// ==================== Clustering Storage ====================

/// Storage operations for atom clustering.
#[async_trait]
pub trait ClusterStore: Send + Sync {
    /// Compute clusters from atom embeddings.
    async fn compute_clusters(
        &self,
        min_similarity: f32,
        min_cluster_size: i32,
    ) -> StorageResult<Vec<AtomCluster>>;

    /// Save computed clusters (replaces existing).
    async fn save_clusters(&self, clusters: &[AtomCluster]) -> StorageResult<()>;

    /// Get cached clusters (recomputes if stale).
    async fn get_clusters(&self) -> StorageResult<Vec<AtomCluster>>;

    /// Get the hierarchical canvas level for a given parent.
    async fn get_canvas_level(
        &self,
        parent_id: Option<&str>,
        children_hint: Option<Vec<String>>,
    ) -> StorageResult<CanvasLevel>;

    /// Enrich clusters (computed without DB) with dominant tag names.
    async fn enrich_clusters_with_tags(
        &self,
        clusters: Vec<AtomCluster>,
    ) -> StorageResult<Vec<AtomCluster>> {
        Ok(clusters)
    }
}

// ==================== Settings Storage ====================

/// Storage operations for key-value settings.
#[async_trait]
pub trait SettingsStore: Send + Sync {
    /// Get all settings as a key-value map.
    async fn get_all_settings(&self) -> StorageResult<std::collections::HashMap<String, String>>;

    /// Get a single setting by key.
    async fn get_setting(&self, key: &str) -> StorageResult<Option<String>>;

    /// Set a setting value (upsert).
    async fn set_setting(&self, key: &str, value: &str) -> StorageResult<()>;

    /// Delete a setting row. No-op if the key isn't present. Used to clear a
    /// per-DB override so the resolver falls back to the workspace default.
    async fn delete_setting(&self, key: &str) -> StorageResult<()>;
}

// ==================== Token Storage ====================

/// Storage operations for API tokens.
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Create a new named API token. Returns (metadata, raw_token).
    async fn create_api_token(
        &self,
        name: &str,
    ) -> StorageResult<(crate::tokens::ApiTokenInfo, String)>;

    /// List all API tokens (metadata only).
    async fn list_api_tokens(&self) -> StorageResult<Vec<crate::tokens::ApiTokenInfo>>;

    /// Verify a raw API token. Returns token info if valid and not revoked.
    async fn verify_api_token(
        &self,
        raw_token: &str,
    ) -> StorageResult<Option<crate::tokens::ApiTokenInfo>>;

    /// Revoke an API token by ID.
    async fn revoke_api_token(&self, id: &str) -> StorageResult<()>;

    /// Update the last_used_at timestamp for a token.
    async fn update_token_last_used(&self, id: &str) -> StorageResult<()>;

    /// Migrate legacy server_auth_token to API tokens table.
    async fn migrate_legacy_token(&self) -> StorageResult<bool>;

    /// Ensure at least one token exists. Creates a "default" token if none exist.
    async fn ensure_default_token(
        &self,
    ) -> StorageResult<Option<(crate::tokens::ApiTokenInfo, String)>>;
}

// ==================== Database Management Storage ====================

/// Storage operations for managing logical databases.
#[async_trait]
pub trait DatabaseStore: Send + Sync {
    /// List all registered databases.
    async fn list_databases(&self) -> StorageResult<Vec<crate::registry::DatabaseInfo>>;

    /// Create a new database entry. Returns the new database info.
    async fn create_database(&self, name: &str) -> StorageResult<crate::registry::DatabaseInfo>;

    /// Rename a database.
    async fn rename_database(&self, id: &str, name: &str) -> StorageResult<()>;

    /// Delete a database entry (cannot delete default).
    async fn delete_database(&self, id: &str) -> StorageResult<()>;

    /// Get the ID of the default database.
    async fn get_default_database_id(&self) -> StorageResult<String>;

    /// Set a database as the new default.
    async fn set_default_database(&self, id: &str) -> StorageResult<()>;

    /// Purge all data for a logical database (delete all rows with the given db_id).
    /// Called after deleting the database entry to avoid orphaned data.
    async fn purge_database_data(&self, db_id: &str) -> StorageResult<()>;
}

// ==================== Task Run Storage ====================

/// Storage operations for the `task_runs` execution ledger.
///
/// The scheduler ledger (`scheduler::ledger`) composes these into a state
/// machine; the trait itself is intentionally CRUD-shaped with the
/// conditional-update predicate baked into each writer so the contention
/// semantics live in SQL, not in Rust. See `docs/plans/reports.md`
/// §"Execution ledger — task_runs" for the contract.
#[async_trait]
pub trait TaskRunStore: Send + Sync {
    /// Insert a fresh run row (state is whatever the caller passes — typically
    /// `pending`). The caller owns id and all timestamps.
    async fn insert_task_run(&self, run: &crate::models::TaskRun) -> StorageResult<()>;

    /// Best-effort variant of [`Self::insert_task_run`]. Returns `false`
    /// when the `idx_task_runs_active_unique` partial index rejected the
    /// insert (another worker already created an active row for this
    /// task/subject). Used by `claim_or_create` to close the race window
    /// between `find_active_task_run` and the actual insert: without this
    /// check, two concurrent claimers can both observe "no active row"
    /// and insert distinct rows that each get claimed, executing the
    /// same report twice.
    async fn try_insert_task_run(&self, run: &crate::models::TaskRun) -> StorageResult<bool>;

    /// Read a single row by id.
    async fn get_task_run(&self, id: &str) -> StorageResult<Option<crate::models::TaskRun>>;

    /// Find the next-runnable row for `(task_id, subject_id)`: either a
    /// `pending` row whose `next_attempt_at <= now`, or a `running` row whose
    /// `lease_until < now` (crash-recovery candidate). Returns the row with
    /// the earliest `next_attempt_at` first.
    async fn find_runnable_task_run(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
        now: &str,
    ) -> StorageResult<Option<crate::models::TaskRun>>;

    /// Find any non-terminal row for `(task_id, subject_id)` regardless of
    /// timing — i.e., pending OR running, with `next_attempt_at` and
    /// `lease_until` ignored. The intended caller is `claim_or_create`: if
    /// a non-runnable active row exists (e.g., running with a live lease, or
    /// pending with `next_attempt_at` in the future), inserting a fresh
    /// pending row would race past it and start a duplicate execution.
    /// Most-recent first.
    async fn find_active_task_run(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
    ) -> StorageResult<Option<crate::models::TaskRun>>;

    /// Conditional `pending → running` transition. Returns `true` iff this
    /// caller won the claim (predicate `id = ? AND state = 'pending'` held).
    /// On success, sets `started_at`, `lease_until`, `updated_at`, and bumps
    /// `attempts` by 1.
    async fn claim_pending_task_run(
        &self,
        id: &str,
        now: &str,
        lease_until: &str,
    ) -> StorageResult<bool>;

    /// Conditional crash-recovery reclaim. Predicate is
    /// `id = ? AND state = 'running' AND lease_until < ?`. On success, sets a
    /// fresh `lease_until` and `started_at`, leaves `attempts` untouched
    /// (we don't punish a process crash as a logic failure).
    async fn reclaim_expired_task_run(
        &self,
        id: &str,
        now: &str,
        lease_until: &str,
    ) -> StorageResult<bool>;

    /// Conditional lease refresh used by the heartbeat task. Predicate is
    /// `id = ? AND state = 'running' AND lease_until = expected_lease` —
    /// the extra lease fence protects a slow worker against a peer that
    /// reclaimed the row after the worker's lease expired (the peer would
    /// have replaced our `lease_until` value, so our refresh no longer
    /// matches). Returns `false` when the row has moved on (terminal state,
    /// or reclaimed by another worker).
    async fn heartbeat_task_run(
        &self,
        id: &str,
        expected_lease: &str,
        new_lease_until: &str,
    ) -> StorageResult<bool>;

    /// Terminal `running → succeeded`. Predicate is
    /// `id = ? AND state = 'running' AND lease_until = expected_lease` so a
    /// stale worker whose lease was already reclaimed by a peer can't
    /// double-complete a run that's been re-attempted underneath it.
    async fn complete_task_run(
        &self,
        id: &str,
        expected_lease: &str,
        result_id: Option<&str>,
        finished_at: &str,
    ) -> StorageResult<bool>;

    /// `running → pending` with `next_attempt_at` set in the future and the
    /// stored `attempts` left at the current value (the claim already
    /// incremented it). Clears `lease_until`. Same fenced predicate as
    /// `complete_task_run`.
    async fn fail_task_run_retry(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        now: &str,
        next_attempt_at: &str,
    ) -> StorageResult<bool>;

    /// Terminal `running → abandoned`. Same fenced predicate as the other
    /// terminal writers.
    async fn fail_task_run_abandon(
        &self,
        id: &str,
        expected_lease: &str,
        last_error: &str,
        finished_at: &str,
    ) -> StorageResult<bool>;

    /// Most-recent-first run history for a task. `subject_id = None` matches
    /// any subject_id (history-by-task); `Some(...)` filters to that subject.
    async fn list_recent_task_runs(
        &self,
        task_id: &str,
        subject_id: Option<&str>,
        limit: i32,
    ) -> StorageResult<Vec<crate::models::TaskRun>>;
}

// ==================== Reports ====================

/// Storage operations for reports, finding provenance, and citations.
///
/// CRUD on the definitions, plus the transactional finding-write helper
/// that wraps the atom + provenance + citation rows in a single commit.
/// `update_report_cache` is the fast-path writer for the advisory cache
/// fields (`last_run_at`, `last_finding_atom_id`, `last_error`); the
/// authoritative state lives on `task_runs` and `report_findings`.
#[async_trait]
pub trait ReportStore: Send + Sync {
    /// List every report definition, most-recently-updated first.
    async fn list_reports(&self) -> StorageResult<Vec<crate::models::Report>>;

    /// List enabled reports only. Fast path for the scheduler tick.
    async fn list_enabled_reports(&self) -> StorageResult<Vec<crate::models::Report>>;

    async fn get_report(&self, id: &str) -> StorageResult<Option<crate::models::Report>>;

    /// Insert a fresh report. The storage layer generates `id`, timestamps,
    /// and the cache columns; the request carries everything else.
    async fn insert_report(
        &self,
        request: &crate::models::CreateReportRequest,
    ) -> StorageResult<crate::models::Report>;

    /// Partial-update by id. Only `Some` fields are written.
    async fn update_report(
        &self,
        id: &str,
        request: &crate::models::UpdateReportRequest,
    ) -> StorageResult<crate::models::Report>;

    async fn set_report_enabled(&self, id: &str, enabled: bool) -> StorageResult<()>;

    async fn delete_report(&self, id: &str) -> StorageResult<()>;

    /// Write the cache columns (`last_run_at`, `last_finding_atom_id`,
    /// `last_error`) after a run terminates. Optional fields lets callers
    /// pass `None` for "leave previous value" or explicit `Some(None)` for
    /// "clear" — encoded as `Option<Option<...>>` everywhere except where
    /// "leave unchanged" is the only sensible no-op (every cache column).
    ///
    /// `last_run_at = None` leaves the column untouched. This is the path
    /// taken by failure stamping: a first-run failure must not write an
    /// empty string into `last_run_at` (which would round-trip back as
    /// `Some("")` and then fail RFC3339 parsing in `schedule::is_due`,
    /// effectively wedging the report).
    async fn update_report_cache(
        &self,
        id: &str,
        last_run_at: Option<&str>,
        last_finding_atom_id: Option<Option<&str>>,
        last_error: Option<Option<&str>>,
    ) -> StorageResult<()>;

    /// Most-recent-first list of provenance rows for a report, joined with
    /// the finding atom so the dashboard history view can render snippets.
    async fn list_findings_for_report(
        &self,
        report_id: &str,
        limit: i32,
    ) -> StorageResult<Vec<(crate::models::ReportFinding, crate::models::AtomWithTags)>>;

    /// Lookup the provenance row for a finding atom. None if the atom is
    /// either not a finding or its provenance row was removed.
    async fn get_finding_provenance(
        &self,
        finding_atom_id: &str,
    ) -> StorageResult<Option<crate::models::ReportFinding>>;

    /// Set of finding atom ids previously produced by `report_id`. Used by
    /// the agent loop to exclude a report's own prior output from its
    /// semantic_search results.
    async fn list_finding_atom_ids_for_report(&self, report_id: &str)
        -> StorageResult<Vec<String>>;

    /// Every citation row for a finding atom, ordered by `position` ASC so
    /// the markdown renderer's `[N]` lookup is a direct index. Returns
    /// empty when the atom isn't a finding (or has no citations).
    async fn list_citations_for_finding(
        &self,
        finding_atom_id: &str,
    ) -> StorageResult<Vec<crate::models::ReportFindingCitation>>;

    /// One-shot transactional write of the finding atom, its tags, its
    /// provenance row, and all citation rows. On any error the entire
    /// commit is rolled back so a partial write cannot orphan a finding
    /// atom without its provenance.
    async fn write_finding_transactionally(
        &self,
        atom_request: &crate::CreateAtomRequest,
        atom_id: &str,
        atom_created_at: &str,
        provenance: &crate::models::ReportFinding,
        citations: &[crate::models::ReportFindingCitation],
    ) -> StorageResult<crate::models::AtomWithTags>;
}

/// Minimal raw-access surface kept alive solely so the phase-3 briefings →
/// finding-atoms migration can read the legacy `briefings` /
/// `briefing_citations` tables and drop them afterwards.
///
/// The full `BriefingStore` was retired in phase 3; everything user-facing
/// moved onto the reports primitive. These two methods are the only
/// remaining touchpoints, kept on the supertrait so the migration can run
/// on any DB an older deployment is upgraded from.
#[async_trait]
pub trait LegacyBriefingsMigrationStore: Send + Sync {
    /// Stream every legacy briefing row joined with its citation rows in a
    /// deterministic order (briefing.created_at ASC, citation.citation_index
    /// ASC). Returns an empty Vec if the tables have already been dropped.
    async fn fetch_legacy_briefings(
        &self,
    ) -> StorageResult<Vec<crate::reports::seed::LegacyBriefingRow>>;

    /// Drop the `briefings` and `briefing_citations` tables. Idempotent —
    /// `DROP TABLE IF EXISTS` semantics so a re-run after a successful
    /// migration is a no-op.
    async fn drop_legacy_briefing_tables(&self) -> StorageResult<()>;
}

// ==================== Supertrait ====================

/// Combined storage trait. Every storage backend must implement all sub-traits.
///
/// This is the main trait that `AtomicCore` holds as `Arc<dyn Storage>`.
#[async_trait]
pub trait Storage:
    AtomStore
    + TagStore
    + ChunkStore
    + SearchStore
    + ChatStore
    + WikiStore
    + FeedStore
    + ClusterStore
    + SettingsStore
    + TokenStore
    + DatabaseStore
    + TaskRunStore
    + ReportStore
    + LegacyBriefingsMigrationStore
    + Send
    + Sync
{
    /// Initialize the storage backend (run migrations, create tables, etc.).
    async fn initialize(&self) -> StorageResult<()>;

    /// Graceful shutdown (optimize, flush, etc.).
    async fn shutdown(&self) -> StorageResult<()>;

    /// Get the database/storage path (for display purposes).
    fn storage_path(&self) -> &std::path::Path;
}
