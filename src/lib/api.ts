import { getTransport } from './transport';
import { HttpTransport } from './transport/http';
import { isTauri } from './platform';

// Type-safe wrapper for checking sqlite-vec
export async function checkSqliteVec(): Promise<string> {
  return getTransport().invoke<string>('check_sqlite_vec');
}

// Semantic search
export async function searchAtomsSemantic(
  query: string,
  limit: number = 20,
  threshold: number = 0.3
): Promise<any[]> {
  return getTransport().invoke('search_atoms_semantic', { query, limit, threshold });
}

// Find similar atoms
export async function findSimilarAtoms(
  atomId: string,
  limit: number = 5,
  threshold: number = 0.7
): Promise<any[]> {
  return getTransport().invoke('find_similar_atoms', { atomId, limit, threshold });
}

// Retry embedding
export async function retryEmbedding(atomId: string): Promise<void> {
  return getTransport().invoke('retry_embedding', { atomId });
}

// Retry tagging
export async function retryTagging(atomId: string): Promise<void> {
  return getTransport().invoke('retry_tagging', { atomId });
}

export interface FailedPipelineAtom {
  atom_id: string;
  title: string;
  snippet: string;
  error: string | null;
  updated_at: string;
}

export interface PipelineStatus {
  pending: number;
  processing: number;
  complete: number;
  failed_count: number;
  failed: FailedPipelineAtom[];
  queued_embedding: number;
  queued_tagging: number;
  tagging_pending: number;
  tagging_processing: number;
  tagging_complete: number;
  tagging_skipped: number;
  tagging_failed_count: number;
  tagging_failed: FailedPipelineAtom[];
  /** Per-DB count of `atom_tags` rows that existed before the source-tracking
   * migration ran. They default to source='auto' and so are candidates for
   * deletion on a "Re-tag all atoms" run. The UI uses this to warn honestly
   * about pre-upgrade rows. Always 0 on Postgres backends and on fresh installs. */
  legacy_auto_tag_count: number;
}

export interface DatabasePipelineStatus {
  database: {
    id: string;
    name: string;
    is_default: boolean;
    created_at: string;
    last_opened_at: string | null;
  };
  status: PipelineStatus;
}

export async function getAllPipelineStatuses(): Promise<DatabasePipelineStatus[]> {
  const result = await getTransport().invoke<{ databases: DatabasePipelineStatus[] }>('get_all_pipeline_statuses');
  return result.databases;
}

export async function retryFailedEmbeddings(dbId: string): Promise<number> {
  return getTransport().invoke('retry_failed_embeddings', { dbId });
}

export async function retryFailedTagging(dbId: string): Promise<number> {
  return getTransport().invoke('retry_failed_tagging', { dbId });
}

// Re-embed all atoms
export async function reembedAllAtoms(dbId?: string): Promise<number> {
  return getTransport().invoke('reembed_all_atoms', dbId ? { dbId } : undefined);
}

// Re-run auto-tagging across all atoms in a database. Removes auto-source
// tag assignments whose tag has no wiki article, then queues every atom
// for tag-only pipeline processing. Manual assignments and wiki-backed
// tag assignments are preserved.
export async function retagAllAtoms(dbId?: string): Promise<number> {
  return getTransport().invoke('retag_all_atoms', dbId ? { dbId } : undefined);
}

export type ExportJobStatus = 'queued' | 'running' | 'complete' | 'failed' | 'cancelled';

export interface ExportJob {
  id: string;
  db_id: string;
  db_name: string;
  status: ExportJobStatus;
  phase: string;
  total_atoms: number;
  processed_atoms: number;
  bytes_written: number;
  created_at: string;
  updated_at: string;
  completed_at: string | null;
  error: string | null;
  download_path: string | null;
  download_expires_at: string | null;
}

export async function startDatabaseMarkdownExport(dbId: string): Promise<ExportJob> {
  return getTransport().invoke('start_markdown_export', { id: dbId });
}

export async function getExportJob(id: string): Promise<ExportJob> {
  return getTransport().invoke('get_export_job', { id });
}

export async function cancelExportJob(id: string): Promise<ExportJob> {
  return getTransport().invoke('cancel_export_job', { id });
}

export async function exportDatabaseMarkdownArchive(
  dbId: string,
  onProgress?: (job: ExportJob) => void,
): Promise<ExportJob> {
  let job = await startDatabaseMarkdownExport(dbId);
  onProgress?.(job);

  while (job.status === 'queued' || job.status === 'running') {
    await new Promise(resolve => setTimeout(resolve, 1000));
    job = await getExportJob(job.id);
    onProgress?.(job);
  }

  if (job.status !== 'complete') {
    throw new Error(job.error || `Export ${job.status}`);
  }

  await downloadExportJob(job);
  return job;
}

export async function downloadExportJob(job: ExportJob): Promise<void> {
  if (!job.download_path) {
    throw new Error('Export is complete but no download URL was issued');
  }

  const transport = getTransport();
  if (!(transport instanceof HttpTransport)) {
    throw new Error('Markdown export requires an HTTP transport');
  }

  const { baseUrl } = transport.getConfig();
  if (!baseUrl) {
    throw new Error('Not connected to a server');
  }

  if (isTauri()) {
    await saveExportJobWithTauri(baseUrl, job);
    return;
  }

  const url = `${baseUrl}${job.download_path}`;
  const a = document.createElement('a');
  a.href = url;
  a.rel = 'noopener noreferrer';
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
}

async function saveExportJobWithTauri(baseUrl: string, job: ExportJob): Promise<void> {
  if (!job.download_path) {
    throw new Error('Export is complete but no download URL was issued');
  }

  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('save_markdown_export', {
    baseUrl,
    downloadPath: job.download_path,
    defaultFileName: defaultExportFilename(job),
  });
}

function defaultExportFilename(job: ExportJob): string {
  const dbName = job.db_name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    || 'database';
  return `atomic-${dbName}-markdown.zip`;
}

export type MigrationJobStatus = 'queued' | 'running' | 'complete' | 'failed' | 'cancelled';

export interface MigrationTableReport {
  table: string;
  source_rows: number;
  copied_rows: number;
}

export interface MigrationReport {
  db_id: string | null;
  db_name: string;
  dry_run: boolean;
  tables: MigrationTableReport[];
  skipped_feed_urls: string[];
  duration_ms: number;
}

export interface MigrationJob {
  id: string;
  kind: 'import' | 'push';
  status: MigrationJobStatus;
  phase: string;
  db_name: string;
  total_rows: number;
  processed_rows: number;
  db_id: string | null;
  report: MigrationReport | null;
  error: string | null;
  created_at: string;
  updated_at: string;
  completed_at: string | null;
}

export interface MigrationPushParams {
  targetUrl: string;
  targetToken: string;
  databaseId: string;
  name?: string;
  pauseFeeds?: boolean;
}

export async function startMigrationPush(params: MigrationPushParams): Promise<MigrationJob> {
  return getTransport().invoke('start_migration_push', { ...params });
}

export async function getMigrationJob(id: string): Promise<MigrationJob> {
  return getTransport().invoke('get_migration_job', { id });
}

export async function cancelMigrationJob(id: string): Promise<MigrationJob> {
  return getTransport().invoke('cancel_migration_job', { id });
}

/// Push a local database to a remote Atomic server, polling the local push
/// job (which mirrors the remote import job) until it reaches a terminal
/// state. Mirrors the exportDatabaseMarkdownArchive polling pattern.
export async function pushDatabaseToCloud(
  params: MigrationPushParams,
  onProgress?: (job: MigrationJob) => void,
): Promise<MigrationJob> {
  let job = await startMigrationPush(params);
  onProgress?.(job);

  while (job.status === 'queued' || job.status === 'running') {
    await new Promise(resolve => setTimeout(resolve, 1000));
    job = await getMigrationJob(job.id);
    onProgress?.(job);
  }

  if (job.status !== 'complete') {
    throw new Error(job.error || `Migration ${job.status}`);
  }
  return job;
}

// Reset atoms stuck in 'processing' state (call on app startup)
export async function resetStuckProcessing(): Promise<number> {
  return getTransport().invoke('reset_stuck_processing');
}

// Process pending embeddings
export async function processPendingEmbeddings(): Promise<number> {
  return getTransport().invoke('process_pending_embeddings');
}

// Process pending tagging (for atoms with completed embeddings)
export async function processPendingTagging(): Promise<number> {
  return getTransport().invoke('process_pending_tagging');
}

export async function processAtomPipeline(atomId: string): Promise<void> {
  return getTransport().invoke('process_atom_pipeline', { id: atomId });
}

// Get embedding status
export async function getEmbeddingStatus(atomId: string): Promise<string> {
  return getTransport().invoke('get_embedding_status', { atomId });
}

// Wiki commands
export async function getWikiArticle(tagId: string): Promise<any | null> {
  return getTransport().invoke('get_wiki_article', { tagId });
}

export async function getWikiArticleStatus(tagId: string): Promise<any> {
  return getTransport().invoke('get_wiki_article_status', { tagId });
}

export async function generateWikiArticle(tagId: string, tagName: string): Promise<any> {
  return getTransport().invoke('generate_wiki_article', { tagId, tagName });
}

export async function updateWikiArticle(tagId: string, tagName: string): Promise<any> {
  return getTransport().invoke('update_wiki_article', { tagId, tagName });
}

export async function deleteWikiArticle(tagId: string): Promise<void> {
  return getTransport().invoke('delete_wiki_article', { tagId });
}

// Canvas position commands
export interface AtomPosition {
  atom_id: string;
  x: number;
  y: number;
}

export interface AtomWithEmbedding {
  id: string;
  content: string;
  source_url: string | null;
  created_at: string;
  updated_at: string;
  embedding_status: string;
  tags: Array<{
    id: string;
    name: string;
    parent_id: string | null;
    created_at: string;
  }>;
  embedding: number[] | null;
}

export async function getAtomPositions(): Promise<AtomPosition[]> {
  return getTransport().invoke('get_atom_positions');
}

export async function saveAtomPositions(positions: AtomPosition[]): Promise<void> {
  return getTransport().invoke('save_atom_positions', { positions });
}

export async function getAtomsWithEmbeddings(): Promise<AtomWithEmbedding[]> {
  return getTransport().invoke('get_atoms_with_embeddings');
}

// Global canvas (PCA-projected positions)
export interface CanvasAtomPosition {
  atom_id: string;
  x: number;
  y: number;
  title: string;
  primary_tag: string | null;
  tag_count: number;
  tag_ids: string[];
}

export interface CanvasEdgeData {
  source: string;
  target: string;
  weight: number;
}

export interface CanvasClusterLabel {
  id: string;
  x: number;
  y: number;
  label: string;
  atom_count: number;
  atom_ids: string[];
}

export interface GlobalCanvasData {
  atoms: CanvasAtomPosition[];
  edges: CanvasEdgeData[];
  clusters: CanvasClusterLabel[];
}

export async function getGlobalCanvas(): Promise<GlobalCanvasData> {
  return getTransport().invoke('get_global_canvas', {});
}

// Semantic graph types and commands
export interface SemanticEdge {
  id: string;
  source_atom_id: string;
  target_atom_id: string;
  similarity_score: number;
  source_chunk_index: number | null;
  target_chunk_index: number | null;
  created_at: string;
}

export interface NeighborhoodAtom {
  id: string;
  content: string;
  source_url: string | null;
  created_at: string;
  updated_at: string;
  embedding_status: string;
  tags: Array<{
    id: string;
    name: string;
    parent_id: string | null;
    created_at: string;
  }>;
  depth: number; // 0 = center, 1 = direct connection, 2 = friend-of-friend
}

export interface NeighborhoodEdge {
  source_id: string;
  target_id: string;
  edge_type: 'tag' | 'semantic' | 'both';
  strength: number; // 0-1
  shared_tag_count: number;
  similarity_score: number | null;
}

export interface NeighborhoodGraph {
  center_atom_id: string;
  atoms: NeighborhoodAtom[];
  edges: NeighborhoodEdge[];
}

export async function getSemanticEdges(minSimilarity: number = 0.5): Promise<SemanticEdge[]> {
  return getTransport().invoke('get_semantic_edges', { minSimilarity });
}

export async function getAtomNeighborhood(
  atomId: string,
  depth: number = 1,
  minSimilarity: number = 0.5
): Promise<NeighborhoodGraph> {
  return getTransport().invoke('get_atom_neighborhood', { atomId, depth, minSimilarity });
}

export async function rebuildSemanticEdges(): Promise<number> {
  return getTransport().invoke('rebuild_semantic_edges');
}

// Clustering types and commands
export interface AtomCluster {
  cluster_id: number;
  atom_ids: string[];
  dominant_tags: string[];
}

export async function computeClusters(
  minSimilarity: number = 0.5,
  minClusterSize: number = 2
): Promise<AtomCluster[]> {
  return getTransport().invoke('compute_clusters', { minSimilarity, minClusterSize });
}

export async function getClusters(): Promise<AtomCluster[]> {
  return getTransport().invoke('get_clusters');
}

export async function getConnectionCounts(
  minSimilarity: number = 0.5
): Promise<Record<string, number>> {
  return getTransport().invoke('get_connection_counts', { minSimilarity });
}

// Model discovery types and commands
export interface AvailableModel {
  id: string;
  name: string;
}

export async function getAvailableLlmModels(): Promise<AvailableModel[]> {
  return getTransport().invoke('get_available_llm_models');
}

// OpenRouter embedding model registry (curated list with known vector dimensions)
export interface OpenRouterEmbeddingModel {
  id: string;
  name: string;
  dimension: number;
  context_length: number;
}

export async function getOpenRouterEmbeddingModels(): Promise<OpenRouterEmbeddingModel[]> {
  return getTransport().invoke('get_openrouter_embedding_models');
}

// Ollama types and commands
export interface OllamaModel {
  id: string;
  name: string;
  is_embedding: boolean;
  embedding_dimension: number | null;
}

export async function testOllamaConnection(host: string): Promise<boolean> {
  return getTransport().invoke('test_ollama', { host });
}

export async function testOpenAICompatConnection(baseUrl: string, apiKey?: string): Promise<boolean> {
  return getTransport().invoke('test_openai_compat_connection', { baseUrl, apiKey });
}

export async function getOllamaModels(host: string): Promise<OllamaModel[]> {
  return getTransport().invoke('get_ollama_models', { host });
}

export async function getOllamaEmbeddingModels(host: string): Promise<AvailableModel[]> {
  return getTransport().invoke('get_ollama_embedding_models_cmd', { host });
}

export async function getOllamaLlmModels(host: string): Promise<AvailableModel[]> {
  return getTransport().invoke('get_ollama_llm_models_cmd', { host });
}

// Setup verification
export async function verifyProviderConfigured(): Promise<boolean> {
  return getTransport().invoke('verify_provider_configured');
}

// Import types and commands
export interface ImportResult {
  imported: number;
  skipped: number;
  errors: number;
  tags_created: number;
  tags_linked: number;
}

export async function importObsidianVault(
  vaultPath: string,
  maxNotes?: number
): Promise<ImportResult> {
  return getTransport().invoke('import_obsidian_vault', { vaultPath, maxNotes });
}

// API Token types and commands
export interface ApiTokenInfo {
  id: string;
  name: string;
  token_prefix: string;
  created_at: string;
  last_used_at: string | null;
  is_revoked: boolean;
}

export interface CreateTokenResponse {
  id: string;
  name: string;
  token: string;
  prefix: string;
  created_at: string;
}

export async function createApiToken(name: string): Promise<CreateTokenResponse> {
  return getTransport().invoke('create_api_token', { name });
}

export async function listApiTokens(): Promise<ApiTokenInfo[]> {
  return getTransport().invoke('list_api_tokens');
}

export async function revokeApiToken(id: string): Promise<void> {
  return getTransport().invoke('revoke_api_token', { id });
}

// Feed types and commands
export interface Feed {
  id: string;
  url: string;
  title: string | null;
  site_url: string | null;
  poll_interval: number;
  last_polled_at: string | null;
  last_error: string | null;
  created_at: string;
  is_paused: boolean;
  tag_ids: string[];
}

export interface IngestionResult {
  atom_id: string;
  url: string;
  title: string;
  content_length: number;
}

export interface FeedPollResult {
  feed_id: string;
  new_items: number;
  skipped: number;
  errors: number;
}

export async function ingestUrl(url: string, tagIds?: string[]): Promise<IngestionResult> {
  return getTransport().invoke('ingest_url', { url, tagIds });
}

export async function listFeeds(): Promise<Feed[]> {
  return getTransport().invoke('list_feeds');
}

export async function createFeed(url: string, pollInterval?: number, tagIds?: string[]): Promise<Feed> {
  return getTransport().invoke('create_feed', { url, pollInterval, tagIds });
}

export async function updateFeed(id: string, opts: { pollInterval?: number; isPaused?: boolean; tagIds?: string[] }): Promise<Feed> {
  return getTransport().invoke('update_feed', { id, ...opts });
}

export async function deleteFeed(id: string): Promise<void> {
  return getTransport().invoke('delete_feed', { id });
}

export async function pollFeed(id: string): Promise<FeedPollResult> {
  return getTransport().invoke('poll_feed', { id });
}

// MCP config — stdio for local desktop, HTTP+auth for remote/web
export interface McpStdioConfig {
  mcpServers: {
    atomic: {
      command: string;
    };
  };
}

export interface McpHttpConfig {
  mcpServers: {
    atomic: {
      url: string;
      headers: {
        Authorization: string;
      };
    };
  };
}

export type McpConfig = McpStdioConfig | McpHttpConfig;

// Logs
export async function exportLogs(): Promise<string> {
  const result = await getTransport().invoke<{ logs: string }>('get_logs');
  return result.logs;
}

export function getMcpStdioConfig(bridgePath: string): McpStdioConfig {
  return {
    mcpServers: {
      atomic: {
        command: bridgePath,
      },
    },
  };
}

export function getMcpHttpConfig(serverBaseUrl: string, token: string): McpHttpConfig {
  return {
    mcpServers: {
      atomic: {
        url: `${serverBaseUrl}/mcp`,
        headers: {
          Authorization: `Bearer ${token}`,
        },
      },
    },
  };
}
