//! Embedding generation pipeline with callback-based events
//!
//! This module handles:
//! - Embedding generation via provider abstraction
//! - Tag extraction via LLM
//! - Semantic edge computation
//! - Callback-based event notification

use crate::chunking::chunk_content;
use crate::extraction::extract_tags_from_content;
use crate::providers::traits::EmbeddingConfig;
use crate::providers::{get_embedding_provider, ProviderConfig, ProviderType};
use crate::storage::StorageBackend;
use crate::CanvasCache;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use uuid::Uuid;

/// Events emitted during the embedding/tagging pipeline
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EmbeddingEvent {
    /// Embedding generation started for an atom
    Started { atom_id: String },
    /// Embedding generation completed successfully
    EmbeddingComplete { atom_id: String },
    /// Embedding generation failed
    EmbeddingFailed { atom_id: String, error: String },
    /// Tag extraction completed
    TaggingComplete {
        atom_id: String,
        tags_extracted: Vec<String>,
        new_tags_created: Vec<String>,
    },
    /// Tag extraction failed
    TaggingFailed { atom_id: String, error: String },
    /// Tag extraction was skipped (disabled or no API key)
    TaggingSkipped { atom_id: String },
    /// Progress update for batch embedding pipeline
    BatchProgress {
        batch_id: String,
        phase: String,
        completed: usize,
        total: usize,
    },
    /// A durable queue run was claimed for background processing.
    PipelineQueueStarted {
        run_id: String,
        total_jobs: usize,
        embedding_total: usize,
    },
    /// A queue stage has work to report. Tagging totals are emitted only after
    /// embedding determines which requested atoms actually reached tagging.
    PipelineQueueProgress {
        run_id: String,
        stage: String,
        completed: usize,
        total: usize,
    },
    /// A durable queue run finished processing its claimed jobs.
    PipelineQueueCompleted {
        run_id: String,
        total_jobs: usize,
        failed_jobs: usize,
    },
}

/// Generate embeddings via provider abstraction (batch support)
/// Uses ProviderConfig to determine which provider to use.
/// Includes retry with exponential backoff for transient failures.
pub async fn generate_embeddings_with_config(
    config: &ProviderConfig,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, EmbedError> {
    let _permit = crate::executor::EMBEDDING_SEMAPHORE
        .acquire()
        .await
        .expect("Embedding semaphore closed unexpectedly");

    let provider = get_embedding_provider(config).map_err(|e| EmbedError {
        message: e.to_string(),
        retryable: false,
        batch_reducible: false,
    })?;
    let embed_config = EmbeddingConfig::new(config.embedding_model());
    let model = config.embedding_model();
    let provider_type = format!("{:?}", config.provider_type);

    let mut last_error = String::new();
    let mut last_retryable = true;
    let mut last_batch_reducible = false;
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
        }

        match provider.embed_batch(texts, &embed_config).await {
            Ok(embeddings) => return Ok(embeddings),
            Err(e) => {
                last_error = e.to_string();
                last_retryable = e.is_retryable();
                last_batch_reducible = e.is_batch_reducible();
                if last_retryable {
                    tracing::warn!(
                        attempt = attempt + 1,
                        model = %model,
                        provider = %provider_type,
                        batch_size = texts.len(),
                        error = %last_error,
                        "Embedding attempt failed (retryable)"
                    );
                    continue;
                } else {
                    tracing::error!(
                        model = %model,
                        provider = %provider_type,
                        batch_size = texts.len(),
                        error = %last_error,
                        "Embedding failed (non-retryable)"
                    );
                    break;
                }
            }
        }
    }

    Err(EmbedError {
        message: last_error,
        retryable: last_retryable,
        batch_reducible: last_batch_reducible,
    })
}

/// Error from embedding generation with retryability info
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct EmbedError {
    pub message: String,
    pub retryable: bool,
    /// True when reducing batch size might resolve the error (e.g. 400 from
    /// providers that enforce smaller batch limits than our default).
    pub batch_reducible: bool,
}

/// Maximum texts per embedding API call for cross-atom batching.
/// At ~800 tokens/chunk this yields ~24k tokens per API call, which fits
/// within most providers' limits. The adaptive retry will split further
/// if a provider rejects the batch size.
const EMBEDDING_BATCH_SIZE: usize = 30;

/// Number of atom bodies to fetch/chunk at once from storage.
const ATOM_FETCH_BATCH_SIZE: usize = 200;

/// Target number of chunks to embed before flushing atom completion status.
/// Provider calls are still batched by `EMBEDDING_BATCH_SIZE`; this controls
/// how many of those provider batches can accumulate before atoms complete.
/// A single large atom may exceed this because atom completion is all-or-nothing.
const EMBEDDING_GROUP_CHUNK_TARGET: usize = EMBEDDING_BATCH_SIZE * 10;

/// Metadata for a chunk awaiting embedding
#[derive(Clone)]
struct PendingChunk {
    atom_id: String,
    existing_chunk_id: Option<String>,
    chunk_index: usize,
    content: String,
}

/// Input source for the embedding batch pipeline.
pub enum AtomInput {
    /// Content already loaded (e.g. from import or bulk create)
    Preloaded(Vec<(String, String)>),
    /// Only atom IDs — content will be loaded per-group from storage
    IdsOnly(Vec<String>),
}

enum TaggingPolicy {
    All,
    None,
}

impl TaggingPolicy {
    fn should_tag(&self, _atom_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
        }
    }

    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

struct QueueRunProgress {
    run_id: String,
    embedding_completed: AtomicUsize,
    tagging_total: AtomicUsize,
    tagging_completed: AtomicUsize,
    failed_jobs: AtomicUsize,
}

impl QueueRunProgress {
    fn new(run_id: String) -> Self {
        Self {
            run_id,
            embedding_completed: AtomicUsize::new(0),
            tagging_total: AtomicUsize::new(0),
            tagging_completed: AtomicUsize::new(0),
            failed_jobs: AtomicUsize::new(0),
        }
    }

    fn record_embedding_done<F>(&self, total: usize, on_event: &F)
    where
        F: Fn(EmbeddingEvent),
    {
        let completed = self.embedding_completed.fetch_add(1, Ordering::Relaxed) + 1;
        on_event(EmbeddingEvent::PipelineQueueProgress {
            run_id: self.run_id.clone(),
            stage: "embedding".to_string(),
            completed,
            total,
        });
    }

    fn add_tagging_total<F>(&self, n: usize, on_event: &F)
    where
        F: Fn(EmbeddingEvent),
    {
        if n == 0 {
            return;
        }
        let total = self.tagging_total.fetch_add(n, Ordering::Relaxed) + n;
        let completed = self.tagging_completed.load(Ordering::Relaxed);
        on_event(EmbeddingEvent::PipelineQueueProgress {
            run_id: self.run_id.clone(),
            stage: "tagging".to_string(),
            completed,
            total,
        });
    }

    fn record_tagging_done<F>(&self, on_event: &F)
    where
        F: Fn(EmbeddingEvent),
    {
        let completed = self.tagging_completed.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.tagging_total.load(Ordering::Relaxed);
        on_event(EmbeddingEvent::PipelineQueueProgress {
            run_id: self.run_id.clone(),
            stage: "tagging".to_string(),
            completed,
            total,
        });
    }

    fn record_failed_job(&self) {
        self.failed_jobs.fetch_add(1, Ordering::Relaxed);
    }

    fn failed_jobs(&self) -> usize {
        self.failed_jobs.load(Ordering::Relaxed)
    }
}

/// Strategy used by the embedding stage for an atom.
///
/// The current implementation always uses whole-atom rechunking. The enum makes
/// that decision explicit so dirty-chunk/incremental embedding can be added
/// behind the same pipeline stage later without changing enqueue call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingStrategy {
    RechunkWholeAtom,
    IncrementalDirtyChunks,
}

impl EmbeddingStrategy {
    fn from_settings(settings: &HashMap<String, String>) -> Self {
        match settings.get("embedding_strategy").map(|s| s.as_str()) {
            Some("incremental_dirty_chunks") => Self::IncrementalDirtyChunks,
            _ => Self::RechunkWholeAtom,
        }
    }
}

/// Strategy used by the auto-tagging stage for an atom.
///
/// `TruncatedFullContent` preserves today's cost-bounded behavior. The
/// `ChunkAssisted` variant is a future hook for large atoms, where tagging can
/// use already-persisted chunks from the embedding stage and consolidate the
/// result back into atom-level tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaggingStrategy {
    TruncatedFullContent,
    ChunkAssisted,
}

impl TaggingStrategy {
    fn from_settings(settings: &HashMap<String, String>) -> Self {
        match settings.get("tagging_strategy").map(|s| s.as_str()) {
            Some("chunk_assisted") => Self::ChunkAssisted,
            _ => Self::TruncatedFullContent,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TaggingOutcome {
    Complete {
        tags_extracted: Vec<String>,
        new_tags_created: Vec<String>,
    },
    Skipped,
}

impl TaggingOutcome {
    fn into_event(self, atom_id: String) -> EmbeddingEvent {
        match self {
            TaggingOutcome::Complete {
                tags_extracted,
                new_tags_created,
            } => EmbeddingEvent::TaggingComplete {
                atom_id,
                tags_extracted,
                new_tags_created,
            },
            TaggingOutcome::Skipped => EmbeddingEvent::TaggingSkipped { atom_id },
        }
    }
}

/// Embed a list of chunks in adaptive batches.
/// Splits into batches of EMBEDDING_BATCH_SIZE, calls the API, and on failure
/// retries at half batch size recursively. Returns (embedded, failed: Vec<(atom_id, error)>).
async fn embed_chunks_batched(
    config: &ProviderConfig,
    chunks: Vec<PendingChunk>,
) -> (Vec<(PendingChunk, Vec<f32>)>, Vec<(String, String)>) {
    if chunks.is_empty() {
        return (vec![], vec![]);
    }

    let mut results: Vec<(PendingChunk, Vec<f32>)> = Vec::with_capacity(chunks.len());
    let mut failed_atoms: Vec<(String, String)> = Vec::new();

    // Split chunks into batches
    let batches: Vec<Vec<PendingChunk>> = chunks
        .into_iter()
        .collect::<Vec<_>>()
        .chunks(EMBEDDING_BATCH_SIZE)
        .map(|c| c.to_vec())
        .collect();

    let total_batches = batches.len();
    for (batch_idx, batch) in batches.into_iter().enumerate() {
        tracing::info!(
            batch = batch_idx + 1,
            total_batches,
            chunks = batch.len(),
            "Embedding batch"
        );
        let (mut successes, mut failures) = embed_batch_adaptive(config, batch).await;
        results.append(&mut successes);
        failed_atoms.append(&mut failures);
    }

    failed_atoms.sort_by(|a, b| a.0.cmp(&b.0));
    failed_atoms.dedup_by(|a, b| a.0 == b.0);
    (results, failed_atoms)
}

/// Try to embed a batch. On failure, split in half and retry each half.
/// Base case: single chunk failure returns the (atom_id, error) as failed.
fn embed_batch_adaptive(
    config: &ProviderConfig,
    batch: Vec<PendingChunk>,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = (Vec<(PendingChunk, Vec<f32>)>, Vec<(String, String)>)>
            + Send
            + '_,
    >,
> {
    Box::pin(async move {
        if batch.is_empty() {
            return (vec![], vec![]);
        }

        let texts: Vec<String> = batch.iter().map(|c| c.content.clone()).collect();

        match generate_embeddings_with_config(config, &texts).await {
            Ok(embeddings) => {
                let results: Vec<_> = batch.into_iter().zip(embeddings.into_iter()).collect();
                (results, vec![])
            }
            Err(e) => {
                let can_split = batch.len() > 1 && (e.retryable || e.batch_reducible);
                if !can_split {
                    // Unsplittable: single chunk, or non-retryable non-batch error
                    if batch.len() > 1 {
                        tracing::error!(
                            batch_size = batch.len(),
                            error = %e.message,
                            "Non-retryable embedding error, failing entire batch"
                        );
                    } else {
                        tracing::error!(
                            atom_id = %batch[0].atom_id,
                            chunk_index = batch[0].chunk_index,
                            error = %e.message,
                            "Single chunk embedding failed after retries"
                        );
                    }
                    let failed: Vec<_> = batch
                        .iter()
                        .map(|c| (c.atom_id.clone(), e.message.clone()))
                        .collect();
                    // dedup by atom_id
                    let mut seen = std::collections::HashSet::new();
                    let failed = failed
                        .into_iter()
                        .filter(|(id, _)| seen.insert(id.clone()))
                        .collect();
                    (vec![], failed)
                } else {
                    // Split in half and retry each half
                    let mid = batch.len() / 2;
                    let (first_half, second_half): (Vec<_>, Vec<_>) =
                        batch.into_iter().enumerate().partition(|(i, _)| *i < mid);
                    let first: Vec<PendingChunk> = first_half.into_iter().map(|(_, c)| c).collect();
                    let second: Vec<PendingChunk> =
                        second_half.into_iter().map(|(_, c)| c).collect();

                    tracing::warn!(
                        original_size = mid * 2,
                        first_half = first.len(),
                        second_half = second.len(),
                        "Batch failed, retrying as 2 smaller batches"
                    );

                    let (mut r1, mut f1) = embed_batch_adaptive(config, first).await;
                    let (mut r2, mut f2) = embed_batch_adaptive(config, second).await;
                    r1.append(&mut r2);
                    f1.append(&mut f2);
                    (r1, f1)
                }
            }
        }
    })
}

/// Generate embeddings via OpenRouter API (batch support)
/// DEPRECATED: Use generate_embeddings_with_config instead
/// Kept for backward compatibility with existing code
pub async fn generate_openrouter_embeddings_public(
    _client: &reqwest::Client,
    api_key: &str,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    use crate::providers::openrouter::OpenRouterProvider;
    use crate::providers::traits::EmbeddingProvider;

    let provider = OpenRouterProvider::new(api_key.to_string());
    let config = EmbeddingConfig::new("openai/text-embedding-3-small");

    provider
        .embed_batch(texts, &config)
        .await
        .map_err(|e| e.to_string())
}

/// Convert f32 vector to binary blob for sqlite-vec
pub fn f32_vec_to_blob_public(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Process ONLY embedding generation for an atom (no tag extraction)
/// This is the fast phase - just embedding API calls
///
/// Steps:
/// 1. Set embedding_status to 'processing'
/// 2. Delete existing chunks
/// 3. Chunk content
/// 4. Generate embeddings via provider
/// 5. Store chunks and embeddings
/// 6. Set embedding_status to 'complete'
/// 7. Mark graph maintenance pending
pub async fn process_embedding_only(
    storage: &StorageBackend,
    atom_id: &str,
    content: &str,
) -> Result<(), String> {
    process_embedding_only_inner(storage, atom_id, content, false, None).await
}

/// Process embedding with externally-provided settings (from registry).
pub async fn process_embedding_only_with_settings(
    storage: &StorageBackend,
    atom_id: &str,
    content: &str,
    settings_map: HashMap<String, String>,
) -> Result<(), String> {
    process_embedding_only_inner(storage, atom_id, content, false, Some(settings_map)).await
}

/// Inner implementation with graph maintenance control.
/// When `skip_edges` is true, graph maintenance is left to the caller.
/// When `external_settings` is Some, uses those settings instead of reading from the data db.
async fn process_embedding_only_inner(
    storage: &StorageBackend,
    atom_id: &str,
    content: &str,
    skip_edges: bool,
    external_settings: Option<HashMap<String, String>>,
) -> Result<(), String> {
    // Set embedding status to processing
    storage
        .set_embedding_status_sync(atom_id, "processing", None)
        .await
        .map_err(|e| e.to_string())?;

    // Get settings for embeddings (from the caller's resolved map if
    // provided, otherwise the storage layer's global tier — provider config
    // is deployment-wide, never per-DB)
    let settings_map = match external_settings {
        Some(ref s) => s.clone(),
        None => storage
            .get_global_settings_sync()
            .await
            .map_err(|e| e.to_string())?,
    };
    let provider_config = ProviderConfig::from_settings(&settings_map);
    let embedding_strategy = EmbeddingStrategy::from_settings(&settings_map);

    // Validate provider configuration
    if provider_config.provider_type == ProviderType::OpenRouter
        && provider_config.openrouter_api_key.is_none()
    {
        return Err("OpenRouter API key not configured. Please set it in Settings.".to_string());
    }

    match embedding_strategy {
        EmbeddingStrategy::RechunkWholeAtom => {}
        EmbeddingStrategy::IncrementalDirtyChunks => {
            tracing::warn!(
                atom_id,
                "incremental dirty-chunk embedding requested but not implemented; falling back to whole-atom rechunking"
            );
        }
    }

    // Delete existing chunks for this atom (handles FTS, vec_chunks, and atom_chunks)
    storage
        .delete_chunks_batch_sync(&[atom_id.to_string()])
        .await
        .map_err(|e| e.to_string())?;

    // Chunk content
    let chunks = chunk_content(content);

    if chunks.is_empty() {
        // No chunks to process, mark embedding as complete, tagging as skipped
        storage
            .set_embedding_status_sync(atom_id, "complete", None)
            .await
            .map_err(|e| e.to_string())?;
        storage
            .set_tagging_status_sync(atom_id, "skipped", None)
            .await
            .map_err(|e| e.to_string())?;
        if !skip_edges {
            let atom_ids = [atom_id.to_string()];
            if let Err(e) = storage
                .set_edges_status_batch_sync(&atom_ids, "pending")
                .await
            {
                tracing::warn!(atom_id, error = %e, "Failed to mark graph maintenance pending");
            } else if let Err(e) = crate::graph_maintenance::mark_dirty(storage).await {
                tracing::warn!(atom_id, error = %e, "Failed to mark graph maintenance dirty");
            }
        }
        return Ok(());
    }

    // Use adaptive batching so provider batch-size limits (e.g. DashScope's
    // max 10) are handled by splitting, same as the bulk embedding path.
    let pending: Vec<PendingChunk> = chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| PendingChunk {
            atom_id: atom_id.to_string(),
            existing_chunk_id: None,
            chunk_index: index,
            content: chunk,
        })
        .collect();

    let (embedded, failed) = embed_chunks_batched(&provider_config, pending).await;

    if !failed.is_empty() {
        let error = failed
            .into_iter()
            .next()
            .map(|(_, e)| e)
            .unwrap_or_default();
        return Err(format!("Failed to generate embeddings: {}", error));
    }

    // Store chunks and embeddings
    let chunks_with_embeddings: Vec<(String, Vec<f32>)> = embedded
        .into_iter()
        .map(|(chunk, emb)| (chunk.content, emb))
        .collect();
    storage
        .save_chunks_and_embeddings_sync(atom_id, &chunks_with_embeddings)
        .await
        .map_err(|e| format!("Failed to store chunks: {}", e))?;

    // Set embedding status to complete
    storage
        .set_embedding_status_sync(atom_id, "complete", None)
        .await
        .map_err(|e| e.to_string())?;

    if !skip_edges {
        let atom_ids = [atom_id.to_string()];
        if let Err(e) = storage
            .set_edges_status_batch_sync(&atom_ids, "pending")
            .await
        {
            tracing::warn!(atom_id, error = %e, "Failed to mark graph maintenance pending");
        } else if let Err(e) = crate::graph_maintenance::mark_dirty(storage).await {
            tracing::warn!(atom_id, error = %e, "Failed to mark graph maintenance dirty");
        }
    }

    Ok(())
}

/// Process tag extraction for an atom using the configured tagging strategy.
/// The current default strategy is cost-bounded full-content tagging with
/// truncation. Future strategies can consume already-persisted chunks from the
/// embedding stage for chunk-assisted tagging.
///
/// Steps:
/// 1. Set tagging_status to 'processing'
/// 2. Check auto_tagging_enabled (skip if disabled)
/// 3. Read raw content from atoms table
/// 4. Extract tags via the configured strategy
/// 5. Link extracted tags to the atom
/// 6. Set tagging_status to 'complete'
pub async fn process_tagging_only(
    storage: &StorageBackend,
    atom_id: &str,
) -> Result<TaggingOutcome, String> {
    process_tagging_only_inner(storage, atom_id, None).await
}

/// Process tagging with externally-provided settings (from registry).
pub async fn process_tagging_only_with_settings(
    storage: &StorageBackend,
    atom_id: &str,
    settings_map: HashMap<String, String>,
) -> Result<TaggingOutcome, String> {
    process_tagging_only_inner(storage, atom_id, Some(settings_map)).await
}

async fn process_tagging_only_inner(
    storage: &StorageBackend,
    atom_id: &str,
    external_settings: Option<HashMap<String, String>>,
) -> Result<TaggingOutcome, String> {
    // Respect atoms that were intentionally marked 'skipped' (e.g. by a
    // dimension-change reset that preserves existing tags) or already
    // 'complete'. Only 'pending'/'failed' atoms should actually run tagging.
    let current_status = storage
        .get_tagging_status_impl(atom_id)
        .await
        .map_err(|e| e.to_string())?;
    if current_status == "skipped" || current_status == "complete" {
        return Ok(if current_status == "skipped" {
            TaggingOutcome::Skipped
        } else {
            TaggingOutcome::Complete {
                tags_extracted: Vec::new(),
                new_tags_created: Vec::new(),
            }
        });
    }

    // Set tagging status to processing
    storage
        .set_tagging_status_sync(atom_id, "processing", None)
        .await
        .map_err(|e| e.to_string())?;

    // Get settings (from the caller's resolved map if provided, otherwise
    // the global tier — tagging config is deployment-wide)
    let settings_map = match external_settings {
        Some(ref s) => s.clone(),
        None => storage
            .get_global_settings_sync()
            .await
            .map_err(|e| e.to_string())?,
    };
    let auto_tagging_enabled = settings_map
        .get("auto_tagging_enabled")
        .map(|v| v == "true")
        .unwrap_or(true);

    if !auto_tagging_enabled {
        tracing::info!(atom_id = %atom_id, "Auto-tagging disabled in settings; marking atom as skipped");
        storage
            .set_tagging_status_sync(atom_id, "skipped", None)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(TaggingOutcome::Skipped);
    }

    let provider_config = ProviderConfig::from_settings(&settings_map);
    let tagging_strategy = TaggingStrategy::from_settings(&settings_map);

    // Validate provider for LLM
    if provider_config.provider_type == ProviderType::OpenRouter
        && provider_config.openrouter_api_key.is_none()
    {
        tracing::warn!(atom_id = %atom_id, "OpenRouter selected but no API key configured; skipping tagging");
        storage
            .set_tagging_status_sync(atom_id, "skipped", None)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(TaggingOutcome::Skipped);
    }

    let tagging_model = provider_config.llm_model().to_string();
    tracing::info!(
        atom_id = %atom_id,
        provider = ?provider_config.provider_type,
        model = %tagging_model,
        "Starting auto-tagging"
    );

    // Read raw content directly from atoms table — no dependency on embedding
    let content = storage
        .get_atom_content_impl(atom_id)
        .await
        .map_err(|e| format!("Failed to get atom content: {}", e))?
        .ok_or_else(|| format!("Atom not found: {}", atom_id))?;

    if content.trim().is_empty() {
        tracing::info!(atom_id = %atom_id, "Auto-tagging skipped because atom content is empty");
        storage
            .set_tagging_status_sync(atom_id, "skipped", None)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(TaggingOutcome::Skipped);
    }

    // Load model capabilities (uses in-memory + DB cache to avoid redundant fetches)
    let supported_params: Option<Vec<String>> =
        if provider_config.provider_type == ProviderType::OpenRouter {
            // Try to load capabilities from the settings cache (global
            // tier — the cache is derived from the global provider config)
            let cached_json = storage
                .get_global_setting_sync("model_capabilities_cache")
                .await
                .ok()
                .flatten();
            let capabilities = if let Some(json) = cached_json {
                serde_json::from_str::<crate::providers::models::ModelCapabilitiesCache>(&json).ok()
            } else {
                None
            };

            capabilities.and_then(|caps| caps.get_supported_params(&tagging_model).cloned())
        } else {
            None
        };

    // Get tag tree for LLM context (only top-level tags flagged as auto-tag targets)
    let tag_tree_json = storage
        .get_tag_tree_for_llm_impl()
        .await
        .map_err(|e| e.to_string())?;

    // No auto-tag targets configured — skip tagging entirely.
    // The user has either unflagged all defaults during onboarding or hasn't created any.
    if tag_tree_json == "(no existing tags)" {
        tracing::info!(atom_id = %atom_id, "No tags flagged as auto-tag targets; skipping tagging (check Settings → Tagging)");
        storage
            .set_tagging_status_sync(atom_id, "skipped", None)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(TaggingOutcome::Skipped);
    }

    let custom_tagging_prompt = settings_map
        .get("tagging_prompt")
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str());
    let tags = run_tagging_strategy(
        tagging_strategy,
        &provider_config,
        &content,
        &tag_tree_json,
        &tagging_model,
        supported_params,
        custom_tagging_prompt,
    )
    .await?;

    let mut all_tag_ids = Vec::new();

    for tag_application in tags {
        let trimmed_name = tag_application.name.trim();
        if trimmed_name.is_empty() || trimmed_name.eq_ignore_ascii_case("null") {
            continue;
        }

        match storage
            .get_or_create_tag_impl(
                &tag_application.name,
                tag_application.parent_name.as_deref(),
            )
            .await
        {
            Ok(tag_id) => all_tag_ids.push(tag_id),
            Err(e) => {
                tracing::error!(tag_name = %tag_application.name, error = %e, "Failed to get/create tag")
            }
        }
    }

    if !all_tag_ids.is_empty() {
        storage
            .link_tags_to_atom_impl(atom_id, &all_tag_ids)
            .await
            .map_err(|e| e.to_string())?;
    }

    // Set tagging status to complete
    storage
        .set_tagging_status_sync(atom_id, "complete", None)
        .await
        .map_err(|e| e.to_string())?;

    all_tag_ids.sort();
    all_tag_ids.dedup();
    let all_new_tag_ids = all_tag_ids.clone();
    tracing::info!(
        atom_id = %atom_id,
        tags_applied = all_tag_ids.len(),
        new_tags_created = all_new_tag_ids.len(),
        "Auto-tagging complete"
    );

    Ok(TaggingOutcome::Complete {
        tags_extracted: all_tag_ids,
        new_tags_created: all_new_tag_ids,
    })
}
async fn run_tagging_strategy(
    strategy: TaggingStrategy,
    provider_config: &ProviderConfig,
    content: &str,
    tag_tree_json: &str,
    model: &str,
    supported_params: Option<Vec<String>>,
    custom_system_prompt: Option<&str>,
) -> Result<Vec<crate::extraction::TagApplication>, String> {
    match strategy {
        TaggingStrategy::TruncatedFullContent => {
            extract_tags_from_content(
                provider_config,
                content,
                tag_tree_json,
                model,
                supported_params,
                custom_system_prompt,
            )
            .await
        }
        TaggingStrategy::ChunkAssisted => {
            tracing::warn!(
                "chunk-assisted tagging requested but not implemented; falling back to truncated full-content tagging"
            );
            extract_tags_from_content(
                provider_config,
                content,
                tag_tree_json,
                model,
                supported_params,
                custom_system_prompt,
            )
            .await
        }
    }
}

/// Process tagging for multiple atoms concurrently with semaphore-based limiting
/// Used by process_pending_tagging for bulk operations
pub async fn process_tagging_batch<F>(storage: StorageBackend, atom_ids: Vec<String>, on_event: F)
where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    process_tagging_batch_inner(storage, atom_ids, on_event, None).await
}

/// Process tagging batch with externally-provided settings (from registry).
pub async fn process_tagging_batch_with_settings<F>(
    storage: StorageBackend,
    atom_ids: Vec<String>,
    on_event: F,
    settings_map: HashMap<String, String>,
) where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    process_tagging_batch_inner(storage, atom_ids, on_event, Some(settings_map)).await
}

async fn process_tagging_batch_inner<F>(
    storage: StorageBackend,
    atom_ids: Vec<String>,
    on_event: F,
    external_settings: Option<HashMap<String, String>>,
) where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    let total = atom_ids.len();
    let emit_progress = total > 1;
    let batch_id = Uuid::new_v4().to_string();
    let counter = Arc::new(AtomicUsize::new(0));

    if emit_progress {
        on_event(EmbeddingEvent::BatchProgress {
            batch_id: batch_id.clone(),
            phase: "tagging".to_string(),
            completed: 0,
            total,
        });
    }

    let mut tasks = Vec::with_capacity(total);

    for atom_id in atom_ids {
        let storage = storage.clone();
        let on_event = on_event.clone();
        let settings = external_settings.clone();
        let counter = counter.clone();
        let batch_id = batch_id.clone();

        let task = tokio::spawn(async move {
            // Acquire semaphore permit
            let _permit = crate::executor::LLM_SEMAPHORE
                .acquire()
                .await
                .expect("Semaphore closed unexpectedly");

            let result = match settings {
                Some(s) => process_tagging_only_with_settings(&storage, &atom_id, s).await,
                None => process_tagging_only(&storage, &atom_id).await,
            };

            let event = match result {
                Ok(outcome) => outcome.into_event(atom_id.clone()),
                Err(e) => {
                    storage
                        .set_tagging_status_sync(&atom_id, "failed", Some(&e))
                        .await
                        .ok();
                    EmbeddingEvent::TaggingFailed {
                        atom_id: atom_id.clone(),
                        error: e,
                    }
                }
            };

            on_event(event);

            if emit_progress {
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                if done % 5 == 0 || done == total {
                    on_event(EmbeddingEvent::BatchProgress {
                        batch_id: batch_id.clone(),
                        phase: "tagging".to_string(),
                        completed: done,
                        total,
                    });
                }
            }
        });

        tasks.push(task);
    }

    // Wait for all tasks to complete
    for task in tasks {
        let _ = task.await;
    }

    if emit_progress {
        on_event(EmbeddingEvent::BatchProgress {
            batch_id,
            phase: "complete".to_string(),
            completed: total,
            total,
        });
    }
}

/// Process embeddings and tagging for a SINGLE atom (used by create_atom/update_atom).
/// Spawns a background task that runs tagging only after embedding succeeds, so
/// `tagging_status = complete` implies chunks and embeddings are already present.
pub fn spawn_embedding_task_single<F>(
    storage: StorageBackend,
    atom_id: String,
    content: String,
    on_event: F,
) where
    F: Fn(EmbeddingEvent) + Send + Sync + 'static,
{
    spawn_embedding_task_single_with_settings(storage, atom_id, content, on_event, None);
}

/// Like `spawn_embedding_task_single` but with externally-provided settings (from registry).
pub fn spawn_embedding_task_single_with_settings<F>(
    storage: StorageBackend,
    atom_id: String,
    content: String,
    on_event: F,
    settings_map: Option<HashMap<String, String>>,
) where
    F: Fn(EmbeddingEvent) + Send + Sync + 'static,
{
    let on_event = Arc::new(on_event);
    crate::executor::spawn(async move {
        on_event(EmbeddingEvent::Started {
            atom_id: atom_id.clone(),
        });

        let embedding_result = match settings_map.clone() {
            Some(s) => process_embedding_only_with_settings(&storage, &atom_id, &content, s).await,
            None => process_embedding_only(&storage, &atom_id, &content).await,
        };

        if let Err(e) = embedding_result {
            storage
                .set_embedding_status_sync(&atom_id, "failed", Some(&e))
                .await
                .ok();
            on_event(EmbeddingEvent::EmbeddingFailed { atom_id, error: e });
            return;
        }

        on_event(EmbeddingEvent::EmbeddingComplete {
            atom_id: atom_id.clone(),
        });

        let _permit = crate::executor::LLM_SEMAPHORE
            .acquire()
            .await
            .expect("Semaphore closed unexpectedly");

        let tagging_result = match settings_map {
            Some(s) => process_tagging_only_with_settings(&storage, &atom_id, s).await,
            None => process_tagging_only(&storage, &atom_id).await,
        };

        match tagging_result {
            Ok(outcome) => on_event(outcome.into_event(atom_id)),
            Err(e) => {
                storage
                    .set_tagging_status_sync(&atom_id, "failed", Some(&e))
                    .await
                    .ok();
                on_event(EmbeddingEvent::TaggingFailed { atom_id, error: e });
            }
        }
    });
}

/// Process embeddings and tagging for multiple atoms concurrently.
/// Uses cross-atom batching for embedding API calls (reducing 10K calls to ~200).
/// Tagging runs per-atom concurrently via semaphores.
/// Set skip_tagging=true when re-embedding due to model/provider change (tags are preserved).
pub async fn process_embedding_batch<F>(
    storage: StorageBackend,
    input: AtomInput,
    skip_tagging: bool,
    on_event: F,
    canvas_cache: Option<CanvasCache>,
) where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    let tagging_policy = if skip_tagging {
        TaggingPolicy::None
    } else {
        TaggingPolicy::All
    };
    process_embedding_batch_inner(storage, input, tagging_policy, on_event, None, canvas_cache)
        .await;
}

/// Process embedding batch with externally-provided settings (from registry).
pub async fn process_embedding_batch_with_settings<F>(
    storage: StorageBackend,
    input: AtomInput,
    skip_tagging: bool,
    on_event: F,
    settings_map: HashMap<String, String>,
    canvas_cache: Option<CanvasCache>,
) where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    let tagging_policy = if skip_tagging {
        TaggingPolicy::None
    } else {
        TaggingPolicy::All
    };
    process_embedding_batch_inner(
        storage,
        input,
        tagging_policy,
        on_event,
        Some(settings_map),
        canvas_cache,
    )
    .await;
}

async fn process_embedding_batch_inner<F>(
    storage: StorageBackend,
    input: AtomInput,
    tagging_policy: TaggingPolicy,
    on_event: F,
    external_settings: Option<HashMap<String, String>>,
    _canvas_cache: Option<CanvasCache>,
) -> Vec<String>
where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    // Extract all atom IDs and optional preloaded content
    let (all_atom_ids, preloaded_content): (Vec<String>, Option<Vec<(String, String)>>) =
        match input {
            AtomInput::Preloaded(atoms) => {
                let ids = atoms.iter().map(|(id, _)| id.clone()).collect();
                (ids, Some(atoms))
            }
            AtomInput::IdsOnly(ids) => (ids, None),
        };

    let total_count = all_atom_ids.len();
    if total_count == 0 {
        return Vec::new();
    }

    let batch_id = Uuid::new_v4().to_string();

    // Only emit batch progress for bulk operations (>1 atom)
    let emit_progress = total_count > 1;
    if emit_progress {
        on_event(EmbeddingEvent::BatchProgress {
            batch_id: batch_id.clone(),
            phase: "chunking".to_string(),
            completed: 0,
            total: total_count,
        });
    }

    tracing::info!(
        total_count,
        fetch_batch_size = ATOM_FETCH_BATCH_SIZE,
        chunk_target = EMBEDDING_GROUP_CHUNK_TARGET,
        "Starting pipeline for atoms"
    );

    // === Get settings (global tier — provider config is deployment-wide) ===
    let provider_config = {
        let settings_map = match external_settings {
            Some(ref s) => s.clone(),
            None => match storage.get_global_settings_sync().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to get settings");
                    return Vec::new();
                }
            },
        };
        let provider_config = ProviderConfig::from_settings(&settings_map);

        if provider_config.provider_type == ProviderType::OpenRouter
            && provider_config.openrouter_api_key.is_none()
        {
            tracing::warn!("OpenRouter API key not configured, skipping embedding");
            return Vec::new();
        }

        provider_config
    };

    // === Clean up old chunks for all atoms ===
    if let Err(e) = storage.delete_chunks_batch_sync(&all_atom_ids).await {
        tracing::error!(error = %e, "Failed to clean up old chunks");
        return Vec::new();
    }
    tracing::info!(total_count, "DB cleanup complete for atoms");

    // === Shared tagging state (fire-and-forget with atomic counter) ===
    let tagging_counter = Arc::new(AtomicUsize::new(0));
    let tagging_remaining = Arc::new(AtomicUsize::new(0));
    let tagging_done_notify = Arc::new(tokio::sync::Notify::new());

    let spawn_tagging_tasks = |atom_ids: Vec<String>| {
        let atom_ids: Vec<String> = atom_ids
            .into_iter()
            .filter(|atom_id| tagging_policy.should_tag(atom_id))
            .collect();

        if tagging_policy.is_none() || atom_ids.is_empty() {
            return;
        }

        tagging_remaining.fetch_add(atom_ids.len(), Ordering::Relaxed);
        for atom_id in atom_ids {
            let storage = storage.clone();
            let on_event = on_event.clone();
            let settings = external_settings.clone();
            let counter = tagging_counter.clone();
            let remaining = tagging_remaining.clone();
            let notify = tagging_done_notify.clone();
            let batch_id = batch_id.clone();
            let should_emit = emit_progress;
            let tagging_total = total_count;

            tokio::spawn(async move {
                let _permit = crate::executor::LLM_SEMAPHORE
                    .acquire()
                    .await
                    .expect("Semaphore closed unexpectedly");

                let result = match settings {
                    Some(s) => process_tagging_only_with_settings(&storage, &atom_id, s).await,
                    None => process_tagging_only(&storage, &atom_id).await,
                };

                let event = match result {
                    Ok(outcome) => outcome.into_event(atom_id.clone()),
                    Err(e) => {
                        storage
                            .set_tagging_status_sync(&atom_id, "failed", Some(&e))
                            .await
                            .ok();
                        EmbeddingEvent::TaggingFailed {
                            atom_id: atom_id.clone(),
                            error: e,
                        }
                    }
                };

                on_event(event);

                // Emit tagging progress every 5 atoms.
                if should_emit {
                    let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    if done % 5 == 0 || done == tagging_total {
                        on_event(EmbeddingEvent::BatchProgress {
                            batch_id: batch_id.clone(),
                            phase: "tagging".to_string(),
                            completed: done,
                            total: tagging_total,
                        });
                    }
                }

                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    notify.notify_one();
                }
            });
        }
    };

    // === Process atoms in bounded groups ===
    let mut completed_atom_ids: Vec<String> = Vec::new();
    let mut atoms_processed = 0usize;

    // Build an iterator of (id, content) pairs per fetch batch. Each fetch
    // batch is then split into smaller embedding groups by chunk count so
    // large atoms do not delay completion events for an entire atom batch.
    let num_groups = (total_count + ATOM_FETCH_BATCH_SIZE - 1) / ATOM_FETCH_BATCH_SIZE;

    // Consume preloaded content into an owned iterator we can drain per-group
    let mut preloaded_iter = preloaded_content.map(|v| v.into_iter());

    for group_idx in 0..num_groups {
        let group_start = group_idx * ATOM_FETCH_BATCH_SIZE;
        let group_end = (group_start + ATOM_FETCH_BATCH_SIZE).min(total_count);
        let group_size = group_end - group_start;

        // Get (id, content) pairs for this group
        let group_atoms: Vec<(String, String)> = if let Some(ref mut iter) = preloaded_iter {
            // Preloaded: take the next group_size items (drains from the Vec)
            iter.by_ref().take(group_size).collect()
        } else {
            // IdsOnly: load content from DB for this group
            let group_ids = &all_atom_ids[group_start..group_end];
            match storage.get_atom_contents_batch_impl(group_ids).await {
                Ok(pairs) => pairs,
                Err(e) => {
                    tracing::error!(error = %e, group = group_idx + 1, "Failed to load atom content for group");
                    // Mark all atoms in this group as failed
                    for id in group_ids {
                        storage
                            .set_embedding_status_sync(id, "failed", Some(&e.to_string()))
                            .await
                            .ok();
                        on_event(EmbeddingEvent::EmbeddingFailed {
                            atom_id: id.clone(),
                            error: format!("Failed to load content: {}", e),
                        });
                    }
                    atoms_processed += group_size;
                    continue;
                }
            }
        };

        tracing::info!(
            group = group_idx + 1,
            num_groups,
            group_atoms = group_atoms.len(),
            chunk_target = EMBEDDING_GROUP_CHUNK_TARGET,
            "Processing atom fetch group"
        );

        // --- Chunk this fetch group into bounded embedding groups ---
        let mut chunk_groups: Vec<Vec<(String, Vec<PendingChunk>)>> = Vec::new();
        let mut current_chunk_group: Vec<(String, Vec<PendingChunk>)> = Vec::new();
        let mut current_chunk_count = 0usize;
        let mut empty_atom_tagging_ids = Vec::new();
        let mut empty_atom_graph_ids = Vec::new();

        for (atom_id, content) in &group_atoms {
            let chunks = chunk_content(content);
            if chunks.is_empty() {
                storage
                    .set_embedding_status_sync(atom_id, "complete", None)
                    .await
                    .ok();
                storage
                    .set_tagging_status_sync(atom_id, "skipped", None)
                    .await
                    .ok();
                on_event(EmbeddingEvent::EmbeddingComplete {
                    atom_id: atom_id.clone(),
                });
                completed_atom_ids.push(atom_id.clone());
                empty_atom_tagging_ids.push(atom_id.clone());
                empty_atom_graph_ids.push(atom_id.clone());
                atoms_processed += 1;
                continue;
            }

            let atom_chunks: Vec<PendingChunk> = chunks
                .into_iter()
                .enumerate()
                .map(|(index, chunk)| PendingChunk {
                    atom_id: atom_id.clone(),
                    existing_chunk_id: None,
                    chunk_index: index,
                    content: chunk,
                })
                .collect();
            let atom_chunk_count = atom_chunks.len();

            if !current_chunk_group.is_empty()
                && current_chunk_count + atom_chunk_count > EMBEDDING_GROUP_CHUNK_TARGET
            {
                chunk_groups.push(std::mem::take(&mut current_chunk_group));
                current_chunk_count = 0;
            }

            current_chunk_count += atom_chunk_count;
            current_chunk_group.push((atom_id.clone(), atom_chunks));
        }

        if !current_chunk_group.is_empty() {
            chunk_groups.push(current_chunk_group);
        }

        if emit_progress && !empty_atom_tagging_ids.is_empty() {
            on_event(EmbeddingEvent::BatchProgress {
                batch_id: batch_id.clone(),
                phase: "embedding".to_string(),
                completed: atoms_processed,
                total: total_count,
            });
        }
        spawn_tagging_tasks(empty_atom_tagging_ids);

        if !empty_atom_graph_ids.is_empty() {
            if let Err(e) = storage
                .set_edges_status_batch_sync(&empty_atom_graph_ids, "pending")
                .await
            {
                tracing::warn!(error = %e, "Failed to mark empty atoms for graph maintenance");
            } else if let Err(e) = crate::graph_maintenance::mark_dirty(&storage).await {
                tracing::warn!(error = %e, "Failed to mark graph maintenance dirty");
            }
        }

        for (chunk_group_idx, chunk_group) in chunk_groups.into_iter().enumerate() {
            let chunk_group_atom_count = chunk_group.len();
            let chunk_count: usize = chunk_group.iter().map(|(_, chunks)| chunks.len()).sum();
            tracing::info!(
                fetch_group = group_idx + 1,
                chunk_group = chunk_group_idx + 1,
                atoms = chunk_group_atom_count,
                chunks = chunk_count,
                "Processing embedding chunk group"
            );

            if emit_progress {
                on_event(EmbeddingEvent::BatchProgress {
                    batch_id: batch_id.clone(),
                    phase: "embedding".to_string(),
                    completed: atoms_processed,
                    total: total_count,
                });
            }

            let mut group_chunks: Vec<PendingChunk> = Vec::with_capacity(chunk_count);
            for (_, chunks) in chunk_group {
                group_chunks.extend(chunks);
            }
            let mut group_tagging_atom_ids: Vec<String> = Vec::new();
            let mut group_completed_atom_ids: Vec<String> = Vec::new();

            let (embedded_chunks, failed_atoms) =
                embed_chunks_batched(&provider_config, group_chunks).await;

            // --- Store results in a single transaction ---
            // Group by atom_id, consuming the embedded results
            let mut by_atom: HashMap<String, Vec<(String, Vec<f32>)>> = HashMap::new();
            for (chunk, embedding) in embedded_chunks {
                by_atom
                    .entry(chunk.atom_id)
                    .or_default()
                    .push((chunk.content, embedding));
            }

            // Batch save: one lock acquire, one transaction, one fsync
            let atoms_vec: Vec<(String, Vec<(String, Vec<f32>)>)> = by_atom.into_iter().collect();
            match storage
                .save_chunks_and_embeddings_batch_sync(&atoms_vec)
                .await
            {
                Ok(succeeded) => {
                    // Batch-set status for all succeeded atoms
                    storage
                        .set_embedding_status_batch_sync(&succeeded, "complete", None)
                        .await
                        .ok();
                    for atom_id in &succeeded {
                        on_event(EmbeddingEvent::EmbeddingComplete {
                            atom_id: atom_id.clone(),
                        });
                    }
                    group_tagging_atom_ids.extend(succeeded.iter().cloned());
                    group_completed_atom_ids.extend(succeeded.iter().cloned());
                    // Track atoms that saved OK but whose chunks failed
                    let succeeded_set: std::collections::HashSet<&String> =
                        succeeded.iter().collect();
                    let mut db_failed: Vec<String> = Vec::new();
                    for (atom_id, _) in &atoms_vec {
                        if !succeeded_set.contains(atom_id) {
                            db_failed.push(atom_id.clone());
                        }
                    }
                    if !db_failed.is_empty() {
                        storage
                            .set_embedding_status_batch_sync(
                                &db_failed,
                                "failed",
                                Some("Failed to store embeddings in DB"),
                            )
                            .await
                            .ok();
                        for atom_id in &db_failed {
                            on_event(EmbeddingEvent::EmbeddingFailed {
                                atom_id: atom_id.clone(),
                                error: "Failed to store embeddings in DB".to_string(),
                            });
                        }
                    }
                    completed_atom_ids.extend(succeeded);
                }
                Err(e) => {
                    // Entire batch transaction failed — fall back to per-atom
                    tracing::warn!(error = %e, "Batch save failed, falling back to per-atom");
                    for (atom_id, chunks_with_embeddings) in &atoms_vec {
                        match storage
                            .save_chunks_and_embeddings_sync(atom_id, chunks_with_embeddings)
                            .await
                        {
                            Ok(()) => {
                                storage
                                    .set_embedding_status_sync(atom_id, "complete", None)
                                    .await
                                    .ok();
                                completed_atom_ids.push(atom_id.clone());
                                group_completed_atom_ids.push(atom_id.clone());
                                group_tagging_atom_ids.push(atom_id.clone());
                                on_event(EmbeddingEvent::EmbeddingComplete {
                                    atom_id: atom_id.clone(),
                                });
                            }
                            Err(_) => {
                                storage
                                    .set_embedding_status_sync(
                                        atom_id,
                                        "failed",
                                        Some("Failed to store embeddings in DB"),
                                    )
                                    .await
                                    .ok();
                                on_event(EmbeddingEvent::EmbeddingFailed {
                                    atom_id: atom_id.clone(),
                                    error: "Failed to store embeddings in DB".to_string(),
                                });
                            }
                        }
                    }
                }
            }

            // Mark atoms that failed embedding API calls
            if !failed_atoms.is_empty() {
                let failed_ids: Vec<String> =
                    failed_atoms.iter().map(|(id, _)| id.clone()).collect();
                // Each atom may have a different error, but for batch status we use a generic message
                // and emit per-atom events with the specific error
                storage
                    .set_embedding_status_batch_sync(
                        &failed_ids,
                        "failed",
                        Some("Embedding API error"),
                    )
                    .await
                    .ok();
                for (atom_id, error) in &failed_atoms {
                    on_event(EmbeddingEvent::EmbeddingFailed {
                        atom_id: atom_id.clone(),
                        error: error.clone(),
                    });
                }
            }

            if emit_progress {
                on_event(EmbeddingEvent::BatchProgress {
                    batch_id: batch_id.clone(),
                    phase: "storing".to_string(),
                    completed: completed_atom_ids.len(),
                    total: total_count,
                });
            }

            atoms_processed += chunk_group_atom_count;
            if emit_progress {
                on_event(EmbeddingEvent::BatchProgress {
                    batch_id: batch_id.clone(),
                    phase: "embedding".to_string(),
                    completed: atoms_processed,
                    total: total_count,
                });
            }

            // Graph maintenance depends on stored embeddings but should not run
            // per chunk group; deferring it keeps bulk imports and single edits
            // on the same stable clustering path.
            if !group_completed_atom_ids.is_empty() {
                if let Err(e) = storage
                    .set_edges_status_batch_sync(&group_completed_atom_ids, "pending")
                    .await
                {
                    tracing::warn!(error = %e, "Failed to mark group atoms for edge computation");
                } else if let Err(e) = crate::graph_maintenance::mark_dirty(&storage).await {
                    tracing::warn!(error = %e, "Failed to mark graph maintenance dirty");
                }
            }

            // --- Spawn tagging tasks for this chunk group (fire-and-forget) ---
            spawn_tagging_tasks(group_tagging_atom_ids);
        }

        // group_atoms, chunk_groups, embedded_chunks, by_atom are all dropped here
    }

    tracing::info!(
        succeeded = completed_atom_ids.len(),
        total = total_count,
        "All groups processed, embeddings stored"
    );

    // Rebuild FTS index once after all groups
    storage.rebuild_fts_index_sync().await.ok();

    if emit_progress {
        on_event(EmbeddingEvent::BatchProgress {
            batch_id: batch_id.clone(),
            phase: "finalizing".to_string(),
            completed: total_count,
            total: total_count,
        });
    }

    // === Wait for tagging to complete ===
    while !tagging_policy.is_none() && tagging_remaining.load(Ordering::Acquire) > 0 {
        tagging_done_notify.notified().await;
    }

    if emit_progress {
        on_event(EmbeddingEvent::BatchProgress {
            batch_id: batch_id.clone(),
            phase: "complete".to_string(),
            completed: total_count,
            total: total_count,
        });
    }

    if tagging_policy.is_none() {
        tracing::info!("Pipeline complete. Tagging was skipped (re-embedding only).");
    } else {
        tracing::info!("Pipeline complete. All embedding and tagging tasks finished.");
    }

    completed_atom_ids
}

async fn process_existing_chunk_reembedding_batch_inner<F>(
    storage: StorageBackend,
    atom_ids: Vec<String>,
    on_event: F,
    external_settings: Option<HashMap<String, String>>,
    canvas_cache: Option<CanvasCache>,
) -> Vec<String>
where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    if atom_ids.is_empty() {
        return Vec::new();
    }

    let settings_map = match external_settings.clone() {
        Some(s) => s,
        None => match storage.get_global_settings_sync().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "Failed to get settings for chunk-preserving re-embed");
                return Vec::new();
            }
        },
    };
    let provider_config = ProviderConfig::from_settings(&settings_map);
    if provider_config.provider_type == ProviderType::OpenRouter
        && provider_config.openrouter_api_key.is_none()
    {
        tracing::warn!("OpenRouter API key not configured, skipping re-embedding");
        return Vec::new();
    }

    let existing_chunks = match storage.get_chunks_for_atoms_sync(&atom_ids).await {
        Ok(chunks) => chunks,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to load existing chunks for re-embedding");
            return process_embedding_batch_inner(
                storage,
                AtomInput::IdsOnly(atom_ids),
                TaggingPolicy::None,
                on_event,
                external_settings,
                canvas_cache,
            )
            .await;
        }
    };

    let mut chunks_by_atom: HashMap<String, Vec<PendingChunk>> = HashMap::new();
    for chunk in existing_chunks {
        chunks_by_atom
            .entry(chunk.atom_id.clone())
            .or_default()
            .push(PendingChunk {
                atom_id: chunk.atom_id,
                existing_chunk_id: Some(chunk.id),
                chunk_index: chunk.chunk_index.max(0) as usize,
                content: chunk.content,
            });
    }

    let atom_id_set: HashSet<String> = chunks_by_atom.keys().cloned().collect();
    let fallback_ids: Vec<String> = atom_ids
        .iter()
        .filter(|atom_id| !atom_id_set.contains(*atom_id))
        .cloned()
        .collect();

    let mut atom_groups: Vec<(String, Vec<PendingChunk>)> = chunks_by_atom.into_iter().collect();
    atom_groups.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, chunks) in &mut atom_groups {
        chunks.sort_by_key(|chunk| chunk.chunk_index);
    }

    let mut chunk_groups: Vec<Vec<(String, Vec<PendingChunk>)>> = Vec::new();
    let mut current_group = Vec::new();
    let mut current_chunk_count = 0usize;
    for (atom_id, chunks) in atom_groups {
        let atom_chunk_count = chunks.len();
        if !current_group.is_empty()
            && current_chunk_count + atom_chunk_count > EMBEDDING_GROUP_CHUNK_TARGET
        {
            chunk_groups.push(std::mem::take(&mut current_group));
            current_chunk_count = 0;
        }
        current_chunk_count += atom_chunk_count;
        current_group.push((atom_id, chunks));
    }
    if !current_group.is_empty() {
        chunk_groups.push(current_group);
    }

    let mut completed_atom_ids = Vec::new();
    for (group_idx, chunk_group) in chunk_groups.into_iter().enumerate() {
        let chunk_count: usize = chunk_group.iter().map(|(_, chunks)| chunks.len()).sum();
        tracing::info!(
            group = group_idx + 1,
            chunks = chunk_count,
            atoms = chunk_group.len(),
            "Re-embedding existing chunk group"
        );

        let original_counts: HashMap<String, usize> = chunk_group
            .iter()
            .map(|(atom_id, chunks)| (atom_id.clone(), chunks.len()))
            .collect();
        let mut group_chunks = Vec::with_capacity(chunk_count);
        for (_, chunks) in chunk_group {
            group_chunks.extend(chunks);
        }

        let (embedded_chunks, failed_atoms) =
            embed_chunks_batched(&provider_config, group_chunks).await;
        let failed_atom_set: HashSet<String> = failed_atoms
            .iter()
            .map(|(atom_id, _)| atom_id.clone())
            .collect();

        let mut updates = Vec::new();
        let mut embedded_counts: HashMap<String, usize> = HashMap::new();
        for (chunk, embedding) in embedded_chunks {
            if failed_atom_set.contains(&chunk.atom_id) {
                continue;
            }
            let Some(chunk_id) = chunk.existing_chunk_id else {
                continue;
            };
            *embedded_counts.entry(chunk.atom_id).or_default() += 1;
            updates.push((chunk_id, embedding));
        }

        if let Err(e) = storage.update_chunk_embeddings_sync(&updates).await {
            tracing::warn!(error = %e, "Failed to update existing chunk embeddings");
            continue;
        }

        let succeeded: Vec<String> = original_counts
            .into_iter()
            .filter_map(|(atom_id, expected)| {
                (embedded_counts.get(&atom_id).copied().unwrap_or(0) == expected).then_some(atom_id)
            })
            .collect();

        if !succeeded.is_empty() {
            storage
                .set_embedding_status_batch_sync(&succeeded, "complete", None)
                .await
                .ok();
            storage
                .set_edges_status_batch_sync(&succeeded, "pending")
                .await
                .ok();
            if let Err(e) = crate::graph_maintenance::mark_dirty(&storage).await {
                tracing::warn!(error = %e, "Failed to mark graph maintenance dirty");
            }
            for atom_id in &succeeded {
                on_event(EmbeddingEvent::EmbeddingComplete {
                    atom_id: atom_id.clone(),
                });
            }
            completed_atom_ids.extend(succeeded);
        }

        if !failed_atoms.is_empty() {
            let failed_ids: Vec<String> = failed_atoms
                .iter()
                .map(|(atom_id, _)| atom_id.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            storage
                .set_embedding_status_batch_sync(&failed_ids, "failed", Some("Embedding API error"))
                .await
                .ok();
            for (atom_id, error) in failed_atoms {
                on_event(EmbeddingEvent::EmbeddingFailed { atom_id, error });
            }
        }
    }

    if !fallback_ids.is_empty() {
        let fallback_completed = process_embedding_batch_inner(
            storage,
            AtomInput::IdsOnly(fallback_ids),
            TaggingPolicy::None,
            on_event,
            external_settings,
            canvas_cache,
        )
        .await;
        completed_atom_ids.extend(fallback_completed);
    }

    completed_atom_ids
}

async fn process_pipeline_jobs_batch<F>(
    storage: StorageBackend,
    jobs: Vec<crate::models::AtomPipelineJob>,
    on_event: F,
    external_settings: Option<HashMap<String, String>>,
    canvas_cache: Option<CanvasCache>,
    progress: Arc<QueueRunProgress>,
    embedding_total: usize,
) where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    if jobs.is_empty() {
        return;
    }

    let mut embed_only_ids = Vec::new();
    let mut embed_and_tag_ids = Vec::new();
    let mut tag_after_embed_ids = HashSet::new();
    let mut tag_only_ids = Vec::new();
    let claimed_jobs = jobs.clone();

    for job in jobs {
        if job.embed_requested {
            if job.tag_requested {
                embed_and_tag_ids.push(job.atom_id.clone());
                tag_after_embed_ids.insert(job.atom_id);
            } else {
                embed_only_ids.push(job.atom_id);
            }
        } else if job.tag_requested {
            tag_only_ids.push(job.atom_id);
        }
    }

    let mut tagging_ids = tag_only_ids;

    let process_embed_ids =
        |embed_ids: Vec<String>,
         preserve_existing_chunks: bool,
         tag_after_embed_ids: Arc<HashSet<String>>| {
            let storage = storage.clone();
            let on_event = on_event.clone();
            let external_settings = external_settings.clone();
            let canvas_cache = canvas_cache.clone();
            let progress = progress.clone();
            async move {
                if embed_ids.is_empty() {
                    return Vec::new();
                }

                if let Err(e) = storage
                    .set_embedding_status_batch_sync(&embed_ids, "processing", None)
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        count = embed_ids.len(),
                        "Failed to mark queued embedding jobs as processing"
                    );
                }

                let requested_embed_ids = embed_ids.clone();
                let terminal_embed_ids = Arc::new(std::sync::Mutex::new(HashSet::new()));
                let progress_on_event = {
                    let on_event = on_event.clone();
                    let progress = progress.clone();
                    let terminal_embed_ids = terminal_embed_ids.clone();
                    move |event: EmbeddingEvent| {
                        match &event {
                            EmbeddingEvent::EmbeddingComplete { atom_id } => {
                                if let Ok(mut ids) = terminal_embed_ids.lock() {
                                    ids.insert(atom_id.clone());
                                }
                                progress.record_embedding_done(embedding_total, &on_event);
                            }
                            EmbeddingEvent::EmbeddingFailed { atom_id, .. } => {
                                if let Ok(mut ids) = terminal_embed_ids.lock() {
                                    ids.insert(atom_id.clone());
                                }
                                progress.record_failed_job();
                                progress.record_embedding_done(embedding_total, &on_event);
                            }
                            _ => {}
                        }
                        on_event(event);
                    }
                };

                let completed_embed_ids = if preserve_existing_chunks {
                    process_existing_chunk_reembedding_batch_inner(
                        storage.clone(),
                        embed_ids,
                        progress_on_event,
                        external_settings.clone(),
                        canvas_cache.clone(),
                    )
                    .await
                } else {
                    process_embedding_batch_inner(
                        storage.clone(),
                        AtomInput::IdsOnly(embed_ids),
                        TaggingPolicy::None,
                        progress_on_event,
                        external_settings.clone(),
                        canvas_cache.clone(),
                    )
                    .await
                };

                for atom_id in &completed_embed_ids {
                    let already_terminal = terminal_embed_ids
                        .lock()
                        .map(|ids| ids.contains(atom_id))
                        .unwrap_or(false);
                    if !already_terminal {
                        if let Ok(mut ids) = terminal_embed_ids.lock() {
                            ids.insert(atom_id.clone());
                        }
                        progress.record_embedding_done(embedding_total, &on_event);
                        on_event(EmbeddingEvent::EmbeddingComplete {
                            atom_id: atom_id.clone(),
                        });
                    }
                }

                let terminal_snapshot = terminal_embed_ids
                    .lock()
                    .map(|ids| ids.clone())
                    .unwrap_or_default();
                let silently_failed_ids: Vec<String> = requested_embed_ids
                    .iter()
                    .filter(|atom_id| !terminal_snapshot.contains(*atom_id))
                    .cloned()
                    .collect();
                if !silently_failed_ids.is_empty() {
                    let error = "Embedding provider was not configured or returned no result";
                    storage
                        .set_embedding_status_batch_sync(
                            &silently_failed_ids,
                            "failed",
                            Some(error),
                        )
                        .await
                        .ok();
                    for atom_id in silently_failed_ids {
                        progress.record_failed_job();
                        progress.record_embedding_done(embedding_total, &on_event);
                        on_event(EmbeddingEvent::EmbeddingFailed {
                            atom_id,
                            error: error.to_string(),
                        });
                    }
                }

                completed_embed_ids
                    .into_iter()
                    .filter(|atom_id| tag_after_embed_ids.contains(atom_id))
                    .collect::<Vec<_>>()
            }
        };

    let no_tag_after_embed = Arc::new(HashSet::new());
    tagging_ids.extend(
        process_embed_ids(embed_only_ids, true, no_tag_after_embed)
            .await
            .into_iter(),
    );
    tagging_ids.extend(
        process_embed_ids(embed_and_tag_ids, false, Arc::new(tag_after_embed_ids))
            .await
            .into_iter(),
    );

    if !tagging_ids.is_empty() {
        progress.add_tagging_total(tagging_ids.len(), &on_event);
        let progress_on_event = {
            let on_event = on_event.clone();
            let progress = progress.clone();
            move |event: EmbeddingEvent| {
                match &event {
                    EmbeddingEvent::TaggingComplete { .. }
                    | EmbeddingEvent::TaggingSkipped { .. } => {
                        progress.record_tagging_done(&on_event);
                    }
                    EmbeddingEvent::TaggingFailed { .. } => {
                        progress.record_failed_job();
                        progress.record_tagging_done(&on_event);
                    }
                    _ => {}
                }
                on_event(event);
            }
        };
        process_tagging_batch_inner(
            storage.clone(),
            tagging_ids,
            progress_on_event,
            external_settings.clone(),
        )
        .await;
    }

    if let Err(e) = storage.clear_pipeline_jobs_sync(&claimed_jobs).await {
        tracing::warn!(error = %e, "Failed to clear processed pipeline jobs");
    }
}

/// Process due embedding/tagging jobs from the unified DB-backed queue.
pub async fn process_queued_pipeline_jobs<F>(
    storage: StorageBackend,
    on_event: F,
    canvas_cache: Option<CanvasCache>,
) -> Result<i32, String>
where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    process_queued_pipeline_jobs_inner(storage, on_event, None, canvas_cache).await
}

/// Process due embedding/tagging jobs using externally-provided settings.
pub async fn process_queued_pipeline_jobs_with_settings<F>(
    storage: StorageBackend,
    on_event: F,
    settings_map: HashMap<String, String>,
    canvas_cache: Option<CanvasCache>,
) -> Result<i32, String>
where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    process_queued_pipeline_jobs_inner(storage, on_event, Some(settings_map), canvas_cache).await
}

async fn process_queued_pipeline_jobs_inner<F>(
    storage: StorageBackend,
    on_event: F,
    external_settings: Option<HashMap<String, String>>,
    canvas_cache: Option<CanvasCache>,
) -> Result<i32, String>
where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    let run_id = Uuid::new_v4().to_string();
    let mut batches = Vec::new();
    let mut total_count = 0usize;
    let mut embedding_total = 0usize;

    loop {
        let now = chrono::Utc::now();
        let lease_until = (now + chrono::Duration::minutes(30)).to_rfc3339();
        let now = now.to_rfc3339();
        let jobs = storage
            .claim_pipeline_jobs_sync(PENDING_BATCH_SIZE, &lease_until, &now)
            .await
            .map_err(|e| e.to_string())?;

        if jobs.is_empty() {
            break;
        }

        total_count += jobs.len();
        embedding_total += jobs.iter().filter(|job| job.embed_requested).count();
        batches.push(jobs);
    }

    if total_count == 0 {
        return Ok(0);
    }

    on_event(EmbeddingEvent::PipelineQueueStarted {
        run_id: run_id.clone(),
        total_jobs: total_count,
        embedding_total,
    });
    if embedding_total > 0 {
        on_event(EmbeddingEvent::PipelineQueueProgress {
            run_id: run_id.clone(),
            stage: "embedding".to_string(),
            completed: 0,
            total: embedding_total,
        });
    }

    let storage = storage.clone();
    let on_event = on_event.clone();
    let settings = external_settings.clone();
    let canvas_cache = canvas_cache.clone();
    let progress = Arc::new(QueueRunProgress::new(run_id.clone()));

    crate::executor::spawn(async move {
        let _permit = crate::executor::EMBEDDING_BATCH_SEMAPHORE
            .acquire()
            .await
            .expect("Embedding batch semaphore closed unexpectedly");

        for jobs in batches {
            process_pipeline_jobs_batch(
                storage.clone(),
                jobs,
                on_event.clone(),
                settings.clone(),
                canvas_cache.clone(),
                progress.clone(),
                embedding_total,
            )
            .await;
        }

        on_event(EmbeddingEvent::PipelineQueueCompleted {
            run_id,
            total_jobs: total_count,
            failed_jobs: progress.failed_jobs(),
        });
    });

    Ok(total_count as i32)
}

/// Claim up to `limit` due embedding/tagging jobs from the durable queue and
/// process them to completion before returning.
///
/// The bounded-batch counterpart of [`process_queued_pipeline_jobs`], for
/// hosts that run pipeline execution in a dedicated worker rather than
/// spawning it from the save path: one claim, one batch, awaited inline. No
/// task is spawned and [`crate::executor::EMBEDDING_BATCH_SEMAPHORE`] is not
/// taken — the caller owns its own concurrency discipline (that semaphore
/// exists to keep the fire-and-forget spawn path from stampeding a single
/// process). Emits the same `PipelineQueue*` event family as the spawning
/// path, scoped to this batch. Returns the number of jobs claimed; `0`
/// means nothing was due.
pub async fn run_pipeline_jobs_batch<F>(
    storage: StorageBackend,
    limit: i32,
    on_event: F,
    external_settings: Option<HashMap<String, String>>,
    canvas_cache: Option<CanvasCache>,
) -> Result<i32, String>
where
    F: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    let now = chrono::Utc::now();
    let lease_until = (now + chrono::Duration::minutes(30)).to_rfc3339();
    let now = now.to_rfc3339();
    let jobs = storage
        .claim_pipeline_jobs_sync(limit, &lease_until, &now)
        .await
        .map_err(|e| e.to_string())?;
    if jobs.is_empty() {
        return Ok(0);
    }

    let run_id = Uuid::new_v4().to_string();
    let total_jobs = jobs.len();
    let embedding_total = jobs.iter().filter(|job| job.embed_requested).count();
    on_event(EmbeddingEvent::PipelineQueueStarted {
        run_id: run_id.clone(),
        total_jobs,
        embedding_total,
    });
    if embedding_total > 0 {
        on_event(EmbeddingEvent::PipelineQueueProgress {
            run_id: run_id.clone(),
            stage: "embedding".to_string(),
            completed: 0,
            total: embedding_total,
        });
    }

    let progress = Arc::new(QueueRunProgress::new(run_id.clone()));
    process_pipeline_jobs_batch(
        storage,
        jobs,
        on_event.clone(),
        external_settings,
        canvas_cache,
        progress.clone(),
        embedding_total,
    )
    .await;

    on_event(EmbeddingEvent::PipelineQueueCompleted {
        run_id,
        total_jobs,
        failed_jobs: progress.failed_jobs(),
    });
    Ok(total_jobs as i32)
}

/// Convert L2 distance to cosine similarity for normalized vectors
/// Formula: cosine_similarity = 1 - (L2_distance² / 2)
/// This derives from: L2² = 2(1 - cos(θ)) for unit vectors
pub fn distance_to_similarity(distance: f32) -> f32 {
    (1.0 - (distance * distance / 2.0)).clamp(-1.0, 1.0)
}

/// Compute semantic edges for an atom after embedding generation
/// Finds similar atoms based on vector similarity and stores edges in semantic_edges table
pub fn compute_semantic_edges_for_atom(
    conn: &rusqlite::Connection,
    atom_id: &str,
    threshold: f32, // Default: 0.5 - lower than UI threshold to capture more relationships
    max_edges: i32, // Default: 15 per atom
) -> Result<i32, String> {
    use std::collections::HashMap;

    // First, delete existing edges for this atom (bidirectional)
    conn.execute(
        "DELETE FROM semantic_edges WHERE source_atom_id = ?1 OR target_atom_id = ?1",
        [atom_id],
    )
    .map_err(|e| format!("Failed to delete existing edges: {}", e))?;

    // Get all chunks for the given atom
    let mut stmt = conn
        .prepare("SELECT id, chunk_index, embedding FROM atom_chunks WHERE atom_id = ?1")
        .map_err(|e| format!("Failed to prepare chunk query: {}", e))?;

    let source_chunks: Vec<(String, i32, Vec<u8>)> = stmt
        .query_map([atom_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| format!("Failed to query chunks: {}", e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Failed to collect chunks: {}", e))?;

    if source_chunks.is_empty() {
        return Ok(0);
    }

    // Map to store best similarity per target atom_id
    // Value: (similarity, source_chunk_index, target_chunk_index)
    let mut atom_similarities: HashMap<String, (f32, i32, i32)> = HashMap::new();

    // For each source chunk, find similar chunks
    for (_, source_chunk_index, embedding_blob) in &source_chunks {
        // Query vec_chunks for similar chunks
        let mut vec_stmt = conn
            .prepare(
                "SELECT chunk_id, distance
                 FROM vec_chunks
                 WHERE embedding MATCH ?1
                 ORDER BY distance
                 LIMIT ?2",
            )
            .map_err(|e| format!("Failed to prepare vec query: {}", e))?;

        let similar_chunks: Vec<(String, f32)> = vec_stmt
            .query_map(rusqlite::params![embedding_blob, max_edges * 5], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|e| format!("Failed to query similar chunks: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect similar chunks: {}", e))?;

        // Filter by threshold, then batch-fetch chunk info
        let filtered: Vec<(String, f32)> = similar_chunks
            .into_iter()
            .filter(|(_, distance)| distance_to_similarity(*distance) >= threshold)
            .collect();

        if filtered.is_empty() {
            continue;
        }

        let chunk_ids: Vec<String> = filtered.iter().map(|(id, _)| id.clone()).collect();
        let placeholders = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let info_query = format!(
            "SELECT id, atom_id, chunk_index FROM atom_chunks WHERE id IN ({})",
            placeholders
        );
        let mut info_stmt = conn
            .prepare(&info_query)
            .map_err(|e| format!("Failed to prepare chunk info query: {}", e))?;
        let chunk_info_map: HashMap<String, (String, i32)> = info_stmt
            .query_map(rusqlite::params_from_iter(chunk_ids.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i32>(2)?,
                ))
            })
            .map_err(|e| format!("Failed to query chunk info: {}", e))?
            .filter_map(|r| r.ok())
            .map(|(id, atom_id, idx)| (id, (atom_id, idx)))
            .collect();

        for (chunk_id, distance) in filtered {
            let similarity = distance_to_similarity(distance);

            if let Some((target_atom_id, target_chunk_index)) = chunk_info_map.get(&chunk_id) {
                if target_atom_id == atom_id {
                    continue;
                }

                let entry = atom_similarities.entry(target_atom_id.clone());
                match entry {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if similarity > e.get().0 {
                            e.insert((similarity, *source_chunk_index, *target_chunk_index));
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert((similarity, *source_chunk_index, *target_chunk_index));
                    }
                }
            }
        }
    }

    // Sort by similarity and take top N
    let mut edges: Vec<(String, f32, i32, i32)> = atom_similarities
        .into_iter()
        .map(|(target_id, (sim, src_idx, tgt_idx))| (target_id, sim, src_idx, tgt_idx))
        .collect();

    edges.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    edges.truncate(max_edges as usize);

    // Insert edges (store bidirectionally with consistent ordering)
    let now = chrono::Utc::now().to_rfc3339();
    let mut edges_created = 0;

    for (target_atom_id, similarity, source_chunk_index, target_chunk_index) in edges {
        // Use consistent ordering: smaller ID is source
        let (src_id, tgt_id, src_chunk, tgt_chunk) = if atom_id < target_atom_id.as_str() {
            (
                atom_id.to_string(),
                target_atom_id.clone(),
                source_chunk_index,
                target_chunk_index,
            )
        } else {
            (
                target_atom_id.clone(),
                atom_id.to_string(),
                target_chunk_index,
                source_chunk_index,
            )
        };

        let edge_id = Uuid::new_v4().to_string();

        // Insert or update (using INSERT OR REPLACE due to UNIQUE constraint)
        let result = conn.execute(
            "INSERT OR REPLACE INTO semantic_edges
             (id, source_atom_id, target_atom_id, similarity_score, source_chunk_index, target_chunk_index, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![&edge_id, &src_id, &tgt_id, similarity, src_chunk, tgt_chunk, &now,],
        );

        if result.is_ok() {
            edges_created += 1;
        }
    }

    Ok(edges_created)
}

/// Max atoms to process per background embedding batch (limits memory usage).
const PENDING_BATCH_SIZE: i32 = 100;

/// Running accumulator for computing a centroid without holding all blobs in memory.
struct CentroidAccumulator {
    sum: Vec<f64>,
    count: usize,
}

impl CentroidAccumulator {
    fn new(dim: usize) -> Self {
        Self {
            sum: vec![0.0f64; dim],
            count: 0,
        }
    }

    /// Add an embedding blob to the running sum. Silently skips malformed blobs.
    fn add_blob(&mut self, blob: &[u8]) {
        let dim = self.sum.len();
        if blob.len() != dim * 4 {
            return;
        }
        for i in 0..dim {
            let bytes: [u8; 4] = [
                blob[i * 4],
                blob[i * 4 + 1],
                blob[i * 4 + 2],
                blob[i * 4 + 3],
            ];
            self.sum[i] += f32::from_le_bytes(bytes) as f64;
        }
        self.count += 1;
    }

    /// Finalize into a normalized unit-length f32 blob. Returns None if empty.
    fn finalize(&self) -> Option<Vec<u8>> {
        if self.count == 0 {
            return None;
        }
        let count = self.count as f64;
        let mut centroid: Vec<f64> = self.sum.iter().map(|v| v / count).collect();

        // Normalize to unit length
        let magnitude: f64 = centroid.iter().map(|v| v * v).sum::<f64>().sqrt();
        if magnitude > 0.0 {
            for val in &mut centroid {
                *val /= magnitude;
            }
        }

        let f32_vec: Vec<f32> = centroid.iter().map(|v| *v as f32).collect();
        Some(f32_vec_to_blob_public(&f32_vec))
    }
}

/// Write a computed centroid to tag_embeddings + vec_tags.
fn upsert_tag_centroid(
    conn: &rusqlite::Connection,
    tag_id: &str,
    embedding_blob: &[u8],
    chunk_count: i32,
) -> Result<(), String> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR REPLACE INTO tag_embeddings (tag_id, embedding, atom_count, updated_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![tag_id, embedding_blob, chunk_count, &now],
    )
    .map_err(|e| format!("Failed to upsert tag_embeddings: {}", e))?;

    // vec0 doesn't support REPLACE, so delete + insert
    conn.execute("DELETE FROM vec_tags WHERE tag_id = ?1", [tag_id])
        .ok();
    conn.execute(
        "INSERT INTO vec_tags (tag_id, embedding) VALUES (?1, ?2)",
        rusqlite::params![tag_id, embedding_blob],
    )
    .map_err(|e| format!("Failed to upsert vec_tags: {}", e))?;

    Ok(())
}

/// Compute the centroid embedding for a single tag (streaming, constant memory).
///
/// Averages all chunk embeddings from atoms under this tag (including descendant tags),
/// normalizes to unit length, and upserts into `tag_embeddings` + `vec_tags`.
pub fn compute_tag_embedding(conn: &rusqlite::Connection, tag_id: &str) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "WITH RECURSIVE descendant_tags(id) AS (
                SELECT ?1
                UNION ALL
                SELECT t.id FROM tags t
                INNER JOIN descendant_tags dt ON t.parent_id = dt.id
            )
            SELECT ac.embedding
            FROM atom_chunks ac
            INNER JOIN atom_tags at ON ac.atom_id = at.atom_id
            WHERE at.tag_id IN (SELECT id FROM descendant_tags)
              AND ac.embedding IS NOT NULL",
        )
        .map_err(|e| format!("Failed to prepare tag embedding query: {}", e))?;

    // Determine dimension from vec_chunks schema
    let dim: usize = conn
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

    let mut acc = CentroidAccumulator::new(dim);

    let mut rows = stmt
        .query([tag_id])
        .map_err(|e| format!("Failed to query tag embeddings: {}", e))?;

    while let Some(row) = rows
        .next()
        .map_err(|e| format!("Failed to read row: {}", e))?
    {
        let blob: Vec<u8> = row
            .get(0)
            .map_err(|e| format!("Failed to get blob: {}", e))?;
        acc.add_blob(&blob);
    }

    match acc.finalize() {
        Some(blob) => upsert_tag_centroid(conn, tag_id, &blob, acc.count as i32),
        None => {
            conn.execute("DELETE FROM vec_tags WHERE tag_id = ?1", [tag_id])
                .ok();
            conn.execute("DELETE FROM tag_embeddings WHERE tag_id = ?1", [tag_id])
                .ok();
            Ok(())
        }
    }
}

/// Compute centroid embeddings for multiple tags in a single pass.
///
/// Builds an inverted ancestry map so each chunk embedding is read from SQLite exactly
/// once and accumulated into every tag centroid that includes it (the tag itself + all
/// its ancestors in the affected set).
pub fn compute_tag_embeddings_batch(
    conn: &rusqlite::Connection,
    tag_ids: &[String],
) -> Result<(), String> {
    if tag_ids.is_empty() {
        return Ok(());
    }

    // For each target tag, get its full descendant hierarchy. Build an inverted map:
    // descendant_tag_id → set of target tag_ids whose centroid it contributes to.
    let mut descendant_to_targets: std::collections::HashMap<String, Vec<&str>> =
        std::collections::HashMap::new();

    for tag_id in tag_ids {
        let mut stmt = conn
            .prepare(
                "WITH RECURSIVE descendant_tags(id) AS (
                    SELECT ?1
                    UNION ALL
                    SELECT t.id FROM tags t
                    INNER JOIN descendant_tags dt ON t.parent_id = dt.id
                )
                SELECT id FROM descendant_tags",
            )
            .map_err(|e| format!("Failed to prepare hierarchy query: {}", e))?;

        let descendants: Vec<String> = stmt
            .query_map([tag_id.as_str()], |row| row.get(0))
            .map_err(|e| format!("Failed to query hierarchy: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect hierarchy: {}", e))?;

        for desc_id in descendants {
            descendant_to_targets
                .entry(desc_id)
                .or_default()
                .push(tag_id.as_str());
        }
    }

    // Determine embedding dimension from vec_chunks schema
    let dim: usize = conn
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

    // Initialize accumulators for each target tag
    let mut accumulators: std::collections::HashMap<&str, CentroidAccumulator> = tag_ids
        .iter()
        .map(|id| (id.as_str(), CentroidAccumulator::new(dim)))
        .collect();

    // Collect all descendant tag IDs that map to at least one target
    let all_descendant_ids: Vec<&str> = descendant_to_targets.keys().map(|s| s.as_str()).collect();

    // Stream chunk embeddings for all atoms tagged under any descendant, in batches
    // to avoid SQLite parameter limits (max ~999)
    for batch in all_descendant_ids.chunks(500) {
        let placeholders = batch.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT at.tag_id, ac.embedding
             FROM atom_chunks ac
             INNER JOIN atom_tags at ON ac.atom_id = at.atom_id
             WHERE at.tag_id IN ({})
               AND ac.embedding IS NOT NULL",
            placeholders
        );

        let mut stmt = conn
            .prepare(&query)
            .map_err(|e| format!("Failed to prepare batch embedding query: {}", e))?;

        let mut rows = stmt
            .query(rusqlite::params_from_iter(batch.iter()))
            .map_err(|e| format!("Failed to query batch embeddings: {}", e))?;

        while let Some(row) = rows
            .next()
            .map_err(|e| format!("Failed to read row: {}", e))?
        {
            let tag_id: String = row
                .get(0)
                .map_err(|e| format!("Failed to get tag_id: {}", e))?;
            let blob: Vec<u8> = row
                .get(1)
                .map_err(|e| format!("Failed to get blob: {}", e))?;

            // Look up which target centroids this row contributes to
            if let Some(targets) = descendant_to_targets.get(&tag_id) {
                for &target_id in targets {
                    if let Some(acc) = accumulators.get_mut(target_id) {
                        acc.add_blob(&blob);
                    }
                }
            }
        }
    }

    // Finalize and write all centroids
    for tag_id in tag_ids {
        if let Some(acc) = accumulators.get(tag_id.as_str()) {
            match acc.finalize() {
                Some(blob) => {
                    if let Err(e) = upsert_tag_centroid(conn, tag_id, &blob, acc.count as i32) {
                        tracing::warn!(tag_id, error = %e, "Failed to write centroid for tag");
                    }
                }
                None => {
                    conn.execute("DELETE FROM vec_tags WHERE tag_id = ?1", [tag_id.as_str()])
                        .ok();
                    conn.execute(
                        "DELETE FROM tag_embeddings WHERE tag_id = ?1",
                        [tag_id.as_str()],
                    )
                    .ok();
                }
            }
        }
    }

    Ok(())
}

/// Max atoms to process per edge computation batch.
const EDGE_BATCH_SIZE: i32 = 500;

/// Process all atoms with pending edge computation in batches.
///
/// Claims atoms in batches, computes edges in a single transaction, marks them
/// complete, and repeats. Each batch is checkpointed so progress survives restarts.
/// Returns the total number of atoms processed.
pub async fn process_pending_edges(
    storage: StorageBackend,
    canvas_cache: Option<CanvasCache>,
) -> Result<i32, String> {
    let pending_count = storage
        .count_pending_edges_sync()
        .await
        .map_err(|e| e.to_string())?;

    if pending_count == 0 {
        return Ok(0);
    }

    tracing::info!(count = pending_count, "Starting batched edge computation");

    let storage_clone = storage.clone();
    crate::executor::spawn(async move {
        let mut total_processed = 0;
        loop {
            let batch = match storage_clone
                .claim_pending_edges_sync(EDGE_BATCH_SIZE)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to claim atoms for edge computation");
                    break;
                }
            };

            if batch.is_empty() {
                break;
            }

            let batch_size = batch.len();

            // Compute all edges in a single transaction (one lock acquire, one fsync)
            let batch_edges = match storage_clone
                .compute_semantic_edges_batch_sync(&batch, 0.5, 15)
                .await
            {
                Ok(count) => count,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to compute edges for batch");
                    // Mark as complete anyway to avoid infinite retry
                    0
                }
            };

            // Checkpoint: mark this batch complete before claiming the next
            if let Err(e) = storage_clone
                .set_edges_status_batch_sync(&batch, "complete")
                .await
            {
                tracing::error!(error = %e, "Failed to mark edges as complete");
                break;
            }

            // Schedule a debounced canvas rebuild so subsequent reads pick
            // up the new edges without thrashing when many batches land in
            // quick succession — successive calls collapse into one rebuild.
            if let Some(cache) = &canvas_cache {
                cache.invalidate_debounced();
            }

            total_processed += batch_size;
            tracing::info!(
                batch_edges,
                progress = total_processed,
                "Edge computation batch complete"
            );

            // Yield to other tasks between batches
            tokio::task::yield_now().await;
        }

        tracing::info!(
            total = total_processed,
            "Edge computation pipeline complete"
        );
    });

    Ok(pending_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distance_to_similarity() {
        // Formula: 1.0 - (distance² / 2.0), clamped to [-1.0, 1.0]
        assert!((distance_to_similarity(0.0) - 1.0).abs() < 0.001); // 1 - 0 = 1.0
        assert!((distance_to_similarity(1.0) - 0.5).abs() < 0.001); // 1 - 0.5 = 0.5
                                                                    // distance = √2 gives 1.0 - 1.0 = 0.0
        assert!((distance_to_similarity(std::f32::consts::SQRT_2) - 0.0).abs() < 0.001);
        // distance = 2.0 gives 1.0 - 2.0 = -1.0 (clamped)
        assert!((distance_to_similarity(2.0) - (-1.0)).abs() < 0.001);
    }
}
