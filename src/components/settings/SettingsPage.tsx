import { useState, useEffect, useCallback } from 'react';
import { toast } from 'sonner';
import { Button } from '../ui/Button';

function normalizeUrl(url: string): string {
  const trimmed = url.trim();
  if (!trimmed) return trimmed;
  if (!/^https?:\/\//i.test(trimmed)) return `http://${trimmed}`;
  return trimmed;
}
import { CustomSelect } from '../ui/CustomSelect';
import { SearchableSelect } from '../ui/SearchableSelect';
import { ConnectionStatus } from '../ui/ConnectionStatus';
import { useSettingsStore } from '../../stores/settings';
import { useAtomsStore } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { useUIStore } from '../../stores/ui';
import { THEMES, Theme } from '../../hooks/useTheme';
import { FONTS, Font } from '../../hooks/useFont';
import {
  getAvailableLlmModels,
  testOllamaConnection,
  testOpenAICompatConnection,
  getOllamaModels,
  importObsidianVault,
  getMcpConfig,
  listApiTokens,
  createApiToken,
  revokeApiToken,
  ingestUrl,
  listFeeds,
  createFeed,
  updateFeed,
  deleteFeed,
  pollFeed,
  type AvailableModel,
  type OllamaModel,
  type ImportResult,
  type McpConfig,
  type ApiTokenInfo,
  type CreateTokenResponse,
  type Feed,
  reembedAllAtoms,
  exportLogs,
  type IngestionResult,
  type FeedPollResult,
} from '../../lib/api';
import { getTransport, switchTransport, switchToLocal, isDesktopApp, isLocalServer, type HttpTransportConfig } from '../../lib/transport';
import { pickDirectory } from '../../lib/platform';
import { formatRelativeDate } from '../../lib/date';
import { useDatabasesStore, type DatabaseInfo, type DatabaseStats } from '../../stores/databases';

type SettingsTab = 'general' | 'ai' | 'connection' | 'feeds' | 'integrations' | 'databases';

const SETTINGS_TABS: { id: SettingsTab; label: string; icon: React.ReactNode }[] = [
  { id: 'general', label: 'General', icon: (
    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
    </svg>
  ) },
  { id: 'ai', label: 'AI Models', icon: (
    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z" />
    </svg>
  ) },
  { id: 'connection', label: 'Connection', icon: (
    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 12a9 9 0 01-9 9m9-9a9 9 0 00-9-9m9 9H3m9 9a9 9 0 01-9-9m9 9c1.657 0 3-4.03 3-9s-1.343-9-3-9m0 18c-1.657 0-3-4.03-3-9s1.343-9 3-9m-9 9a9 9 0 019-9" />
    </svg>
  ) },
  { id: 'feeds', label: 'Feeds', icon: (
    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 5c7.18 0 13 5.82 13 13M6 11a7 7 0 017 7m-6 0a1 1 0 11-2 0 1 1 0 012 0z" />
    </svg>
  ) },
  { id: 'integrations', label: 'Integrations', icon: (
    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M11 4a2 2 0 114 0v1a1 1 0 001 1h3a1 1 0 011 1v3a1 1 0 01-1 1h-1a2 2 0 100 4h1a1 1 0 011 1v3a1 1 0 01-1 1h-3a1 1 0 01-1-1v-1a2 2 0 10-4 0v1a1 1 0 01-1 1H7a1 1 0 01-1-1v-3a1 1 0 00-1-1H4a2 2 0 110-4h1a1 1 0 001-1V7a1 1 0 011-1h3a1 1 0 001-1V4z" />
    </svg>
  ) },
  { id: 'databases', label: 'Databases', icon: (
    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 7v10c0 2.21 3.582 4 8 4s8-1.79 8-4V7M4 7c0 2.21 3.582 4 8 4s8-1.79 8-4M4 7c0-2.21 3.582-4 8-4s8 1.79 8 4m0 5c0 2.21-3.582 4-8 4s-8-1.79-8-4" />
    </svg>
  ) },
];

function DatabasesTab() {
  const { databases, activeId, fetchDatabases, renameDatabase, deleteDatabase, setDefaultDatabase, getDatabaseStats } = useDatabasesStore();
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editName, setEditName] = useState('');
  const [confirmDeleteDb, setConfirmDeleteDb] = useState<DatabaseInfo | null>(null);
  const [deleteStats, setDeleteStats] = useState<DatabaseStats | null>(null);
  const [isLoadingStats, setIsLoadingStats] = useState(false);
  const [isDeleting, setIsDeleting] = useState(false);

  useEffect(() => {
    fetchDatabases();
  }, [fetchDatabases]);

  const handleRename = async (id: string) => {
    const trimmed = editName.trim();
    if (!trimmed) { setEditingId(null); return; }
    await renameDatabase(id, trimmed);
    setEditingId(null);
  };

  const handleStartDelete = async (db: DatabaseInfo) => {
    setConfirmDeleteDb(db);
    setDeleteStats(null);
    setIsLoadingStats(true);
    try {
      const stats = await getDatabaseStats(db.id);
      setDeleteStats(stats);
    } catch {
      setDeleteStats({ atom_count: -1 });
    }
    setIsLoadingStats(false);
  };

  const handleConfirmDelete = async () => {
    if (!confirmDeleteDb) return;
    setIsDeleting(true);
    try {
      await deleteDatabase(confirmDeleteDb.id);
      setConfirmDeleteDb(null);
    } catch {
      // Keep dialog open so user can retry or cancel
    } finally {
      setIsDeleting(false);
    }
  };

  const handleSetDefault = async (id: string) => {
    await setDefaultDatabase(id);
  };

  return (
    <>
      <div className="space-y-1">
        <h3 className="text-sm font-medium text-[var(--color-text-primary)]">Manage Databases</h3>
        <p className="text-xs text-[var(--color-text-secondary)]">
          Rename, delete, or change the default database. The default database is used by integrations (MCP, API).
        </p>
      </div>

      <div className="space-y-1">
        {databases.map(db => (
          <div
            key={db.id}
            className="flex items-center gap-3 px-3 py-2.5 rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-main)]"
          >
            {editingId === db.id ? (
              <input
                autoFocus
                className="flex-1 bg-transparent border border-[var(--color-accent)] rounded px-2 py-1 text-sm text-[var(--color-text-primary)] outline-none"
                value={editName}
                onChange={e => setEditName(e.target.value)}
                onKeyDown={e => {
                  if (e.key === 'Enter') handleRename(db.id);
                  if (e.key === 'Escape') setEditingId(null);
                }}
                onBlur={() => handleRename(db.id)}
              />
            ) : (
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <span className="text-sm text-[var(--color-text-primary)] truncate">{db.name}</span>
                  {db.is_default && (
                    <span className="text-[10px] px-1.5 py-0.5 rounded bg-[var(--color-accent)]/20 text-[var(--color-accent)] font-medium">
                      Default
                    </span>
                  )}
                  {db.id === activeId && (
                    <span className="text-[10px] px-1.5 py-0.5 rounded bg-green-500/20 text-green-400 font-medium">
                      Active
                    </span>
                  )}
                </div>
              </div>
            )}

            {editingId !== db.id && (
              <div className="flex items-center gap-1">
                {!db.is_default && (
                  <button
                    onClick={() => handleSetDefault(db.id)}
                    className="px-2 py-1 text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                    title="Set as default"
                  >
                    Set default
                  </button>
                )}
                <button
                  onClick={() => { setEditingId(db.id); setEditName(db.name); }}
                  className="p-1.5 text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                  title="Rename"
                >
                  <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor">
                    <path d="M12.146.854a.5.5 0 0 1 .708 0l2.292 2.292a.5.5 0 0 1 0 .708l-9.5 9.5a.5.5 0 0 1-.168.11l-4 1.5a.5.5 0 0 1-.65-.65l1.5-4a.5.5 0 0 1 .11-.168l9.5-9.5z"/>
                  </svg>
                </button>
                {!db.is_default && (
                  <button
                    onClick={() => handleStartDelete(db)}
                    className="p-1.5 text-[var(--color-text-tertiary)] hover:text-red-400 hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                    title="Delete database"
                  >
                    <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor">
                      <path d="M5.5 5.5A.5.5 0 0 1 6 6v6a.5.5 0 0 1-1 0V6a.5.5 0 0 1 .5-.5zm2.5 0a.5.5 0 0 1 .5.5v6a.5.5 0 0 1-1 0V6a.5.5 0 0 1 .5-.5zm3 .5a.5.5 0 0 0-1 0v6a.5.5 0 0 0 1 0V6z"/>
                      <path fillRule="evenodd" d="M14.5 3a1 1 0 0 1-1 1H13v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V4h-.5a1 1 0 0 1-1-1V2a1 1 0 0 1 1-1H6a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1h3.5a1 1 0 0 1 1 1v1zM4.118 4L4 4.059V13a1 1 0 0 0 1 1h6a1 1 0 0 0 1-1V4.059L11.882 4H4.118z"/>
                    </svg>
                  </button>
                )}
              </div>
            )}
          </div>
        ))}
      </div>

      {databases.length === 0 && (
        <p className="text-sm text-[var(--color-text-secondary)] text-center py-4">No databases found.</p>
      )}

      {/* Delete confirmation dialog */}
      {confirmDeleteDb && (
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50">
          <div className="bg-[var(--color-bg-panel)] border border-[var(--color-border)] rounded-lg shadow-xl p-6 mx-4 max-w-sm w-full space-y-4">
            <div className="space-y-2">
              <h3 className="text-sm font-semibold text-[var(--color-text-primary)]">Delete database?</h3>
              <p className="text-xs text-[var(--color-text-secondary)]">
                This will permanently delete <span className="font-medium text-[var(--color-text-primary)]">"{confirmDeleteDb.name}"</span>
                {isLoadingStats ? (
                  <span> and all its data.</span>
                ) : deleteStats && deleteStats.atom_count >= 0 ? (
                  <span> and its <span className="font-medium text-[var(--color-text-primary)]">{deleteStats.atom_count} atom{deleteStats.atom_count !== 1 ? 's' : ''}</span>. </span>
                ) : (
                  <span> and all its data. </span>
                )}
                This action cannot be undone.
              </p>
            </div>
            <div className="flex gap-2 justify-end">
              <button
                onClick={() => setConfirmDeleteDb(null)}
                disabled={isDeleting}
                className="px-3 py-1.5 text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
              >
                Cancel
              </button>
              <button
                onClick={handleConfirmDelete}
                disabled={isDeleting || isLoadingStats}
                className="px-3 py-1.5 text-xs bg-red-600 hover:bg-red-700 text-white rounded transition-colors disabled:opacity-50"
              >
                {isDeleting ? 'Deleting...' : 'Delete'}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}

export function SettingsPage() {
  const viewMode = useUIStore(s => s.viewMode);
  const goBack = useUIStore(s => s.goBack);

  const settings = useSettingsStore(s => s.settings);
  const fetchSettings = useSettingsStore(s => s.fetchSettings);
  const setSetting = useSettingsStore(s => s.setSetting);
  const testOpenRouterConnection = useSettingsStore(s => s.testOpenRouterConnection);

  // Theme & Font
  const [theme, setTheme] = useState<Theme>('obsidian');
  const [font, setFont] = useState<Font>('ibm-plex-sans');

  // Provider selection
  const [provider, setProvider] = useState<'openrouter' | 'ollama' | 'openai_compat'>('openrouter');

  // OpenRouter settings
  const [apiKey, setApiKey] = useState('');
  const [showApiKey, setShowApiKey] = useState(false);
  const [isTesting, setIsTesting] = useState(false);
  const [testResult, setTestResult] = useState<'success' | 'error' | null>(null);
  const [testError, setTestError] = useState<string | null>(null);

  const [openrouterContextLength, setOpenrouterContextLength] = useState('');

  // OpenAI Compatible settings
  const [openaiCompatBaseUrl, setOpenaiCompatBaseUrl] = useState('');
  const [openaiCompatApiKey, setOpenaiCompatApiKey] = useState('');
  const [openaiCompatShowApiKey, setOpenaiCompatShowApiKey] = useState(false);
  const [openaiCompatEmbeddingModel, setOpenaiCompatEmbeddingModel] = useState('');
  const [openaiCompatEmbeddingDimension, setOpenaiCompatEmbeddingDimension] = useState('1536');
  const [openaiCompatLlmModel, setOpenaiCompatLlmModel] = useState('');
  const [openaiCompatContextLength, setOpenaiCompatContextLength] = useState('65536');
  const [openaiCompatTimeoutSecs, setOpenaiCompatTimeoutSecs] = useState('300');
  const [openaiCompatStatus, setOpenaiCompatStatus] = useState<'idle' | 'checking' | 'connected' | 'error'>('idle');
  const [openaiCompatError, setOpenaiCompatError] = useState<string | null>(null);

  // Ollama settings
  const [ollamaHost, setOllamaHost] = useState('http://127.0.0.1:11434');
  const [ollamaStatus, setOllamaStatus] = useState<'checking' | 'connected' | 'disconnected'>('checking');
  const [ollamaError, setOllamaError] = useState<string | undefined>();
  const [ollamaModels, setOllamaModels] = useState<OllamaModel[]>([]);
  const [ollamaEmbeddingModel, setOllamaEmbeddingModel] = useState('nomic-embed-text');
  const [ollamaLlmModel, setOllamaLlmModel] = useState('llama3.2');
  const [ollamaContextLength, setOllamaContextLength] = useState('65536');
  const [ollamaTimeoutSecs, setOllamaTimeoutSecs] = useState('120');
  const [isLoadingOllamaModels, setIsLoadingOllamaModels] = useState(false);

  // Common settings
  const [autoTaggingEnabled, setAutoTaggingEnabled] = useState(true);
  const [embeddingModel, setEmbeddingModel] = useState('openai/text-embedding-3-small');
  const [taggingModel, setTaggingModel] = useState('openai/gpt-4o-mini');
  const [wikiModel, setWikiModel] = useState('anthropic/claude-sonnet-4.5');
  const [wikiStrategy, setWikiStrategy] = useState('centroid');
  const [chatModel, setChatModel] = useState('anthropic/claude-sonnet-4.5');
  const [saveError, setSaveError] = useState<string | null>(null);

  // Re-embedding confirmation
  const [pendingEmbeddingChange, setPendingEmbeddingChange] = useState<{ key: string; value: string; label: string } | null>(null);

  // OpenRouter model loading
  const [availableModels, setAvailableModels] = useState<AvailableModel[]>([]);
  const [isLoadingModels, setIsLoadingModels] = useState(false);

  // Import state
  const [isImporting, setIsImporting] = useState(false);
  const [importResult, setImportResult] = useState<ImportResult | null>(null);
  const [importError, setImportError] = useState<string | null>(null);

  // Tab state
  const [activeTab, setActiveTab] = useState<SettingsTab>('general');

  // MCP setup state
  const [showMcpSetup, setShowMcpSetup] = useState(false);
  const [mcpConfig, setMcpConfig] = useState<McpConfig | null>(null);
  const [mcpConfigCopied, setMcpConfigCopied] = useState(false);

  // Remote server state
  const [serverUrl, setServerUrl] = useState('');
  const [serverToken, setServerToken] = useState('');
  const [isTestingServer, setIsTestingServer] = useState(false);
  const [serverTestResult, setServerTestResult] = useState<'success' | 'error' | null>(null);
  const [serverTestError, setServerTestError] = useState<string | null>(null);
  const [showChangeServer, setShowChangeServer] = useState(false);

  // API Token management state
  const [apiTokens, setApiTokens] = useState<ApiTokenInfo[]>([]);
  const [isLoadingTokens, setIsLoadingTokens] = useState(false);
  const [newTokenName, setNewTokenName] = useState('');
  const [isCreatingToken, setIsCreatingToken] = useState(false);
  const [createdToken, setCreatedToken] = useState<CreateTokenResponse | null>(null);
  const [tokenCopied, setTokenCopied] = useState(false);
  const [showTokenSection, setShowTokenSection] = useState(false);
  const [confirmRevokeId, setConfirmRevokeId] = useState<string | null>(null);

  // Feeds state
  const [feeds, setFeeds] = useState<Feed[]>([]);
  const [feedsLoading, setFeedsLoading] = useState(false);
  const [newFeedUrl, setNewFeedUrl] = useState('');
  const [addingFeed, setAddingFeed] = useState(false);
  const [feedError, setFeedError] = useState<string | null>(null);

  // Ingest URL state
  const [ingestUrlValue, setIngestUrlValue] = useState('');
  const [ingesting, setIngesting] = useState(false);
  const [ingestResult, setIngestResult] = useState<IngestionResult | null>(null);
  const [ingestError, setIngestError] = useState<string | null>(null);

  // Re-embed state
  const [showReembedConfirm, setShowReembedConfirm] = useState(false);
  const [reembedding, setReembedding] = useState(false);
  const [reembedResult, setReembedResult] = useState<number | null>(null);
  const [reembedError, setReembedError] = useState<string | null>(null);

  // Dimension change confirmation (typed "RE-EMBED" required)
  const [dimensionChangeInfo, setDimensionChangeInfo] = useState<{
    old_dim: number;
    new_dim: number;
    total_atom_count: number;
  } | null>(null);
  const [reembedConfirmText, setReembedConfirmText] = useState('');

  // Feed action state
  const [pollingFeedId, setPollingFeedId] = useState<string | null>(null);
  const [pollResult, setPollResult] = useState<FeedPollResult | null>(null);
  const [deletingFeedId, setDeletingFeedId] = useState<string | null>(null);

  // Derived: whether we're connected to a remote (non-local) server
  const isRemoteMode = isDesktopApp() ? !isLocalServer() : true;

  const isOpen = viewMode === 'settings';

  // Check Ollama connection
  const checkOllamaConnection = useCallback(async (host: string) => {
    setOllamaStatus('checking');
    setOllamaError(undefined);
    try {
      const connected = await testOllamaConnection(host);
      if (connected) {
        setOllamaStatus('connected');
        setIsLoadingOllamaModels(true);
        const models = await getOllamaModels(host);
        setOllamaModels(models);
        setIsLoadingOllamaModels(false);
      } else {
        setOllamaStatus('disconnected');
        setOllamaError('Could not connect to Ollama');
      }
    } catch (e) {
      setOllamaStatus('disconnected');
      setOllamaError(String(e));
      setIsLoadingOllamaModels(false);
    }
  }, []);

  // Test remote server connection
  const handleTestServer = async () => {
    if (!serverUrl.trim() || !serverToken.trim()) return;
    setIsTestingServer(true);
    setServerTestResult(null);
    setServerTestError(null);
    try {
      const resp = await fetch(`${serverUrl.trim().replace(/\/$/, '')}/health`);
      if (resp.ok) {
        setServerTestResult('success');
      } else {
        setServerTestResult('error');
        setServerTestError(`Server returned ${resp.status}`);
      }
    } catch (e) {
      setServerTestResult('error');
      setServerTestError(String(e));
    } finally {
      setIsTestingServer(false);
    }
  };

  const fetchAtoms = useAtomsStore((state) => state.fetchAtoms);
  const fetchTags = useTagsStore((state) => state.fetchTags);

  const handleConnectServer = async () => {
    try {
      await switchTransport({ baseUrl: serverUrl.trim().replace(/\/$/, ''), authToken: serverToken.trim() });
      setShowChangeServer(false);
      fetchSettings();
      fetchAtoms();
      fetchTags();
    } catch (e) {
      setServerTestResult('error');
      setServerTestError(String(e));
    }
  };

  const handleDisconnectServer = async () => {
    try {
      await switchToLocal();
      fetchSettings();
      fetchAtoms();
      fetchTags();
    } catch (e) {
      console.error('Failed to switch to local:', e);
      toast.error('Failed to switch to local server', { description: String(e) });
    }
  };

  // Load API tokens for remote mode
  const loadApiTokens = useCallback(async () => {
    setIsLoadingTokens(true);
    try {
      const tokens = await listApiTokens();
      setApiTokens(tokens);
    } catch (e) {
      console.error('Failed to load API tokens:', e);
      toast.error('Failed to load API tokens', { description: String(e) });
    } finally {
      setIsLoadingTokens(false);
    }
  }, []);

  // Create new API token
  const handleCreateToken = async () => {
    if (!newTokenName.trim() || isCreatingToken) return;
    setIsCreatingToken(true);
    try {
      const result = await createApiToken(newTokenName.trim());
      setCreatedToken(result);
      setNewTokenName('');
      setTokenCopied(false);
      await loadApiTokens();
    } catch (e) {
      console.error('Failed to create token:', e);
      toast.error('Failed to create API token', { description: String(e) });
    } finally {
      setIsCreatingToken(false);
    }
  };

  // Revoke an API token
  const handleRevokeToken = async (tokenId: string) => {
    const currentPrefix = serverToken.substring(0, 10);
    const tokenToRevoke = apiTokens.find(t => t.id === tokenId);
    const isCurrentToken = tokenToRevoke && tokenToRevoke.token_prefix === currentPrefix;

    try {
      await revokeApiToken(tokenId);
      if (isCurrentToken) {
        localStorage.removeItem('atomic-server-config');
        window.location.reload();
        return;
      }
      await loadApiTokens();
    } catch (e) {
      console.error('Failed to revoke token:', e);
      toast.error('Failed to revoke token', { description: String(e) });
    } finally {
      setConfirmRevokeId(null);
    }
  };

  // Load feeds
  const loadFeeds = useCallback(async () => {
    setFeedsLoading(true);
    setFeedError(null);
    try {
      const result = await listFeeds();
      setFeeds(result);
    } catch (e) {
      console.error('Failed to load feeds:', e);
      toast.error('Failed to load feeds', { description: String(e) });
      setFeedError(String(e));
    } finally {
      setFeedsLoading(false);
    }
  }, []);

  // Ingest a single URL
  const handleIngestUrl = async () => {
    if (!ingestUrlValue.trim() || ingesting) return;
    setIngesting(true);
    setIngestResult(null);
    setIngestError(null);
    try {
      const result = await ingestUrl(ingestUrlValue.trim());
      setIngestResult(result);
      setIngestUrlValue('');
    } catch (e) {
      setIngestError(String(e));
    } finally {
      setIngesting(false);
    }
  };

  // Add a new feed
  const handleAddFeed = async () => {
    if (!newFeedUrl.trim() || addingFeed) return;
    setAddingFeed(true);
    setFeedError(null);
    try {
      await createFeed(newFeedUrl.trim());
      setNewFeedUrl('');
      await loadFeeds();
    } catch (e) {
      setFeedError(String(e));
    } finally {
      setAddingFeed(false);
    }
  };

  // Poll a feed
  const handlePollFeed = async (feedId: string) => {
    setPollingFeedId(feedId);
    setPollResult(null);
    try {
      const result = await pollFeed(feedId);
      setPollResult(result);
      await loadFeeds();
    } catch (e) {
      setFeedError(String(e));
    } finally {
      setPollingFeedId(null);
    }
  };

  // Toggle feed pause/resume
  const handleToggleFeedPause = async (feed: Feed) => {
    try {
      await updateFeed(feed.id, { isPaused: !feed.is_paused });
      await loadFeeds();
    } catch (e) {
      setFeedError(String(e));
    }
  };

  // Delete a feed
  const handleDeleteFeed = async (feedId: string) => {
    setDeletingFeedId(feedId);
    try {
      await deleteFeed(feedId);
      await loadFeeds();
    } catch (e) {
      setFeedError(String(e));
    } finally {
      setDeletingFeedId(null);
    }
  };

  // Copy text to clipboard, with fallback for non-secure contexts (HTTP)
  const copyToClipboard = async (text: string) => {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
    } else {
      const textarea = document.createElement('textarea');
      textarea.value = text;
      textarea.style.position = 'fixed';
      textarea.style.opacity = '0';
      document.body.appendChild(textarea);
      textarea.select();
      document.execCommand('copy');
      document.body.removeChild(textarea);
    }
  };

  // Copy created token to clipboard
  const handleCopyToken = async () => {
    if (!createdToken) return;
    try {
      await copyToClipboard(createdToken.token);
      setTokenCopied(true);
      setTimeout(() => setTokenCopied(false), 2000);
    } catch (e) {
      console.error('Failed to copy:', e);
    }
  };

  // Load data when settings page becomes active
  useEffect(() => {
    if (isOpen) {
      // Load saved server config, defaulting to current origin
      const saved = localStorage.getItem('atomic-server-config');
      if (saved) {
        const config: HttpTransportConfig = JSON.parse(saved);
        setServerUrl(config.baseUrl);
        setServerToken(config.authToken);
      } else {
        setServerUrl(window.location.origin);
      }
      // Only fetch settings/models if transport is actually connected
      const transport = getTransport();
      if (transport.isConnected()) {
        fetchSettings();
        // Fetch OpenRouter models
        setIsLoadingModels(true);
        getAvailableLlmModels()
          .then(models => setAvailableModels(models))
          .catch(err => { console.error('Failed to load models:', err); toast.error('Failed to load models', { description: String(err) }); })
          .finally(() => setIsLoadingModels(false));
      }
      // Load API tokens when connected to a non-local server
      if (!isLocalServer() && transport.isConnected()) {
        loadApiTokens();
      }
      // Reset token creation state
      setCreatedToken(null);
      setTokenCopied(false);
      setShowTokenSection(false);
      setConfirmRevokeId(null);
    }
  }, [isOpen, fetchSettings, loadApiTokens]);

  // Load feeds when feeds tab is active
  useEffect(() => {
    if (isOpen && activeTab === 'feeds' && getTransport().isConnected()) {
      loadFeeds();
      setIngestResult(null);
      setIngestError(null);
      setPollResult(null);
      setFeedError(null);
    }
  }, [isOpen, activeTab, loadFeeds]);

  // Load settings into state
  useEffect(() => {
    const p = settings.provider as 'openrouter' | 'ollama' | 'openai_compat' | undefined;
    setTheme((settings.theme as Theme) || 'obsidian');
    setFont((settings.font as Font) || 'ibm-plex-sans');
    setProvider(p || 'openrouter');
    setApiKey(settings.openrouter_api_key || '');
    setOpenrouterContextLength(settings.openrouter_context_length || '');
    setAutoTaggingEnabled(settings.auto_tagging_enabled !== 'false');
    setEmbeddingModel(settings.embedding_model || 'openai/text-embedding-3-small');
    setTaggingModel(settings.tagging_model || 'openai/gpt-4o-mini');
    setWikiModel(settings.wiki_model || 'anthropic/claude-sonnet-4.5');
    setWikiStrategy(settings.wiki_strategy || 'centroid');
    setChatModel(settings.chat_model || 'anthropic/claude-sonnet-4.5');
    setOllamaHost(settings.ollama_host || 'http://127.0.0.1:11434');
    setOllamaEmbeddingModel(settings.ollama_embedding_model || 'nomic-embed-text');
    setOllamaLlmModel(settings.ollama_llm_model || 'llama3.2');
    setOllamaContextLength(settings.ollama_context_length || '65536');
    setOllamaTimeoutSecs(settings.ollama_timeout_secs || '120');
    setOpenaiCompatBaseUrl(settings.openai_compat_base_url || '');
    setOpenaiCompatApiKey(settings.openai_compat_api_key || '');
    setOpenaiCompatEmbeddingModel(settings.openai_compat_embedding_model || '');
    setOpenaiCompatEmbeddingDimension(settings.openai_compat_embedding_dimension || '1536');
    setOpenaiCompatLlmModel(settings.openai_compat_llm_model || '');
    setOpenaiCompatContextLength(settings.openai_compat_context_length || '65536');
    setOpenaiCompatTimeoutSecs(settings.openai_compat_timeout_secs || '300');
  }, [settings]);

  // Check Ollama connection when provider is ollama or host changes (debounced)
  useEffect(() => {
    if (!isOpen || provider !== 'ollama') return;
    const timer = setTimeout(() => {
      checkOllamaConnection(normalizeUrl(ollamaHost));
    }, 800);
    return () => clearTimeout(timer);
  }, [isOpen, provider, ollamaHost, checkOllamaConnection]);

  // ESC key to go back
  useEffect(() => {
    const handleEscape = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        goBack();
      }
    };

    if (isOpen) {
      document.addEventListener('keydown', handleEscape);
    }

    return () => {
      document.removeEventListener('keydown', handleEscape);
    };
  }, [isOpen, goBack]);

  // Auto-save a single setting
  const autoSave = useCallback(async (key: string, value: string) => {
    try {
      const result = await setSetting(key, value);
      if (result?.dimension_changed) {
        setDimensionChangeInfo({
          old_dim: result.old_dim,
          new_dim: result.new_dim,
          total_atom_count: result.total_atom_count,
        });
        setReembedConfirmText('');
      }
      if (result?.retried_failed_count > 0) {
        setSaveError(null);
        setReembedResult(result.retried_failed_count);
        setTimeout(() => setReembedResult(null), 5000);
      }
    } catch (e) {
      console.error(`Failed to save ${key}:`, e);
      setSaveError(`Failed to save setting`);
      setTimeout(() => setSaveError(null), 3000);
    }
  }, [setSetting]);

  // Handle changes that trigger re-embedding
  const handleEmbeddingModelChange = (value: string) => {
    setPendingEmbeddingChange({ key: 'embedding_model', value, label: value.split('/').pop() || value });
  };

  const handleOllamaEmbeddingModelChange = (value: string) => {
    setPendingEmbeddingChange({ key: 'ollama_embedding_model', value, label: value });
  };

  const handleOpenaiCompatEmbeddingModelChange = (value: string) => {
    setPendingEmbeddingChange({ key: 'openai_compat_embedding_model', value, label: value });
  };

  const handleOpenaiCompatEmbeddingDimensionChange = (value: string) => {
    setPendingEmbeddingChange({ key: 'openai_compat_embedding_dimension', value, label: `${value} dimensions` });
  };

  const confirmEmbeddingChange = async () => {
    if (!pendingEmbeddingChange) return;
    const { key, value } = pendingEmbeddingChange;
    if (key === 'embedding_model') setEmbeddingModel(value);
    if (key === 'ollama_embedding_model') setOllamaEmbeddingModel(value);
    if (key === 'openai_compat_embedding_model') setOpenaiCompatEmbeddingModel(value);
    if (key === 'openai_compat_embedding_dimension') setOpenaiCompatEmbeddingDimension(value);
    await autoSave(key, value);
    setPendingEmbeddingChange(null);
  };

  const cancelEmbeddingChange = () => {
    setPendingEmbeddingChange(null);
  };

  // Test OpenAI Compatible connection
  const checkOpenaiCompatConnection = useCallback(async (baseUrl: string, apiKey?: string) => {
    if (!baseUrl.trim()) return;
    setOpenaiCompatStatus('checking');
    setOpenaiCompatError(null);
    try {
      await testOpenAICompatConnection(baseUrl, apiKey || undefined);
      setOpenaiCompatStatus('connected');
    } catch (e) {
      setOpenaiCompatStatus('error');
      setOpenaiCompatError(String(e));
    }
  }, []);

  // Handle provider change
  const handleProviderChange = async (value: 'openrouter' | 'ollama' | 'openai_compat') => {
    setProvider(value);
    await autoSave('provider', value);
    if (value === 'openrouter' && apiKey.trim()) {
      setIsTesting(true);
      setTestResult(null);
      setTestError(null);
      try {
        await testOpenRouterConnection(apiKey);
        setTestResult('success');
      } catch (e) {
        setTestResult('error');
        setTestError(String(e));
      } finally {
        setIsTesting(false);
      }
    } else if (value === 'openai_compat' && openaiCompatBaseUrl.trim()) {
      checkOpenaiCompatConnection(openaiCompatBaseUrl, openaiCompatApiKey || undefined);
    }
  };

  // API key handlers
  const handleApiKeyChange = (value: string) => {
    setApiKey(value);
    setTestResult(null);
    setTestError(null);
  };

  const handleApiKeyBlur = async () => {
    if (!apiKey.trim()) return;
    await autoSave('openrouter_api_key', apiKey);
    setIsTesting(true);
    setTestResult(null);
    setTestError(null);
    try {
      await testOpenRouterConnection(apiKey);
      setTestResult('success');
    } catch (e) {
      setTestResult('error');
      setTestError(String(e));
    } finally {
      setIsTesting(false);
    }
  };

  // Handle MCP setup expand
  const handleMcpExpand = () => {
    const newState = !showMcpSetup;
    setShowMcpSetup(newState);
    if (newState && !mcpConfig) {
      const transport = getTransport() as import('../../lib/transport/http').HttpTransport;
      const config = getMcpConfig(transport.getConfig().baseUrl);
      setMcpConfig(config);
    }
  };

  // Copy MCP config to clipboard
  const handleCopyMcpConfig = async () => {
    if (!mcpConfig) return;
    try {
      await copyToClipboard(JSON.stringify(mcpConfig, null, 2));
      setMcpConfigCopied(true);
      setTimeout(() => setMcpConfigCopied(false), 2000);
    } catch (e) {
      console.error('Failed to copy:', e);
    }
  };

  // Handle Obsidian import
  const handleObsidianImport = async () => {
    setImportResult(null);
    setImportError(null);

    try {
      const selected = await pickDirectory('Select Obsidian Vault');
      if (!selected) return;

      setIsImporting(true);
      const result = await importObsidianVault(selected);
      setImportResult(result);

      if (result.imported > 0) {
        await Promise.all([fetchAtoms(), fetchTags()]);
      }
    } catch (e) {
      setImportError(String(e));
    } finally {
      setIsImporting(false);
    }
  };

  // Get Ollama embedding models
  const ollamaEmbeddingModels: AvailableModel[] = ollamaModels
    .filter(m => m.is_embedding)
    .map(m => ({ id: m.id, name: m.name }));

  // Get Ollama LLM models
  const ollamaLlmModels: AvailableModel[] = ollamaModels
    .filter(m => !m.is_embedding)
    .map(m => ({ id: m.id, name: m.name }));

  if (!isOpen) return null;

  return (
    <div className="flex h-full bg-[var(--color-bg-main)]">
      {/* Left navigation sidebar */}
      <nav className="w-52 shrink-0 border-r border-[var(--color-border)] bg-[var(--color-bg-panel)] flex flex-col">
        {/* Back button */}
        <div className="h-[52px] flex items-center px-3 flex-shrink-0">
          <button
            onClick={goBack}
            className="flex items-center gap-2 text-sm text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors px-2 py-1.5 rounded-md hover:bg-[var(--color-bg-hover)]"
          >
            <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 19l-7-7 7-7" />
            </svg>
            Back
          </button>
        </div>

        {/* Tab list */}
        <div className="flex-1 px-2 py-1 space-y-0.5">
          {SETTINGS_TABS.map((tab) => (
            <button
              key={tab.id}
              onClick={() => setActiveTab(tab.id)}
              className={`w-full flex items-center gap-2.5 px-3 py-2 text-sm rounded-md transition-colors ${
                activeTab === tab.id
                  ? 'bg-[var(--color-bg-hover)] text-[var(--color-text-primary)] font-medium'
                  : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'
              }`}
            >
              {tab.icon}
              {tab.label}
            </button>
          ))}
        </div>
      </nav>

      {/* Content area */}
      <div className="flex-1 overflow-y-auto p-8 relative">
        <div className="max-w-3xl mx-auto space-y-6">
              {/* ===== GENERAL TAB ===== */}
              {activeTab === 'general' && (
                <>
                  {/* Theme Selector */}
                  <div className="space-y-2">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      Theme
                    </label>
                    <CustomSelect
                      value={theme}
                      onChange={(v) => { setTheme(v as Theme); autoSave('theme', v); }}
                      options={THEMES}
                    />
                  </div>

                  {/* Font Selector */}
                  <div className="space-y-2">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      Font
                    </label>
                    <CustomSelect
                      value={font}
                      onChange={(v) => { setFont(v as Font); autoSave('font', v); }}
                      options={FONTS}
                    />
                  </div>

                  {/* Auto-tagging Toggle Section */}
                  <div className="flex items-center justify-between">
                    <div className="space-y-1">
                      <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                        Automatic Tag Extraction
                      </label>
                      <p className="text-xs text-[var(--color-text-secondary)]">
                        Automatically suggest tags when creating atoms
                      </p>
                    </div>
                    <button
                      type="button"
                      role="switch"
                      aria-checked={autoTaggingEnabled}
                      onClick={() => { const next = !autoTaggingEnabled; setAutoTaggingEnabled(next); autoSave('auto_tagging_enabled', next ? 'true' : 'false'); }}
                      className={`relative inline-flex h-6 w-11 flex-shrink-0 cursor-pointer rounded-full border-2 border-transparent transition-colors duration-200 ease-in-out focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:ring-offset-2 focus:ring-offset-[var(--color-bg-panel)] ${
                        autoTaggingEnabled ? 'bg-[var(--color-accent)]' : 'bg-[var(--color-bg-hover)]'
                      }`}
                    >
                      <span
                        className={`pointer-events-none inline-block h-5 w-5 transform rounded-full bg-white shadow ring-0 transition duration-200 ease-in-out ${
                          autoTaggingEnabled ? 'translate-x-5' : 'translate-x-0'
                        }`}
                      />
                    </button>
                  </div>

                  {/* Troubleshooting */}
                  <div className="space-y-2 pt-4 border-t border-[var(--color-border)]">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      Troubleshooting
                    </label>
                    <p className="text-xs text-[var(--color-text-secondary)]">
                      Export recent server logs to help diagnose issues
                    </p>
                    <Button
                      onClick={async () => {
                        try {
                          const logs = await exportLogs();
                          const date = new Date().toISOString().split('T')[0];
                          const blob = new Blob([logs], { type: 'text/plain' });
                          const url = URL.createObjectURL(blob);
                          const a = document.createElement('a');
                          a.href = url;
                          a.download = `atomic-logs-${date}.txt`;
                          a.click();
                          URL.revokeObjectURL(url);
                          toast.success('Logs exported');
                        } catch (e) {
                          toast.error('Failed to export logs', { description: String(e) });
                        }
                      }}
                      variant="secondary"
                    >
                      Export Logs
                    </Button>
                  </div>
                </>
              )}

              {/* ===== AI MODELS TAB ===== */}
              {activeTab === 'ai' && (
                <>
                  {/* Provider Selector */}
                  <div className="space-y-2">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      AI Provider
                    </label>
                    <p className="text-xs text-[var(--color-text-secondary)]">
                      Choose your AI provider
                    </p>
                    <CustomSelect
                      value={provider}
                      onChange={(v) => handleProviderChange(v as 'openrouter' | 'ollama' | 'openai_compat')}
                      options={[
                        { value: 'openrouter', label: 'OpenRouter' },
                        { value: 'ollama', label: 'Ollama' },
                        { value: 'openai_compat', label: 'OpenAI Compatible' },
                      ]}
                    />
                    {/* Connection status */}
                    {provider === 'openrouter' && isTesting && (
                      <div className="flex items-center gap-2 text-sm text-[var(--color-text-secondary)]">
                        <svg className="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
                          <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                          <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                        </svg>
                        Testing connection...
                      </div>
                    )}
                    {provider === 'openrouter' && !isTesting && testResult === 'success' && (
                      <div className="flex items-center gap-2 text-sm text-green-500">
                        <div className="w-2 h-2 rounded-full bg-green-500" />
                        Connected
                      </div>
                    )}
                    {provider === 'openrouter' && !isTesting && testResult === 'error' && (
                      <div className="flex items-center gap-2 text-sm text-red-500">
                        <div className="w-2 h-2 rounded-full bg-red-500" />
                        {testError || 'Connection failed'}
                      </div>
                    )}
                  </div>

                  {/* OpenRouter Settings */}
                  {provider === 'openrouter' && (
                    <>
                      <div className="space-y-2">
                        <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                          OpenRouter API Key
                        </label>
                        <p className="text-xs text-[var(--color-text-secondary)]">
                          Required for AI features. Get your key at openrouter.ai
                        </p>
                        <div className="relative">
                          <input
                            type={showApiKey ? 'text' : 'password'}
                            value={apiKey}
                            onChange={(e) => handleApiKeyChange(e.target.value)}
                            onBlur={handleApiKeyBlur}
                            placeholder="sk-or-..."
                            className="w-full px-3 py-2 pr-10 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150"
                          />
                          <button
                            type="button"
                            onClick={() => setShowApiKey(!showApiKey)}
                            className="absolute right-2 top-1/2 -translate-y-1/2 text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
                          >
                            {showApiKey ? (
                              <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M13.875 18.825A10.05 10.05 0 0112 19c-4.478 0-8.268-2.943-9.543-7a9.97 9.97 0 011.563-3.029m5.858.908a3 3 0 114.243 4.243M9.878 9.878l4.242 4.242M9.88 9.88l-3.29-3.29m7.532 7.532l3.29 3.29M3 3l3.59 3.59m0 0A9.953 9.953 0 0112 5c4.478 0 8.268 2.943 9.543 7a10.025 10.025 0 01-4.132 5.411m0 0L21 21" />
                              </svg>
                            ) : (
                              <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M2.458 12C3.732 7.943 7.523 5 12 5c4.478 0 8.268 2.943 9.542 7-1.274 4.057-5.064 7-9.542 7-4.477 0-8.268-2.943-9.542-7z" />
                              </svg>
                            )}
                          </button>
                        </div>
                      </div>

                      {/* Model Configuration for OpenRouter */}
                      <div className="space-y-4 pt-2">
                        <div className="text-sm font-medium text-[var(--color-text-primary)]">Model Configuration</div>
                        <p className="text-xs text-[var(--color-text-secondary)]">
                          Select models for different AI tasks.
                        </p>

                        {/* Embedding Model */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Embedding Model
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Used for semantic search
                          </p>
                          <CustomSelect
                            value={embeddingModel}
                            onChange={handleEmbeddingModelChange}
                            options={[
                              { value: 'openai/text-embedding-3-small', label: 'text-embedding-3-small (1536 dim)' },
                              { value: 'openai/text-embedding-3-large', label: 'text-embedding-3-large (3072 dim)' },
                            ]}
                          />
                        </div>

                        {/* Tagging Model */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Tagging Model
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Used for automatic tag extraction
                          </p>
                          <SearchableSelect
                            value={taggingModel}
                            onChange={(v) => { setTaggingModel(v); autoSave('tagging_model', v); }}
                            options={availableModels}
                            isLoading={isLoadingModels}
                            placeholder="Select tagging model..."
                          />
                        </div>

                        {/* Wiki Model */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Wiki Model
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Used for wiki article generation
                          </p>
                          <SearchableSelect
                            value={wikiModel}
                            onChange={(v) => { setWikiModel(v); autoSave('wiki_model', v); }}
                            options={availableModels}
                            isLoading={isLoadingModels}
                            placeholder="Select wiki model..."
                          />
                        </div>

                        {/* Wiki Strategy */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Wiki Strategy
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            How source material is selected for wiki articles
                          </p>
                          <CustomSelect
                            value={wikiStrategy}
                            onChange={(v) => { setWikiStrategy(v); autoSave('wiki_strategy', v); }}
                            options={[
                              { value: 'centroid', label: 'Centroid — rank chunks by embedding similarity' },
                              { value: 'agentic', label: 'Agentic — AI agent searches and curates sources' },
                            ]}
                          />
                        </div>

                        {/* Chat Model */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Chat Model
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Used for conversational AI assistant
                          </p>
                          <SearchableSelect
                            value={chatModel}
                            onChange={(v) => { setChatModel(v); autoSave('chat_model', v); }}
                            options={availableModels}
                            isLoading={isLoadingModels}
                            placeholder="Select chat model..."
                          />
                        </div>

                        {/* Context Length */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Context Length
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Override context window limit (default: use model's max)
                          </p>
                          <CustomSelect
                            value={openrouterContextLength}
                            onChange={(v) => { setOpenrouterContextLength(v); autoSave('openrouter_context_length', v); }}
                            options={[
                              { value: '', label: 'Model default' },
                              { value: '8192', label: '8K' },
                              { value: '16384', label: '16K' },
                              { value: '32768', label: '32K' },
                              { value: '65536', label: '64K' },
                              { value: '131072', label: '128K' },
                              { value: '262144', label: '256K' },
                              { value: '1000000', label: '1M' },
                            ]}
                          />
                        </div>
                      </div>
                    </>
                  )}

                  {/* Ollama Settings */}
                  {provider === 'ollama' && (
                    <>
                      <div className="space-y-2">
                        <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                          Ollama Server URL
                        </label>
                        <p className="text-xs text-[var(--color-text-secondary)]">
                          URL of your local Ollama server (default: http://127.0.0.1:11434)
                        </p>
                        <input
                          type="text"
                          value={ollamaHost}
                          onChange={(e) => setOllamaHost(e.target.value)}
                          onBlur={() => autoSave('ollama_host', normalizeUrl(ollamaHost))}
                          placeholder="http://127.0.0.1:11434"
                          className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150"
                        />
                        <ConnectionStatus status={ollamaStatus} error={ollamaError} />
                      </div>

                      {ollamaStatus === 'connected' && (
                        <div className="space-y-4">
                          {/* Ollama Embedding Model */}
                          <div className="space-y-1">
                            <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                              Embedding Model
                            </label>
                            <p className="text-xs text-[var(--color-text-secondary)]">
                              Used for semantic search. Pull nomic-embed-text if not available.
                            </p>
                            {ollamaEmbeddingModels.length > 0 ? (
                              <SearchableSelect
                                value={ollamaEmbeddingModel}
                                onChange={handleOllamaEmbeddingModelChange}
                                options={ollamaEmbeddingModels}
                                isLoading={isLoadingOllamaModels}
                                placeholder="Select embedding model..."
                              />
                            ) : (
                              <div className="px-3 py-2 bg-[var(--color-bg-card)] border border-amber-500/50 rounded-md text-sm text-amber-400">
                                No embedding models found. Run: ollama pull nomic-embed-text
                              </div>
                            )}
                          </div>

                          {/* Ollama LLM Model */}
                          <div className="space-y-1">
                            <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                              LLM Model
                            </label>
                            <p className="text-xs text-[var(--color-text-secondary)]">
                              Used for tagging, wiki generation, and chat
                            </p>
                            {ollamaLlmModels.length > 0 ? (
                              <SearchableSelect
                                value={ollamaLlmModel}
                                onChange={(v) => { setOllamaLlmModel(v); autoSave('ollama_llm_model', v); }}
                                options={ollamaLlmModels}
                                isLoading={isLoadingOllamaModels}
                                placeholder="Select LLM model..."
                              />
                            ) : (
                              <div className="px-3 py-2 bg-[var(--color-bg-card)] border border-amber-500/50 rounded-md text-sm text-amber-400">
                                No LLM models found. Run: ollama pull llama3.2
                              </div>
                            )}
                          </div>

                          {/* Context Length */}
                          <div className="space-y-1">
                            <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                              Context Length
                            </label>
                            <p className="text-xs text-[var(--color-text-secondary)]">
                              Max context window of your LLM model (used to truncate prompts)
                            </p>
                            <CustomSelect
                              value={ollamaContextLength}
                              onChange={(v) => { setOllamaContextLength(v); autoSave('ollama_context_length', v); }}
                              options={[
                                { value: '2048', label: '2K' },
                                { value: '4096', label: '4K' },
                                { value: '8192', label: '8K' },
                                { value: '16384', label: '16K' },
                                { value: '32768', label: '32K' },
                                { value: '65536', label: '64K' },
                                { value: '131072', label: '128K' },
                                { value: '262144', label: '256K' },
                                { value: '1000000', label: '1M' },
                              ]}
                            />
                          </div>

                          {/* Timeout */}
                          <div className="space-y-1">
                            <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                              Request Timeout
                            </label>
                            <p className="text-xs text-[var(--color-text-secondary)]">
                              Maximum time to wait for Ollama to respond
                            </p>
                            <CustomSelect
                              value={ollamaTimeoutSecs}
                              onChange={(v) => { setOllamaTimeoutSecs(v); autoSave('ollama_timeout_secs', v); }}
                              options={[
                                { value: '30', label: '30 seconds' },
                                { value: '60', label: '60 seconds' },
                                { value: '120', label: '2 minutes' },
                                { value: '180', label: '3 minutes' },
                                { value: '300', label: '5 minutes' },
                                { value: '600', label: '10 minutes' },
                              ]}
                            />
                          </div>
                        </div>
                      )}

                      {ollamaStatus === 'disconnected' && (
                        <div className="p-4 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md space-y-2">
                          <p className="text-sm text-[var(--color-text-primary)]">Make sure Ollama is running:</p>
                          <ol className="text-xs text-[var(--color-text-secondary)] space-y-1 list-decimal list-inside">
                            <li>Install Ollama from ollama.com</li>
                            <li>Start Ollama (it runs in the background)</li>
                            <li>Pull required models: ollama pull llama3.2 && ollama pull nomic-embed-text</li>
                          </ol>
                          <Button
                            variant="secondary"
                            onClick={() => checkOllamaConnection(ollamaHost)}
                            className="mt-2"
                          >
                            Retry Connection
                          </Button>
                        </div>
                      )}
                    </>
                  )}

                  {/* OpenAI Compatible Settings */}
                  {provider === 'openai_compat' && (
                    <>
                      <div className="space-y-2">
                        <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                          Base URL
                        </label>
                        <p className="text-xs text-[var(--color-text-secondary)]">
                          OpenAI-compatible API endpoint (e.g. http://localhost:8080/v1)
                        </p>
                        <input
                          type="text"
                          value={openaiCompatBaseUrl}
                          onChange={(e) => setOpenaiCompatBaseUrl(e.target.value)}
                          onBlur={() => {
                            autoSave('openai_compat_base_url', openaiCompatBaseUrl);
                            if (openaiCompatBaseUrl.trim()) {
                              checkOpenaiCompatConnection(openaiCompatBaseUrl, openaiCompatApiKey || undefined);
                            }
                          }}
                          placeholder="http://localhost:8080/v1"
                          className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150"
                        />
                        {openaiCompatStatus === 'checking' && (
                          <div className="flex items-center gap-2 text-sm text-[var(--color-text-secondary)]">
                            <svg className="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
                              <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                              <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                            </svg>
                            Testing connection...
                          </div>
                        )}
                        {openaiCompatStatus === 'connected' && (
                          <div className="flex items-center gap-2 text-sm text-green-500">
                            <div className="w-2 h-2 rounded-full bg-green-500" />
                            Connected
                          </div>
                        )}
                        {openaiCompatStatus === 'error' && (
                          <div className="flex items-center gap-2 text-sm text-red-500">
                            <div className="w-2 h-2 rounded-full bg-red-500" />
                            {openaiCompatError || 'Connection failed'}
                          </div>
                        )}
                      </div>

                      <div className="space-y-2">
                        <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                          API Key
                        </label>
                        <p className="text-xs text-[var(--color-text-secondary)]">
                          Optional. Required if your server uses Bearer token auth.
                        </p>
                        <div className="relative">
                          <input
                            type={openaiCompatShowApiKey ? 'text' : 'password'}
                            value={openaiCompatApiKey}
                            onChange={(e) => setOpenaiCompatApiKey(e.target.value)}
                            onBlur={() => autoSave('openai_compat_api_key', openaiCompatApiKey)}
                            placeholder="sk-..."
                            className="w-full px-3 py-2 pr-10 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150"
                          />
                          <button
                            type="button"
                            onClick={() => setOpenaiCompatShowApiKey(!openaiCompatShowApiKey)}
                            className="absolute right-2 top-1/2 -translate-y-1/2 text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
                          >
                            {openaiCompatShowApiKey ? (
                              <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M13.875 18.825A10.05 10.05 0 0112 19c-4.478 0-8.268-2.943-9.543-7a9.97 9.97 0 011.563-3.029m5.858.908a3 3 0 114.243 4.243M9.878 9.878l4.242 4.242M9.88 9.88l-3.29-3.29m7.532 7.532l3.29 3.29M3 3l3.59 3.59m0 0A9.953 9.953 0 0112 5c4.478 0 8.268 2.943 9.543 7a10.025 10.025 0 01-4.132 5.411m0 0L21 21" />
                              </svg>
                            ) : (
                              <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
                                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M2.458 12C3.732 7.943 7.523 5 12 5c4.478 0 8.268 2.943 9.542 7-1.274 4.057-5.064 7-9.542 7-4.477 0-8.268-2.943-9.542-7z" />
                              </svg>
                            )}
                          </button>
                        </div>
                      </div>

                      <div className="space-y-4 pt-2">
                        <div className="text-sm font-medium text-[var(--color-text-primary)]">Model Configuration</div>
                        <p className="text-xs text-[var(--color-text-secondary)]">
                          Enter the exact model names your server expects.
                        </p>

                        {/* Embedding Model */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Embedding Model
                          </label>
                          <input
                            type="text"
                            value={openaiCompatEmbeddingModel}
                            onChange={(e) => setOpenaiCompatEmbeddingModel(e.target.value)}
                            onBlur={() => {
                              if (openaiCompatEmbeddingModel !== (settings.openai_compat_embedding_model || '')) {
                                handleOpenaiCompatEmbeddingModelChange(openaiCompatEmbeddingModel);
                              }
                            }}
                            placeholder="text-embedding-3-small"
                            className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150"
                          />
                        </div>

                        {/* Embedding Dimension */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Embedding Dimension
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Vector dimension of your embedding model (e.g. 1536 for text-embedding-3-small)
                          </p>
                          <input
                            type="number"
                            value={openaiCompatEmbeddingDimension}
                            onChange={(e) => setOpenaiCompatEmbeddingDimension(e.target.value)}
                            onBlur={() => {
                              if (openaiCompatEmbeddingDimension !== (settings.openai_compat_embedding_dimension || '1536')) {
                                handleOpenaiCompatEmbeddingDimensionChange(openaiCompatEmbeddingDimension);
                              }
                            }}
                            placeholder="1536"
                            className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150"
                          />
                        </div>

                        {/* LLM Model */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            LLM Model
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Used for tagging, wiki generation, and chat
                          </p>
                          <input
                            type="text"
                            value={openaiCompatLlmModel}
                            onChange={(e) => setOpenaiCompatLlmModel(e.target.value)}
                            onBlur={() => autoSave('openai_compat_llm_model', openaiCompatLlmModel)}
                            placeholder="meta-llama/Llama-3.1-8B-Instruct"
                            className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150"
                          />
                        </div>

                        {/* Context Length */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Context Length
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Max context window of your LLM model (used to truncate prompts)
                          </p>
                          <CustomSelect
                            value={openaiCompatContextLength}
                            onChange={(v) => { setOpenaiCompatContextLength(v); autoSave('openai_compat_context_length', v); }}
                            options={[
                              { value: '2048', label: '2K' },
                              { value: '4096', label: '4K' },
                              { value: '8192', label: '8K' },
                              { value: '16384', label: '16K' },
                              { value: '32768', label: '32K' },
                              { value: '65536', label: '64K' },
                              { value: '131072', label: '128K' },
                              { value: '262144', label: '256K' },
                              { value: '1000000', label: '1M' },
                            ]}
                          />
                        </div>

                        {/* Timeout */}
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Request Timeout
                          </label>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Maximum time to wait for the server to respond
                          </p>
                          <CustomSelect
                            value={openaiCompatTimeoutSecs}
                            onChange={(v) => { setOpenaiCompatTimeoutSecs(v); autoSave('openai_compat_timeout_secs', v); }}
                            options={[
                              { value: '30', label: '30 seconds' },
                              { value: '60', label: '60 seconds' },
                              { value: '120', label: '2 minutes' },
                              { value: '180', label: '3 minutes' },
                              { value: '300', label: '5 minutes' },
                              { value: '600', label: '10 minutes' },
                            ]}
                          />
                        </div>
                      </div>
                    </>
                  )}
                  {/* Dimension Change Confirmation */}
                  {dimensionChangeInfo && (
                    <div className="p-4 bg-red-500/10 border border-red-500/40 rounded-md space-y-3">
                      <div className="flex items-center gap-2">
                        <svg className="w-5 h-5 text-red-400 shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z" />
                        </svg>
                        <span className="text-red-400 font-medium text-sm">Embedding Dimensions Changed</span>
                      </div>
                      <p className="text-sm text-[var(--color-text-secondary)]">
                        Dimensions changed from <strong className="text-[var(--color-text-primary)]">{dimensionChangeInfo.old_dim}</strong> → <strong className="text-[var(--color-text-primary)]">{dimensionChangeInfo.new_dim}</strong>.
                        This will re-embed <strong className="text-[var(--color-text-primary)]">{dimensionChangeInfo.total_atom_count}</strong> atoms.
                        Existing search results will be unavailable until re-embedding completes.
                      </p>
                      <div className="space-y-2">
                        <label className="block text-xs text-[var(--color-text-secondary)]">
                          Type <span className="font-mono font-bold text-red-400">RE-EMBED</span> to confirm:
                        </label>
                        <input
                          type="text"
                          value={reembedConfirmText}
                          onChange={(e) => setReembedConfirmText(e.target.value)}
                          placeholder="RE-EMBED"
                          className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] font-mono placeholder-[var(--color-text-secondary)]/40 focus:outline-none focus:ring-2 focus:ring-red-500/50 focus:border-transparent"
                          autoFocus
                        />
                      </div>
                      <div className="flex gap-2">
                        <Button
                          onClick={async () => {
                            setDimensionChangeInfo(null);
                            setReembedConfirmText('');
                            setReembedding(true);
                            try {
                              const count = await reembedAllAtoms();
                              setReembedResult(count);
                            } catch (e) {
                              setReembedError(String(e));
                            } finally {
                              setReembedding(false);
                            }
                          }}
                          disabled={reembedConfirmText !== 'RE-EMBED'}
                        >
                          Confirm Re-embed
                        </Button>
                        <Button
                          variant="secondary"
                          onClick={() => { setDimensionChangeInfo(null); setReembedConfirmText(''); }}
                        >
                          Skip (keep existing embeddings)
                        </Button>
                      </div>
                    </div>
                  )}

                  {/* Re-embed All Section */}
                  <div className="space-y-3 pt-4 border-t border-[var(--color-border)]">
                    <div className="space-y-1">
                      <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                        Re-embed All Atoms
                      </label>
                      <p className="text-xs text-[var(--color-text-secondary)]">
                        Regenerate embeddings for every atom in the current database. Useful after changing providers or if embeddings were interrupted.
                      </p>
                    </div>

                    {!showReembedConfirm ? (
                      <Button
                        variant="secondary"
                        onClick={() => { setShowReembedConfirm(true); setReembedResult(null); setReembedError(null); }}
                        disabled={reembedding}
                      >
                        Re-embed All Atoms
                      </Button>
                    ) : (
                      <div className="p-3 bg-yellow-500/10 border border-yellow-500/30 rounded-md space-y-3">
                        <p className="text-sm text-yellow-200">
                          This will re-embed <strong>all</strong> atoms in the current database. This is a bulk operation that may take a while depending on how many atoms you have and your provider's rate limits.
                        </p>
                        <div className="flex gap-2">
                          <Button
                            onClick={async () => {
                              setReembedding(true);
                              setShowReembedConfirm(false);
                              setReembedResult(null);
                              setReembedError(null);
                              try {
                                const count = await reembedAllAtoms();
                                setReembedResult(count);
                              } catch (e) {
                                setReembedError(String(e));
                              } finally {
                                setReembedding(false);
                              }
                            }}
                            disabled={reembedding}
                          >
                            {reembedding ? (
                              <>
                                <svg className="w-4 h-4 animate-spin mr-1" fill="none" viewBox="0 0 24 24">
                                  <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                                  <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                                </svg>
                                Starting...
                              </>
                            ) : 'Confirm Re-embed'}
                          </Button>
                          <Button
                            variant="secondary"
                            onClick={() => setShowReembedConfirm(false)}
                          >
                            Cancel
                          </Button>
                        </div>
                      </div>
                    )}

                    {reembedResult !== null && (
                      <div className="p-3 bg-green-500/10 border border-green-500/30 rounded-md text-sm">
                        <div className="text-green-400 font-medium">Queued {reembedResult} atoms for re-embedding</div>
                        <div className="text-[var(--color-text-secondary)]">Embeddings are being generated in the background.</div>
                      </div>
                    )}

                    {reembedError && (
                      <div className="p-3 bg-red-500/10 border border-red-500/30 rounded-md text-sm">
                        <div className="text-red-400 font-medium">Re-embedding failed</div>
                        <div className="text-[var(--color-text-secondary)]">{reembedError}</div>
                      </div>
                    )}
                  </div>
                </>
              )}

              {/* ===== CONNECTION TAB ===== */}
              {activeTab === 'connection' && (
                <>
                  {/* Connect to Server Section — desktop + local server */}
                  {isDesktopApp() && isLocalServer() && (
                    <div className="space-y-3">
                      <div className="flex items-center justify-between">
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Server
                          </label>
                          <p className="text-xs text-green-500 flex items-center gap-1.5">
                            <span className="inline-block w-2 h-2 rounded-full bg-green-500" />
                            Local
                          </p>
                        </div>
                        <Button variant="secondary" onClick={() => setShowChangeServer(!showChangeServer)}>
                          {showChangeServer ? 'Cancel' : 'Connect to Custom Server'}
                        </Button>
                      </div>
                      {showChangeServer && (
                      <div className="space-y-3 pt-2">
                      <p className="text-xs text-[var(--color-text-secondary)]">
                        Connect to a remote atomic-server instance
                      </p>
                      <input
                        type="text"
                        value={serverUrl}
                        onChange={(e) => { setServerUrl(e.target.value); setServerTestResult(null); }}
                        placeholder="http://localhost:8080"
                        className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
                      />
                      <input
                        type="password"
                        value={serverToken}
                        onChange={(e) => { setServerToken(e.target.value); setServerTestResult(null); }}
                        placeholder="Auth token"
                        className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
                      />
                      <div className="flex gap-2">
                        <Button variant="secondary" onClick={handleTestServer} disabled={!serverUrl.trim() || !serverToken.trim() || isTestingServer}>
                          {isTestingServer ? 'Testing...' : 'Test'}
                        </Button>
                        <Button onClick={handleConnectServer} disabled={serverTestResult !== 'success'}>
                          Connect
                        </Button>
                      </div>
                      {serverTestResult === 'success' && (
                        <div className="text-sm text-green-500">Server reachable</div>
                      )}
                      {serverTestResult === 'error' && (
                        <div className="text-sm text-red-500">{serverTestError}</div>
                      )}
                      </div>
                      )}
                    </div>
                  )}

                  {/* Connected to remote — show status with change/disconnect options */}
                  {isRemoteMode && getTransport().isConnected() && (
                    <div className="space-y-3">
                      <div className="flex items-center justify-between">
                        <div className="space-y-1">
                          <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                            Remote Server
                          </label>
                          <p className="text-xs text-green-500 flex items-center gap-1.5">
                            <span className="inline-block w-2 h-2 rounded-full bg-green-500" />
                            Connected to {serverUrl}
                          </p>
                        </div>
                        <div className="flex gap-2">
                          <Button variant="secondary" onClick={() => setShowChangeServer(!showChangeServer)}>
                            {showChangeServer ? 'Cancel' : 'Change'}
                          </Button>
                          {isDesktopApp() ? (
                            <Button variant="secondary" onClick={handleDisconnectServer}>
                              Switch to Local
                            </Button>
                          ) : (
                            <Button variant="secondary" onClick={() => {
                              localStorage.removeItem('atomic-server-config');
                              window.location.reload();
                            }}>
                              Log Out
                            </Button>
                          )}
                        </div>
                      </div>
                      {showChangeServer && (
                        <div className="space-y-3 pt-2">
                          <input
                            type="text"
                            value={serverUrl}
                            onChange={(e) => { setServerUrl(e.target.value); setServerTestResult(null); }}
                            placeholder="http://localhost:8080"
                            className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
                          />
                          <input
                            type="password"
                            value={serverToken}
                            onChange={(e) => { setServerToken(e.target.value); setServerTestResult(null); }}
                            placeholder="Auth token"
                            className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
                          />
                          <div className="flex gap-2">
                            <Button variant="secondary" onClick={handleTestServer} disabled={!serverUrl.trim() || !serverToken.trim() || isTestingServer}>
                              {isTestingServer ? 'Testing...' : 'Test'}
                            </Button>
                            <Button onClick={handleConnectServer} disabled={serverTestResult !== 'success'}>
                              Reconnect
                            </Button>
                          </div>
                          {serverTestResult === 'success' && (
                            <div className="text-sm text-green-500">Server reachable</div>
                          )}
                          {serverTestResult === 'error' && (
                            <div className="text-sm text-red-500">{serverTestError}</div>
                          )}
                        </div>
                      )}
                    </div>
                  )}

                  {/* API Tokens Section — remote/web only */}
                  {!isLocalServer() && getTransport().isConnected() && (
                    <div className="space-y-3 pt-4 border-t border-[var(--color-border)]">
                      <button
                        type="button"
                        onClick={() => setShowTokenSection(!showTokenSection)}
                        className="flex items-center gap-2 text-sm font-medium text-[var(--color-text-primary)] hover:text-white transition-colors w-full"
                      >
                        <svg
                          className={`w-4 h-4 transition-transform ${showTokenSection ? 'rotate-90' : ''}`}
                          fill="none"
                          stroke="currentColor"
                          viewBox="0 0 24 24"
                        >
                          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 5l7 7-7 7" />
                        </svg>
                        API Tokens
                        {apiTokens.filter(t => !t.is_revoked).length > 0 && (
                          <span className="text-xs text-[var(--color-text-secondary)]">
                            ({apiTokens.filter(t => !t.is_revoked).length} active)
                          </span>
                        )}
                      </button>

                      {showTokenSection && (
                        <div className="space-y-4 pl-6 border-l-2 border-[var(--color-border)]">
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Manage API tokens for accessing this server. Each device or integration should use its own token.
                          </p>

                          {/* Token list */}
                          {isLoadingTokens ? (
                            <div className="flex items-center gap-2 text-sm text-[var(--color-text-secondary)]">
                              <svg className="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
                                <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                              </svg>
                              Loading tokens...
                            </div>
                          ) : apiTokens.length === 0 ? (
                            <div className="text-sm text-[var(--color-text-secondary)]">No tokens found.</div>
                          ) : (
                            <div className="space-y-2">
                              {apiTokens.filter(t => !t.is_revoked).map((token) => {
                                const isCurrentToken = token.token_prefix === serverToken.substring(0, 10);
                                return (
                                  <div
                                    key={token.id}
                                    className={`p-3 bg-[var(--color-bg-card)] border rounded-md text-sm ${
                                      isCurrentToken ? 'border-green-500/50' : 'border-[var(--color-border)]'
                                    }`}
                                  >
                                    <div className="flex items-center justify-between">
                                      <div className="flex items-center gap-2">
                                        <span className="font-medium text-[var(--color-text-primary)]">{token.name}</span>
                                        {isCurrentToken && (
                                          <span className="text-xs px-1.5 py-0.5 rounded bg-green-500/20 text-green-400">current</span>
                                        )}
                                      </div>
                                      {confirmRevokeId === token.id ? (
                                        <div className="flex items-center gap-2">
                                          <span className="text-xs text-amber-400">
                                            {isCurrentToken ? 'This will log you out!' : 'Revoke?'}
                                          </span>
                                          <button
                                            onClick={() => handleRevokeToken(token.id)}
                                            className="text-xs px-2 py-1 rounded bg-red-500/20 text-red-400 hover:bg-red-500/30 transition-colors"
                                          >
                                            Confirm
                                          </button>
                                          <button
                                            onClick={() => setConfirmRevokeId(null)}
                                            className="text-xs px-2 py-1 rounded text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
                                          >
                                            Cancel
                                          </button>
                                        </div>
                                      ) : (
                                        <button
                                          onClick={() => setConfirmRevokeId(token.id)}
                                          className="text-xs text-red-400 hover:text-red-300 transition-colors"
                                        >
                                          Revoke
                                        </button>
                                      )}
                                    </div>
                                    <div className="flex items-center gap-3 mt-1 text-xs text-[var(--color-text-secondary)]">
                                      <span className="font-mono">{token.token_prefix}...</span>
                                      <span>Created {new Date(token.created_at).toLocaleDateString()}</span>
                                      {token.last_used_at && (
                                        <span>Last used {new Date(token.last_used_at).toLocaleDateString()}</span>
                                      )}
                                    </div>
                                  </div>
                                );
                              })}
                            </div>
                          )}

                          {/* Created token display */}
                          {createdToken && (
                            <div className="p-3 bg-amber-500/10 border border-amber-500/30 rounded-md space-y-2">
                              <div className="text-sm font-medium text-amber-400">
                                Token created — save it now, it won't be shown again
                              </div>
                              <div className="flex items-center gap-2">
                                <code className="flex-1 text-xs font-mono bg-[var(--color-bg-main)] px-2 py-1.5 rounded border border-[var(--color-border)] text-[var(--color-text-primary)] break-all select-all">
                                  {createdToken.token}
                                </code>
                                <button
                                  onClick={handleCopyToken}
                                  className="p-1.5 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors flex-shrink-0"
                                  title="Copy to clipboard"
                                >
                                  {tokenCopied ? (
                                    <svg className="w-4 h-4 text-green-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
                                    </svg>
                                  ) : (
                                    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z" />
                                    </svg>
                                  )}
                                </button>
                              </div>
                            </div>
                          )}

                          {/* Create new token */}
                          <div className="flex gap-2">
                            <input
                              type="text"
                              value={newTokenName}
                              onChange={(e) => setNewTokenName(e.target.value)}
                              onKeyDown={(e) => { if (e.key === 'Enter') handleCreateToken(); }}
                              placeholder="Token name (e.g. laptop, phone)"
                              className="flex-1 px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
                            />
                            <Button
                              variant="secondary"
                              onClick={handleCreateToken}
                              disabled={!newTokenName.trim() || isCreatingToken}
                            >
                              {isCreatingToken ? 'Creating...' : 'Create'}
                            </Button>
                          </div>
                        </div>
                      )}
                    </div>
                  )}

                </>
              )}

              {/* ===== FEEDS TAB ===== */}
              {activeTab === 'feeds' && (
                <>
                  {/* Ingest URL Section */}
                  <div className="space-y-3">
                    <div className="space-y-1">
                      <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                        Ingest URL
                      </label>
                      <p className="text-xs text-[var(--color-text-secondary)]">
                        Extract and save an article from any web page
                      </p>
                    </div>
                    <div className="flex gap-2">
                      <input
                        type="url"
                        value={ingestUrlValue}
                        onChange={(e) => { setIngestUrlValue(e.target.value); setIngestResult(null); setIngestError(null); }}
                        onKeyDown={(e) => { if (e.key === 'Enter') handleIngestUrl(); }}
                        placeholder="https://example.com/article"
                        className="flex-1 px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
                      />
                      <Button onClick={handleIngestUrl} disabled={!ingestUrlValue.trim() || ingesting}>
                        {ingesting ? (
                          <>
                            <svg className="w-4 h-4 animate-spin mr-1" fill="none" viewBox="0 0 24 24">
                              <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                              <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                            </svg>
                            Ingesting...
                          </>
                        ) : 'Ingest'}
                      </Button>
                    </div>

                    {ingestResult && (
                      <div className="p-3 bg-green-500/10 border border-green-500/30 rounded-md text-sm">
                        <div className="text-green-400 font-medium mb-1">Added to knowledge base</div>
                        <div className="text-[var(--color-text-secondary)]">{ingestResult.title}</div>
                      </div>
                    )}

                    {ingestError && (
                      <div className="p-3 bg-red-500/10 border border-red-500/30 rounded-md text-sm">
                        <div className="text-red-400 font-medium mb-1">Ingestion failed</div>
                        <div className="text-[var(--color-text-secondary)]">{ingestError}</div>
                      </div>
                    )}
                  </div>

                  {/* RSS Feeds Section */}
                  <div className="space-y-3 pt-4 border-t border-[var(--color-border)]">
                    <div className="space-y-1">
                      <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                        RSS Feeds
                      </label>
                      <p className="text-xs text-[var(--color-text-secondary)]">
                        Subscribe to RSS feeds to automatically ingest new articles
                      </p>
                    </div>

                    {/* Poll result banner */}
                    {pollResult && (
                      <div className="p-3 bg-green-500/10 border border-green-500/30 rounded-md text-sm">
                        <div className="text-green-400 font-medium mb-1">Poll complete</div>
                        <div className="text-[var(--color-text-secondary)]">
                          {pollResult.new_items} new{pollResult.skipped > 0 && `, ${pollResult.skipped} skipped`}{pollResult.errors > 0 && `, ${pollResult.errors} errors`}
                        </div>
                      </div>
                    )}

                    {feedError && (
                      <div className="p-3 bg-red-500/10 border border-red-500/30 rounded-md text-sm">
                        <div className="text-red-400 font-medium mb-1">Error</div>
                        <div className="text-[var(--color-text-secondary)]">{feedError}</div>
                      </div>
                    )}

                    {/* Feed list */}
                    {feedsLoading ? (
                      <div className="flex items-center gap-2 text-sm text-[var(--color-text-secondary)] py-4">
                        <svg className="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
                          <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                          <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                        </svg>
                        Loading feeds...
                      </div>
                    ) : feeds.length === 0 ? (
                      <div className="text-sm text-[var(--color-text-secondary)] py-4">
                        No feeds yet. Add an RSS feed URL below to get started.
                      </div>
                    ) : (
                      <div className="space-y-2">
                        {feeds.map((feed) => (
                          <div
                            key={feed.id}
                            className="p-3 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md space-y-2"
                          >
                            <div className="flex items-start justify-between gap-2">
                              <div className="min-w-0 flex-1">
                                <div className="flex items-center gap-2">
                                  <span className="text-sm font-medium text-[var(--color-text-primary)] truncate">
                                    {feed.title || feed.url}
                                  </span>
                                  {feed.is_paused && (
                                    <span className="px-1.5 py-0.5 text-xs rounded bg-yellow-500/20 text-yellow-400">
                                      Paused
                                    </span>
                                  )}
                                </div>
                                {feed.title && (
                                  <div className="text-xs text-[var(--color-text-secondary)] truncate mt-0.5">
                                    {feed.url}
                                  </div>
                                )}
                                <div className="text-xs text-[var(--color-text-secondary)] mt-1">
                                  Every {feed.poll_interval}m
                                  {feed.last_polled_at && (
                                    <> · Polled {formatRelativeDate(feed.last_polled_at)}</>
                                  )}
                                </div>
                                {feed.last_error && (
                                  <div className="text-xs text-red-400 mt-1 truncate" title={feed.last_error}>
                                    {feed.last_error}
                                  </div>
                                )}
                              </div>
                              <div className="flex items-center gap-1 flex-shrink-0">
                                <button
                                  type="button"
                                  onClick={() => handlePollFeed(feed.id)}
                                  disabled={pollingFeedId === feed.id}
                                  className="p-1.5 text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors disabled:opacity-50"
                                  title="Poll now"
                                >
                                  {pollingFeedId === feed.id ? (
                                    <svg className="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
                                      <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                                      <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                                    </svg>
                                  ) : (
                                    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                                    </svg>
                                  )}
                                </button>
                                <button
                                  type="button"
                                  onClick={() => handleToggleFeedPause(feed)}
                                  className="p-1.5 text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                                  title={feed.is_paused ? 'Resume' : 'Pause'}
                                >
                                  {feed.is_paused ? (
                                    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M14.752 11.168l-3.197-2.132A1 1 0 0010 9.87v4.263a1 1 0 001.555.832l3.197-2.132a1 1 0 000-1.664z" />
                                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
                                    </svg>
                                  ) : (
                                    <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M10 9v6m4-6v6m7-3a9 9 0 11-18 0 9 9 0 0118 0z" />
                                    </svg>
                                  )}
                                </button>
                                <button
                                  type="button"
                                  onClick={() => handleDeleteFeed(feed.id)}
                                  disabled={deletingFeedId === feed.id}
                                  className="p-1.5 text-[var(--color-text-secondary)] hover:text-red-400 hover:bg-[var(--color-bg-hover)] rounded transition-colors disabled:opacity-50"
                                  title="Delete feed"
                                >
                                  <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                                  </svg>
                                </button>
                              </div>
                            </div>
                          </div>
                        ))}
                      </div>
                    )}

                    {/* Add feed form */}
                    <div className="flex gap-2 pt-2">
                      <input
                        type="url"
                        value={newFeedUrl}
                        onChange={(e) => { setNewFeedUrl(e.target.value); setFeedError(null); }}
                        onKeyDown={(e) => { if (e.key === 'Enter') handleAddFeed(); }}
                        placeholder="https://example.com/feed.xml"
                        className="flex-1 px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
                      />
                      <Button variant="secondary" onClick={handleAddFeed} disabled={!newFeedUrl.trim() || addingFeed}>
                        {addingFeed ? (
                          <>
                            <svg className="w-4 h-4 animate-spin mr-1" fill="none" viewBox="0 0 24 24">
                              <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                              <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                            </svg>
                            Adding...
                          </>
                        ) : 'Add Feed'}
                      </Button>
                    </div>
                  </div>
                </>
              )}

              {/* ===== INTEGRATIONS TAB ===== */}
              {activeTab === 'integrations' && (
                <>
                  {/* Import Section — desktop + local server only */}
                  {isDesktopApp() && isLocalServer() && (
                    <div className="space-y-3">
                      <div className="space-y-1">
                        <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                          Import Notes
                        </label>
                        <p className="text-xs text-[var(--color-text-secondary)]">
                          Import notes from other applications
                        </p>
                      </div>

                      <Button
                        variant="secondary"
                        onClick={handleObsidianImport}
                        disabled={isImporting}
                        className="w-full justify-center"
                      >
                        {isImporting ? (
                          <>
                            <svg className="w-4 h-4 animate-spin mr-2" fill="none" viewBox="0 0 24 24">
                              <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                              <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                            </svg>
                            Importing...
                          </>
                        ) : (
                          <>
                            <svg className="w-4 h-4 mr-2" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-8l-4-4m0 0L8 8m4-4v12" />
                            </svg>
                            Import from Obsidian
                          </>
                        )}
                      </Button>

                      {/* Import Result */}
                      {importResult && (
                        <div className="p-3 bg-green-500/10 border border-green-500/30 rounded-md text-sm">
                          <div className="text-green-400 font-medium mb-1">Import complete!</div>
                          <div className="text-[var(--color-text-secondary)] space-y-0.5">
                            <div>Imported: {importResult.imported} notes</div>
                            {importResult.tags_created > 0 && (
                              <div>Tags created: {importResult.tags_created}</div>
                            )}
                            {importResult.skipped > 0 && (
                              <div>Skipped: {importResult.skipped} (duplicates/empty)</div>
                            )}
                          </div>
                        </div>
                      )}

                      {/* Import Error */}
                      {importError && (
                        <div className="p-3 bg-red-500/10 border border-red-500/30 rounded-md text-sm">
                          <div className="text-red-400 font-medium mb-1">Import failed</div>
                          <div className="text-[var(--color-text-secondary)]">{importError}</div>
                        </div>
                      )}
                    </div>
                  )}

                  {/* MCP Server Setup Section */}
                  {getTransport().isConnected() && (
                    <div className="space-y-3">
                      <button
                        type="button"
                        onClick={handleMcpExpand}
                        className="flex items-center gap-2 text-sm font-medium text-[var(--color-text-primary)] hover:text-white transition-colors w-full"
                      >
                        <svg
                          className={`w-4 h-4 transition-transform ${showMcpSetup ? 'rotate-90' : ''}`}
                          fill="none"
                          stroke="currentColor"
                          viewBox="0 0 24 24"
                        >
                          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 5l7 7-7 7" />
                        </svg>
                        Claude Desktop Integration
                      </button>

                      {showMcpSetup && (
                        <div className="space-y-4 pl-6 border-l-2 border-[var(--color-border)]">
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Connect Atomic to Claude Desktop as an MCP server. This allows Claude to search and create notes in your knowledge base.
                          </p>

                          <div className="space-y-2">
                            <div className="text-sm font-medium text-[var(--color-text-primary)]">Setup Instructions</div>
                            <ol className="text-xs text-[var(--color-text-secondary)] space-y-2 list-decimal list-inside">
                              <li>
                                Open Claude Desktop settings
                                <span className="text-[var(--color-text-tertiary)]"> (Claude → Settings → Developer)</span>
                              </li>
                              <li>Click "Edit Config" to open claude_desktop_config.json</li>
                              <li>Add the following configuration:</li>
                            </ol>
                          </div>

                          {/* Config JSON */}
                          <div className="relative">
                            <pre className="p-3 bg-[var(--color-bg-main)] border border-[var(--color-border)] rounded-md text-xs text-[var(--color-text-primary)] overflow-x-auto">
                              {mcpConfig ? JSON.stringify(mcpConfig, null, 2) : 'Loading...'}
                            </pre>
                            <button
                              type="button"
                              onClick={handleCopyMcpConfig}
                              disabled={!mcpConfig}
                              className="absolute top-2 right-2 p-1.5 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors disabled:opacity-50"
                              title="Copy to clipboard"
                            >
                              {mcpConfigCopied ? (
                                <svg className="w-4 h-4 text-green-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                  <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
                                </svg>
                              ) : (
                                <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                  <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z" />
                                </svg>
                              )}
                            </button>
                          </div>

                          <ol start={4} className="text-xs text-[var(--color-text-secondary)] space-y-2 list-decimal list-inside">
                            <li>Save the config file and restart Claude Desktop</li>
                            <li>
                              Verify by checking for "atomic" in Claude's MCP servers
                              <span className="text-[var(--color-text-tertiary)]"> (hammer icon)</span>
                            </li>
                          </ol>

                          <div className="p-3 bg-green-500/10 border border-green-500/30 rounded-md text-xs text-green-400">
                            <strong>Note:</strong> The MCP server is served over HTTP by the Atomic server. The server must be running for Claude to access your notes.
                          </div>
                        </div>
                      )}
                    </div>
                  )}
                </>
              )}

              {/* ===== DATABASES TAB ===== */}
              {activeTab === 'databases' && (
                <DatabasesTab />
              )}
        </div>

        {saveError && (
          <div className="fixed bottom-4 right-4 z-50 px-4 py-3 bg-[var(--color-bg-panel)] border border-red-500/30 rounded-lg shadow-xl">
            <div className="flex items-start gap-2 text-sm text-red-500">
              <svg className="w-4 h-4 mt-0.5 flex-shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
              </svg>
              <span>{saveError}</span>
            </div>
          </div>
        )}

        {/* Re-embedding confirmation dialog */}
        {pendingEmbeddingChange && (
          <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/40">
            <div className="bg-[var(--color-bg-panel)] border border-[var(--color-border)] rounded-lg shadow-xl p-6 mx-8 max-w-sm space-y-4">
              <div className="space-y-2">
                <h3 className="text-sm font-semibold text-[var(--color-text-primary)]">Re-embed all atoms?</h3>
                <p className="text-xs text-[var(--color-text-secondary)]">
                  Changing the embedding model to <span className="font-medium text-[var(--color-text-primary)]">{pendingEmbeddingChange.label}</span> will
                  re-embed all atoms. This may take a while and will use API credits.
                </p>
              </div>
              <div className="flex justify-end gap-2">
                <Button variant="secondary" onClick={cancelEmbeddingChange}>Cancel</Button>
                <Button onClick={confirmEmbeddingChange}>Re-embed</Button>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
