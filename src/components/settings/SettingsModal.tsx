import { useState, useEffect, useRef, useCallback, useMemo } from 'react';
import { createPortal } from 'react-dom';
import { toast } from 'sonner';
import {
  X,
  Check,
  Copy,
  Eye,
  EyeOff,
  Loader2,
  Pause,
  Play,
  Pencil,
  RefreshCw,
  Trash2,
  Upload,
  Download,
  ChevronRight,
  AlertCircle,
} from 'lucide-react';
import { Button } from '../ui/Button';
import { CustomSelect } from '../ui/CustomSelect';
import { SearchableSelect } from '../ui/SearchableSelect';
import { ConnectionStatus } from '../ui/ConnectionStatus';
import { Modal } from '../ui/Modal';
import { useSettingsStore } from '../../stores/settings';
import { useAtomsStore } from '../../stores/atoms';
import { useTagsStore, type TagWithCount } from '../../stores/tags';
import { THEMES, Theme } from '../../hooks/useTheme';
import { FONTS, Font } from '../../hooks/useFont';
import {
  getAvailableLlmModels,
  getOpenRouterEmbeddingModels,
  testOllamaConnection,
  testOpenAICompatConnection,
  getOllamaModels,
  getMcpStdioConfig,
  getMcpHttpConfig,
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
  type OpenRouterEmbeddingModel,
  type OllamaModel,
  type ImportResult,
  type McpConfig,
  type ApiTokenInfo,
  type CreateTokenResponse,
  type Feed,
  getAllPipelineStatuses,
  retryFailedEmbeddings,
  retryFailedTagging,
  reembedAllAtoms,
  retagAllAtoms,
  exportDatabaseMarkdownArchive,
  type ExportJob,
  type DatabasePipelineStatus,
  exportLogs,
  type IngestionResult,
  type FeedPollResult,
} from '../../lib/api';
import { getTransport, switchTransport, switchToLocal, isDesktopApp, isLocalServer, getLocalServerConfig, getMcpBridgePath, isCloudTenant, type HttpTransportConfig } from '../../lib/transport';
import { pickDirectory, isMacOS, openExternalUrl } from '../../lib/platform';
import { importMarkdownFolder, type ImportProgress } from '../../lib/import';
import { importAppleNotes, AppleNotesImportError } from '../../lib/import-apple-notes';

/** macOS deep-link that opens the Full Disk Access pane in System Settings. */
const MACOS_FULL_DISK_ACCESS_URL =
  'x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles';
import { formatRelativeDate } from '../../lib/date';
import { getBrowserTimeZone, getSupportedTimeZones } from '../../lib/tz';
import { useDatabasesStore, type DatabaseInfo, type DatabaseStats } from '../../stores/databases';
import { OverrideControls } from './OverrideControls';

export type SettingsTab = 'general' | 'ai' | 'tag-categories' | 'connection' | 'integrations' | 'databases' | 'prompts';

const SETTINGS_TABS: { id: SettingsTab; label: string }[] = [
  { id: 'general', label: 'General' },
  { id: 'ai', label: 'AI Models' },
  { id: 'prompts', label: 'Prompts' },
  { id: 'tag-categories', label: 'Tags' },
  { id: 'connection', label: 'Connection' },
  { id: 'integrations', label: 'Integrations' },
  { id: 'databases', label: 'Databases' },
];

function TagCategoriesTab() {
  const tags = useTagsStore(s => s.tags);
  const fetchTags = useTagsStore(s => s.fetchTags);
  const setTagAutotagTarget = useTagsStore(s => s.setTagAutotagTarget);
  const setTagAutotagDescription = useTagsStore(s => s.setTagAutotagDescription);
  const createTag = useTagsStore(s => s.createTag);
  const [newName, setNewName] = useState('');
  const [creating, setCreating] = useState(false);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [expandedTagId, setExpandedTagId] = useState<string | null>(null);
  const [descriptionDrafts, setDescriptionDrafts] = useState<Record<string, string>>({});

  useEffect(() => {
    fetchTags();
  }, [fetchTags]);

  // Top-level tags only — flag is currently constrained to root tags.
  const topLevel = tags.filter(t => !t.parent_id);
  const targets = topLevel.filter(t => t.is_autotag_target);
  const available = topLevel.filter(t => !t.is_autotag_target);

  const handleToggle = async (id: string, value: boolean) => {
    setErrorMsg(null);
    try {
      await setTagAutotagTarget(id, value);
    } catch (e) {
      setErrorMsg(String(e));
    }
  };

  const handleCreate = async () => {
    const trimmed = newName.trim();
    if (!trimmed) return;
    if (trimmed.includes('/')) {
      setErrorMsg('Category names cannot contain "/".');
      return;
    }
    if (topLevel.some(t => t.name.toLowerCase() === trimmed.toLowerCase())) {
      setErrorMsg(`A top-level tag named "${trimmed}" already exists.`);
      return;
    }
    setCreating(true);
    setErrorMsg(null);
    try {
      const created = await createTag(trimmed);
      await setTagAutotagTarget(created.id, true);
      setNewName('');
    } catch (e) {
      setErrorMsg(String(e));
    } finally {
      setCreating(false);
    }
  };

  const handleDescriptionChange = (id: string, value: string) => {
    setDescriptionDrafts(current => ({ ...current, [id]: value }));
  };

  const handleDescriptionSave = async (tag: TagWithCount) => {
    const draft = descriptionDrafts[tag.id] ?? tag.autotag_description ?? '';
    if (draft === (tag.autotag_description ?? '')) return;
    setErrorMsg(null);
    try {
      await setTagAutotagDescription(tag.id, draft);
      setDescriptionDrafts(current => {
        const next = { ...current };
        delete next[tag.id];
        return next;
      });
    } catch (e) {
      setErrorMsg(String(e));
    }
  };

  return (
    <>
      <div className="space-y-1">
        <h3 className="text-sm font-medium text-[var(--color-text-primary)]">Auto-Tag Categories</h3>
        <p className="text-xs text-[var(--color-text-secondary)]">
          The AI auto-tagger only creates new sub-tags under categories you mark as targets.
        </p>
      </div>

      {targets.length === 0 && (
        <div className="rounded-lg border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-xs text-yellow-200">
          No auto-tag targets configured. Auto-tagging will be skipped for new atoms until you mark at least one category.
        </div>
      )}

      <div className="space-y-2">
        <div className="text-xs font-medium uppercase tracking-wide text-[var(--color-text-tertiary)]">Active targets</div>
        {targets.length === 0 ? (
          <p className="text-xs text-[var(--color-text-secondary)] italic">None yet.</p>
        ) : (
          <div className="space-y-1">
            {targets.map(tag => {
              const isExpanded = expandedTagId === tag.id;
              const description = tag.autotag_description ?? '';
              const draft = descriptionDrafts[tag.id] ?? description;

              return (
              <div
                key={tag.id}
                className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-main)]"
              >
                <div className="flex items-center justify-between gap-3 px-3 py-2">
                  <button
                    type="button"
                    onClick={() => setExpandedTagId(isExpanded ? null : tag.id)}
                    className="flex min-w-0 flex-1 items-center gap-2 text-left"
                    title={isExpanded ? 'Hide AI description' : 'Edit AI description'}
                  >
                    <ChevronRight
                      className={`h-4 w-4 flex-shrink-0 text-[var(--color-text-tertiary)] transition-transform ${isExpanded ? 'rotate-90' : ''}`}
                      strokeWidth={2}
                    />
                    <span className="text-sm text-[var(--color-text-primary)] truncate">{tag.name}</span>
                    <span className="text-[10px] text-[var(--color-text-tertiary)]">
                      {(tag as TagWithCount).atom_count} atoms
                    </span>
                    {description.trim() && (
                      <span className="text-[10px] text-[var(--color-accent)]">
                        Description
                      </span>
                    )}
                  </button>
                  <button
                    onClick={() => handleToggle(tag.id, false)}
                    className="px-2 py-1 text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                  >
                    Unflag
                  </button>
                </div>
                {isExpanded && (
                  <div className="border-t border-[var(--color-border)] px-3 pb-3 pt-2">
                    <label className="mb-1 block text-xs font-medium text-[var(--color-text-secondary)]">
                      AI description
                    </label>
                    <textarea
                      value={draft}
                      onChange={e => handleDescriptionChange(tag.id, e.target.value)}
                      onBlur={() => handleDescriptionSave(tag)}
                      placeholder="Tell the auto-tagger when this category should be used."
                      rows={3}
                      className="w-full resize-y rounded border border-[var(--color-border)] bg-[var(--color-bg-panel)] px-3 py-2 text-sm text-[var(--color-text-primary)] outline-none placeholder:text-[var(--color-text-secondary)]/50 focus:border-[var(--color-accent)]"
                    />
                  </div>
                )}
              </div>
            );
            })}
          </div>
        )}
      </div>

      <div className="space-y-2">
        <div className="text-xs font-medium uppercase tracking-wide text-[var(--color-text-tertiary)]">Available top-level tags</div>
        {available.length === 0 ? (
          <p className="text-xs text-[var(--color-text-secondary)] italic">All your top-level tags are already targets.</p>
        ) : (
          <div className="space-y-1">
            {available.map(tag => (
              <div
                key={tag.id}
                className="flex items-center justify-between gap-3 px-3 py-2 rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-main)]"
              >
                <div className="flex items-center gap-2 min-w-0">
                  <span className="text-sm text-[var(--color-text-primary)] truncate">{tag.name}</span>
                  <span className="text-[10px] text-[var(--color-text-tertiary)]">
                    {(tag as TagWithCount).atom_count} atoms
                  </span>
                </div>
                <button
                  onClick={() => handleToggle(tag.id, true)}
                  className="px-2 py-1 text-xs text-[var(--color-accent)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                >
                  Mark as target
                </button>
              </div>
            ))}
          </div>
        )}
      </div>

      <div className="space-y-2">
        <div className="text-xs font-medium uppercase tracking-wide text-[var(--color-text-tertiary)]">Create new target</div>
        <div className="flex gap-2">
          <input
            type="text"
            value={newName}
            onChange={e => setNewName(e.target.value)}
            onKeyDown={e => { if (e.key === 'Enter') handleCreate(); }}
            placeholder="e.g., Methodologies"
            disabled={creating}
            className="flex-1 bg-[var(--color-bg-main)] border border-[var(--color-border)] rounded px-3 py-1.5 text-sm text-[var(--color-text-primary)] outline-none focus:border-[var(--color-accent)]"
          />
          <Button onClick={handleCreate} disabled={creating || !newName.trim()}>
            {creating ? 'Adding…' : 'Add'}
          </Button>
        </div>
      </div>

      {errorMsg && (
        <div className="text-xs text-red-400">{errorMsg}</div>
      )}
    </>
  );
}

function pipelineSummary(status?: DatabasePipelineStatus['status']) {
  if (!status) {
    return {
      tone: 'muted' as const,
      label: 'Loading',
      detail: 'Checking AI pipeline state...',
    };
  }

  const failed = status.failed_count + status.tagging_failed_count;
  const pending = status.pending + status.tagging_pending;
  const processing = status.processing + status.tagging_processing;

  if (failed > 0) {
    const parts = [
      status.failed_count > 0 ? `${status.failed_count} embedding` : null,
      status.tagging_failed_count > 0 ? `${status.tagging_failed_count} tagging` : null,
    ].filter(Boolean);
    return {
      tone: 'error' as const,
      label: 'Needs attention',
      detail: `${parts.join(', ')} failed`,
    };
  }

  if (processing > 0 || pending > 0) {
    const parts = [
      pending > 0 ? `${pending} pending` : null,
      processing > 0 ? `${processing} processing` : null,
    ].filter(Boolean);
    return {
      tone: 'working' as const,
      label: 'Working',
      detail: parts.join(', '),
    };
  }

  return {
    tone: 'healthy' as const,
    label: 'Healthy',
    detail: `${status.complete} embedded · ${status.tagging_complete} tagged${status.tagging_skipped > 0 ? ` · ${status.tagging_skipped} skipped` : ''}`,
  };
}

function PipelineDetailCounts({ status }: { status: DatabasePipelineStatus['status'] }) {
  const cellClass = 'px-2 py-1.5 text-right tabular-nums';
  const labelClass = 'px-2 py-1.5 text-left font-medium text-[var(--color-text-secondary)]';

  return (
    <div className="overflow-x-auto rounded border border-[var(--color-border)] bg-[var(--color-bg-panel)]">
      <table className="w-full min-w-[560px] text-xs">
        <thead className="text-[var(--color-text-tertiary)]">
          <tr className="border-b border-[var(--color-border)]">
            <th className={labelClass}>Stage</th>
            <th className={cellClass}>Pending</th>
            <th className={cellClass}>Processing</th>
            <th className={cellClass}>Complete</th>
            <th className={cellClass}>Skipped</th>
            <th className={cellClass}>Failed</th>
          </tr>
        </thead>
        <tbody>
          <tr className="border-b border-[var(--color-border)]">
            <td className={labelClass}>Embeddings</td>
            <td className={cellClass}>{status.pending}</td>
            <td className={cellClass}>{status.processing}</td>
            <td className={cellClass}>{status.complete}</td>
            <td className={cellClass}>-</td>
            <td className={`${cellClass} ${status.failed_count > 0 ? 'text-red-300 font-medium' : ''}`}>{status.failed_count}</td>
          </tr>
          <tr>
            <td className={labelClass}>Tagging</td>
            <td className={cellClass}>{status.tagging_pending}</td>
            <td className={cellClass}>{status.tagging_processing}</td>
            <td className={cellClass}>{status.tagging_complete}</td>
            <td className={cellClass}>{status.tagging_skipped}</td>
            <td className={`${cellClass} ${status.tagging_failed_count > 0 ? 'text-red-300 font-medium' : ''}`}>{status.tagging_failed_count}</td>
          </tr>
        </tbody>
      </table>
    </div>
  );
}

function DatabasesTab() {
  const { databases, activeId, fetchDatabases, renameDatabase, deleteDatabase, setDefaultDatabase, getDatabaseStats } = useDatabasesStore();
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editName, setEditName] = useState('');
  const [confirmDeleteDb, setConfirmDeleteDb] = useState<DatabaseInfo | null>(null);
  const [deleteStats, setDeleteStats] = useState<DatabaseStats | null>(null);
  const [isLoadingStats, setIsLoadingStats] = useState(false);
  const [isDeleting, setIsDeleting] = useState(false);
  const [pipelineItems, setPipelineItems] = useState<DatabasePipelineStatus[]>([]);
  const [pipelineLoading, setPipelineLoading] = useState(false);
  const [pipelineError, setPipelineError] = useState<string | null>(null);
  const [retrying, setRetrying] = useState<string | null>(null);
  const [reembeddingDb, setReembeddingDb] = useState<string | null>(null);
  const [retaggingDb, setRetaggingDb] = useState<string | null>(null);
  const [exportingDb, setExportingDb] = useState<string | null>(null);
  const [exportJobsByDb, setExportJobsByDb] = useState<Record<string, ExportJob>>({});
  const [confirmReembedDb, setConfirmReembedDb] = useState<DatabaseInfo | null>(null);
  const [confirmRetagDb, setConfirmRetagDb] = useState<DatabaseInfo | null>(null);
  const [expandedPipeline, setExpandedPipeline] = useState<string | null>(null);
  const liveRefreshTimer = useRef<ReturnType<typeof setTimeout>>();

  const pipelineByDb = useMemo(() => {
    return new Map(pipelineItems.map(item => [item.database.id, item.status]));
  }, [pipelineItems]);

  useEffect(() => {
    fetchDatabases();
  }, [fetchDatabases]);

  const loadPipelineStatuses = useCallback(async (silent = false) => {
    if (!silent) setPipelineLoading(true);
    setPipelineError(null);
    try {
      setPipelineItems(await getAllPipelineStatuses());
    } catch (e) {
      setPipelineError(String(e));
    } finally {
      if (!silent) setPipelineLoading(false);
    }
  }, []);

  useEffect(() => {
    loadPipelineStatuses();
  }, [loadPipelineStatuses]);

  useEffect(() => {
    const transport = getTransport();
    const scheduleRefresh = () => {
      clearTimeout(liveRefreshTimer.current);
      liveRefreshTimer.current = setTimeout(() => {
        void loadPipelineStatuses(true);
      }, 600);
    };

    const events = [
      'atom-created',
      'atom-updated',
      'embedding-started',
      'embedding-complete',
      'tagging-complete',
      'embeddings-reset',
      'pipeline-queue-started',
      'pipeline-queue-progress',
      'pipeline-queue-completed',
    ];
    const unsubs = events.map(event => transport.subscribe(event, scheduleRefresh));

    return () => {
      clearTimeout(liveRefreshTimer.current);
      unsubs.forEach(unsub => unsub());
    };
  }, [loadPipelineStatuses]);

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

  const handleExportMarkdown = async (db: DatabaseInfo) => {
    if (exportingDb) return;
    setExportingDb(db.id);
    setPipelineError(null);
    try {
      const job = await exportDatabaseMarkdownArchive(db.id, progress => {
        setExportJobsByDb(current => ({ ...current, [db.id]: progress }));
      });
      toast.success(`Exported ${job.total_atoms} ${job.total_atoms === 1 ? 'atom' : 'atoms'} as markdown`);
    } catch (e) {
      toast.error('Failed to export database', { description: String(e) });
    } finally {
      setExportingDb(null);
    }
  };

  const retryFailed = async (dbId: string, stage: 'embedding' | 'tagging') => {
    const key = `${dbId}:${stage}`;
    setRetrying(key);
    setPipelineError(null);
    try {
      const count = stage === 'embedding'
        ? await retryFailedEmbeddings(dbId)
        : await retryFailedTagging(dbId);
      toast.success(`Queued ${count} failed ${stage === 'embedding' ? 'embedding' : 'tagging'} ${count === 1 ? 'job' : 'jobs'}`);
      await loadPipelineStatuses();
    } catch (e) {
      setPipelineError(String(e));
    } finally {
      setRetrying(null);
    }
  };

  const handleConfirmReembedAll = async () => {
    if (!confirmReembedDb || reembeddingDb) return;
    const db = confirmReembedDb;
    setReembeddingDb(db.id);
    setPipelineError(null);
    try {
      const count = await reembedAllAtoms(db.id);
      toast.success(`Queued ${count} ${count === 1 ? 'atom' : 'atoms'} for re-embedding`);
      setConfirmReembedDb(null);
      await loadPipelineStatuses();
    } catch (e) {
      setPipelineError(String(e));
    } finally {
      setReembeddingDb(null);
    }
  };

  const handleConfirmRetagAll = async () => {
    if (!confirmRetagDb || retaggingDb) return;
    const db = confirmRetagDb;
    setRetaggingDb(db.id);
    setPipelineError(null);
    try {
      const count = await retagAllAtoms(db.id);
      toast.success(`Queued ${count} ${count === 1 ? 'atom' : 'atoms'} for re-tagging`);
      setConfirmRetagDb(null);
      await loadPipelineStatuses();
    } catch (e) {
      setPipelineError(String(e));
    } finally {
      setRetaggingDb(null);
    }
  };

  return (
    <>
      <div className="space-y-1">
        <h3 className="text-sm font-medium text-[var(--color-text-primary)]">Manage Databases</h3>
        <p className="text-xs text-[var(--color-text-secondary)]">
          Rename, delete, or change the default database. The default database is used by integrations (MCP, API).
        </p>
      </div>

      <div className="space-y-2">
        {databases.map(db => {
          const status = pipelineByDb.get(db.id);
          const summary = pipelineSummary(status);
          const embeddingRetryKey = `${db.id}:embedding`;
          const taggingRetryKey = `${db.id}:tagging`;
          const exportJob = exportJobsByDb[db.id];
          const isExpanded = expandedPipeline === db.id;
          const summaryClass = summary.tone === 'error'
            ? 'text-red-300'
            : summary.tone === 'working'
              ? 'text-amber-300'
              : summary.tone === 'healthy'
                ? 'text-green-300'
                : 'text-[var(--color-text-secondary)]';

          return (
            <div
              key={db.id}
              className="rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-main)]"
            >
              <div className="flex items-start gap-3 px-3 py-2.5">
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
                  <div className="flex-1 min-w-0 space-y-1">
                    <div className="flex items-center gap-2">
                      <span className="text-sm text-[var(--color-text-primary)] truncate font-medium">{db.name}</span>
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
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
                      <span className={`font-medium ${summaryClass}`}>{summary.label}</span>
                      <span className="text-[var(--color-text-secondary)]">{summary.detail}</span>
                    </div>
                  </div>
                )}

                {editingId !== db.id && (
                  <div className="flex items-center gap-1 flex-shrink-0">
                    {status && status.failed_count > 0 && (
                      <button
                        onClick={() => retryFailed(db.id, 'embedding')}
                        disabled={retrying === embeddingRetryKey}
                        className="inline-flex items-center gap-1 rounded px-2 py-1 text-[11px] text-red-300 hover:bg-red-500/10 disabled:opacity-50"
                        title="Retry failed embeddings"
                      >
                        {retrying === embeddingRetryKey ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={2} /> : <RefreshCw className="w-3.5 h-3.5" strokeWidth={2} />}
                        Emb
                      </button>
                    )}
                    {status && status.tagging_failed_count > 0 && (
                      <button
                        onClick={() => retryFailed(db.id, 'tagging')}
                        disabled={retrying === taggingRetryKey}
                        className="inline-flex items-center gap-1 rounded px-2 py-1 text-[11px] text-red-300 hover:bg-red-500/10 disabled:opacity-50"
                        title="Retry failed tagging"
                      >
                        {retrying === taggingRetryKey ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={2} /> : <RefreshCw className="w-3.5 h-3.5" strokeWidth={2} />}
                        Tag
                      </button>
                    )}
                    {status && (
                      <button
                        onClick={() => setExpandedPipeline(isExpanded ? null : db.id)}
                        className="px-2 py-1 text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                      >
                        {isExpanded ? 'Hide' : 'Details'}
                      </button>
                    )}
                    <button
                      onClick={() => handleExportMarkdown(db)}
                      disabled={!!exportingDb}
                      className="inline-flex items-center gap-1 rounded px-2 py-1 text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] disabled:opacity-50 disabled:hover:bg-transparent transition-colors"
                      title="Export database as markdown archive"
                    >
                      {exportingDb === db.id ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={2} /> : <Download className="w-3.5 h-3.5" strokeWidth={2} />}
                      {exportingDb === db.id && exportJob?.total_atoms
                        ? `${Math.round((exportJob.processed_atoms / exportJob.total_atoms) * 100)}%`
                        : 'Export'}
                    </button>
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
                      <Pencil width="14" height="14" strokeWidth={2} />
                    </button>
                    {!db.is_default && (
                      <button
                        onClick={() => handleStartDelete(db)}
                        className="p-1.5 text-[var(--color-text-tertiary)] hover:text-red-400 hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                        title="Delete database"
                      >
                        <Trash2 width="14" height="14" strokeWidth={2} />
                      </button>
                    )}
                  </div>
                )}
              </div>

              {isExpanded && status && (
                <div className="px-3 pb-3 space-y-3">
                  <PipelineDetailCounts status={status} />
                  {status.complete > 0 && (
                    <div className="flex items-center justify-between gap-3 rounded border border-[var(--color-border)] bg-[var(--color-bg-panel)] px-3 py-2">
                      <div className="min-w-0">
                        <div className="text-xs font-medium text-[var(--color-text-primary)]">Recalculate embeddings</div>
                        <div className="text-[11px] text-[var(--color-text-secondary)]">
                          Queues embed-only work for this database while preserving tags and chunk content.
                        </div>
                      </div>
                      <Button
                        variant="secondary"
                        size="sm"
                        onClick={() => setConfirmReembedDb(db)}
                        disabled={reembeddingDb === db.id}
                        title="Re-embed all atoms in this database"
                        className="flex-shrink-0 gap-1"
                      >
                        {reembeddingDb === db.id ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={2} /> : <RefreshCw className="w-3.5 h-3.5" strokeWidth={2} />}
                        Re-embed
                      </Button>
                    </div>
                  )}
                  {status.complete > 0 && (
                    <div className="flex items-center justify-between gap-3 rounded border border-[var(--color-border)] bg-[var(--color-bg-panel)] px-3 py-2">
                      <div className="min-w-0">
                        <div className="text-xs font-medium text-[var(--color-text-primary)]">Re-run auto-tagging</div>
                        <div className="text-[11px] text-[var(--color-text-secondary)]">
                          Removes auto-generated tags (except those whose tag has a wiki article) and re-extracts tags using your current tagging model. Manual tags are preserved.
                        </div>
                      </div>
                      <Button
                        variant="secondary"
                        size="sm"
                        onClick={() => setConfirmRetagDb(db)}
                        disabled={retaggingDb === db.id}
                        title="Re-run auto-tagging for all atoms in this database"
                        className="flex-shrink-0 gap-1"
                      >
                        {retaggingDb === db.id ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={2} /> : <RefreshCw className="w-3.5 h-3.5" strokeWidth={2} />}
                        Re-tag
                      </Button>
                    </div>
                  )}
                </div>
              )}
            </div>
          );
        })}
      </div>

      {databases.length === 0 && (
        <p className="text-sm text-[var(--color-text-secondary)] text-center py-4">No databases found.</p>
      )}

      {pipelineError && (
        <div className="p-3 bg-red-500/10 border border-red-500/30 rounded-md text-sm text-red-300">
          {pipelineError}
        </div>
      )}

      {pipelineLoading && databases.length > 0 && (
        <div className="flex items-center gap-2 text-xs text-[var(--color-text-secondary)]">
          <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={2} />
          Checking AI pipeline status...
        </div>
      )}

      {/* Delete confirmation dialog */}
      <Modal
        isOpen={!!confirmDeleteDb}
        onClose={() => {
          if (!isDeleting) setConfirmDeleteDb(null);
        }}
        title="Delete database?"
        showFooter={false}
      >
        <div className="space-y-4">
          <p className="text-xs text-[var(--color-text-secondary)]">
            This will permanently delete <span className="font-medium text-[var(--color-text-primary)]">"{confirmDeleteDb?.name}"</span>
            {isLoadingStats ? (
              <span> and all its data.</span>
            ) : deleteStats && deleteStats.atom_count >= 0 ? (
              <span> and its <span className="font-medium text-[var(--color-text-primary)]">{deleteStats.atom_count} atom{deleteStats.atom_count !== 1 ? 's' : ''}</span>. </span>
            ) : (
              <span> and all its data. </span>
            )}
            This action cannot be undone.
          </p>
          <div className="flex gap-2 justify-end">
            <Button
              variant="secondary"
              size="sm"
              onClick={() => setConfirmDeleteDb(null)}
              disabled={isDeleting}
            >
              Cancel
            </Button>
            <Button
              variant="danger"
              size="sm"
              onClick={handleConfirmDelete}
              disabled={isDeleting || isLoadingStats}
            >
              {isDeleting ? 'Deleting...' : 'Delete'}
            </Button>
          </div>
        </div>
      </Modal>

      <Modal
        isOpen={!!confirmReembedDb}
        onClose={() => {
          if (!reembeddingDb) setConfirmReembedDb(null);
        }}
        title="Re-embed database?"
        showFooter={false}
      >
        <div className="space-y-4">
          <p className="text-xs text-[var(--color-text-secondary)]">
            This clears embedding status for <span className="font-medium text-[var(--color-text-primary)]">"{confirmReembedDb?.name}"</span> and queues embed-only work for its atoms.
            Existing tags and chunk content are preserved.
          </p>
          <div className="flex gap-2 justify-end">
            <Button
              variant="secondary"
              size="sm"
              onClick={() => setConfirmReembedDb(null)}
              disabled={!!reembeddingDb}
            >
              Cancel
            </Button>
            <Button
              size="sm"
              onClick={handleConfirmReembedAll}
              disabled={!!reembeddingDb}
            >
              {reembeddingDb === confirmReembedDb?.id ? 'Queuing...' : 'Re-embed'}
            </Button>
          </div>
        </div>
      </Modal>

      <Modal
        isOpen={!!confirmRetagDb}
        onClose={() => {
          if (!retaggingDb) setConfirmRetagDb(null);
        }}
        title="Re-run auto-tagging?"
        showFooter={false}
      >
        <div className="space-y-3">
          <p className="text-xs text-[var(--color-text-secondary)]">
            This will remove auto-generated tag assignments from <span className="font-medium text-[var(--color-text-primary)]">"{confirmRetagDb?.name}"</span> and re-run auto-tagging across every atom using your current tagging model.
          </p>
          <ul className="text-xs text-[var(--color-text-secondary)] space-y-1 pl-4 list-disc">
            <li>Manually-added tags are preserved.</li>
            <li>Tags with a wiki article are preserved (across all their atoms).</li>
            <li>All other auto-generated tag assignments are removed before re-tagging.</li>
          </ul>
          {(() => {
            const legacy = confirmRetagDb
              ? pipelineByDb.get(confirmRetagDb.id)?.legacy_auto_tag_count ?? 0
              : 0;
            if (legacy <= 0) return null;
            return (
              <p className="text-xs text-amber-300/90 bg-amber-500/10 border border-amber-500/30 rounded px-2 py-1.5">
                Note: {legacy.toLocaleString()} tag assignment{legacy === 1 ? '' : 's'} from before this update will be treated as auto-generated and may be removed if the tag has no wiki article.
              </p>
            );
          })()}
          <div className="flex gap-2 justify-end">
            <Button
              variant="secondary"
              size="sm"
              onClick={() => setConfirmRetagDb(null)}
              disabled={!!retaggingDb}
            >
              Cancel
            </Button>
            <Button
              size="sm"
              onClick={handleConfirmRetagAll}
              disabled={!!retaggingDb}
            >
              {retaggingDb === confirmRetagDb?.id ? 'Queuing...' : 'Re-tag'}
            </Button>
          </div>
        </div>
      </Modal>
    </>
  );
}

interface SettingsModalProps {
  isOpen: boolean;
  onClose: () => void;
  initialTab?: SettingsTab;
}

export function SettingsModal({ isOpen, onClose, initialTab }: SettingsModalProps) {
  const settings = useSettingsStore(s => s.settings);
  const fetchSettings = useSettingsStore(s => s.fetchSettings);
  const setSetting = useSettingsStore(s => s.setSetting);
  const testOpenRouterConnection = useSettingsStore(s => s.testOpenRouterConnection);
  // Per-DB context was previously consumed by the briefing schedule panel,
  // which moved onto the reports primitive in phase 3. Removing the
  // selectors keeps the modal from re-rendering on unrelated DB-list
  // changes; phase 4's report-authoring UI will pull what it needs back.

  // Theme & Font
  const [theme, setTheme] = useState<Theme>('obsidian');
  const [font, setFont] = useState<Font>('ibm-plex-sans');
  const [timezone, setTimezone] = useState(getBrowserTimeZone());
  const supportedTimeZones = useMemo(() => getSupportedTimeZones(), []);

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
  const [wikiModel, setWikiModel] = useState('anthropic/claude-sonnet-4.6');
  const [wikiStrategy, setWikiStrategy] = useState('centroid');
  const [wikiGenerationPrompt, setWikiGenerationPrompt] = useState('');
  const [wikiUpdatePrompt, setWikiUpdatePrompt] = useState('');
  const [chatPrompt, setChatPrompt] = useState('');
  const [taggingPrompt, setTaggingPrompt] = useState('');
  const [chatModel, setChatModel] = useState('anthropic/claude-sonnet-4.6');
  const [saveError, setSaveError] = useState<string | null>(null);

  // Re-embedding confirmation
  const [pendingEmbeddingChange, setPendingEmbeddingChange] = useState<{ key: string; value: string; label: string } | null>(null);

  // OpenRouter model loading
  const [availableModels, setAvailableModels] = useState<AvailableModel[]>([]);
  const [openrouterEmbeddingModels, setOpenrouterEmbeddingModels] = useState<OpenRouterEmbeddingModel[]>([]);
  const [isLoadingModels, setIsLoadingModels] = useState(false);

  // Import state
  const [isImporting, setIsImporting] = useState(false);
  const [importResult, setImportResult] = useState<ImportResult | null>(null);
  const [importError, setImportError] = useState<string | null>(null);
  const [importTags, setImportTags] = useState(false);
  const [importProgress, setImportProgress] = useState<ImportProgress | null>(null);

  // Tab state
  const [activeTab, setActiveTab] = useState<SettingsTab>(initialTab ?? 'general');

  // When the modal is reopened with a new initialTab, sync the active tab.
  useEffect(() => {
    if (isOpen && initialTab) {
      setActiveTab(initialTab);
    }
  }, [isOpen, initialTab]);

  // Integrations tab: collapsible sections — only one open at a time.
  const [expandedIntegration, setExpandedIntegration] = useState<
    'markdown' | 'apple-notes' | 'mcp' | null
  >(null);

  // MCP setup state
  const showMcpSetup = expandedIntegration === 'mcp';
  const [mcpConfig, setMcpConfig] = useState<McpConfig | null>(null);
  const [mcpConfigCopied, setMcpConfigCopied] = useState(false);
  const [isCreatingMcpToken, setIsCreatingMcpToken] = useState(false);
  const [mcpTokenError, setMcpTokenError] = useState<string | null>(null);

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
  const [localUrlCopied, setLocalUrlCopied] = useState(false);
  const [localTokenCopied, setLocalTokenCopied] = useState(false);

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

  // Feed action state
  const [pollingFeedId, setPollingFeedId] = useState<string | null>(null);
  const [pollResult, setPollResult] = useState<FeedPollResult | null>(null);
  const [deletingFeedId, setDeletingFeedId] = useState<string | null>(null);

  const overlayRef = useRef<HTMLDivElement>(null);

  // Derived: whether we're connected to a remote (non-local) server
  // Desktop + local sidecar → false; Desktop + remote override → true; Web → always true
  const isRemoteMode = isDesktopApp() ? !isLocalServer() : true;
  const localServerConfig = isDesktopApp() ? getLocalServerConfig() : null;

  // On Atomic Cloud the provider/models and the connection itself are owned by
  // the control plane (managed key or dashboard BYOK; fixed cookie-auth origin;
  // account-scoped tokens live in the dashboard). Those tabs would only mislead
  // — and their fetches (provider models, /api/auth/tokens) 404 — so a cloud
  // tenant never sees them.
  const isCloud = isCloudTenant();
  const visibleTabs = useMemo(
    () =>
      isCloud
        ? SETTINGS_TABS.filter(tab => tab.id !== 'ai' && tab.id !== 'connection')
        : SETTINGS_TABS,
    [isCloud],
  );

  // Check Ollama connection
  const checkOllamaConnection = useCallback(async (host: string) => {
    setOllamaStatus('checking');
    setOllamaError(undefined);
    try {
      const connected = await testOllamaConnection(host);
      if (connected) {
        setOllamaStatus('connected');
        // Fetch available models
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

  const handleConnectServer = async () => {
    try {
      await switchTransport({ baseUrl: serverUrl.trim().replace(/\/$/, ''), authToken: serverToken.trim() });
      setShowChangeServer(false);
      // Refresh data from new source
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
      // Refresh data from local source
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
      // Refresh token list
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
    // Check if revoking the current token
    const currentPrefix = serverToken.substring(0, 10);
    const tokenToRevoke = apiTokens.find(t => t.id === tokenId);
    const isCurrentToken = tokenToRevoke && tokenToRevoke.token_prefix === currentPrefix;

    try {
      await revokeApiToken(tokenId);
      if (isCurrentToken) {
        // Revoking current token — log out
        localStorage.removeItem('atomic-server-config');
        window.location.reload();
        return;
      }
      // Refresh list
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

  const handleCopyLocalUrl = async () => {
    if (!localServerConfig) return;
    try {
      await copyToClipboard(localServerConfig.baseUrl);
      setLocalUrlCopied(true);
      setTimeout(() => setLocalUrlCopied(false), 2000);
    } catch (e) {
      console.error('Failed to copy local server URL:', e);
    }
  };

  const handleCopyLocalToken = async () => {
    if (!localServerConfig?.authToken) return;
    try {
      await copyToClipboard(localServerConfig.authToken);
      setLocalTokenCopied(true);
      setTimeout(() => setLocalTokenCopied(false), 2000);
    } catch (e) {
      console.error('Failed to copy local API token:', e);
    }
  };

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
        // Provider/model config is control-plane-owned on cloud — skip the
        // model registry fetches that feed the (hidden) AI Models tab.
        if (!isCloud) {
          // Fetch OpenRouter models
          setIsLoadingModels(true);
          getAvailableLlmModels()
            .then(models => setAvailableModels(models))
            .catch(err => { console.error('Failed to load models:', err); toast.error('Failed to load models', { description: String(err) }); })
            .finally(() => setIsLoadingModels(false));
          // Fetch curated OpenRouter embedding model registry
          getOpenRouterEmbeddingModels()
            .then(models => setOpenrouterEmbeddingModels(models))
            .catch(err => { console.error('Failed to load embedding models:', err); });
        }
      }
      // Load API tokens when connected to a non-local server. Cloud tenants
      // manage account-scoped tokens in the dashboard, not here (the data-plane
      // /api/auth/tokens route is not exposed), so skip it.
      if (!isLocalServer() && !isCloud && transport.isConnected()) {
        loadApiTokens();
      }
      // Reset token creation state
      setCreatedToken(null);
      setTokenCopied(false);
      setShowTokenSection(false);
      setConfirmRevokeId(null);
    }
  }, [isOpen, isCloud, fetchSettings, loadApiTokens]);

  // If the active tab isn't available in this mode (e.g. a cloud tenant landing
  // on the hidden AI/Connection tab via initialTab), fall back to General.
  useEffect(() => {
    if (isOpen && !visibleTabs.some(tab => tab.id === activeTab)) {
      setActiveTab('general');
    }
  }, [isOpen, activeTab, visibleTabs]);

  // Load feeds when integrations tab is active.
  useEffect(() => {
    if (isOpen && activeTab === 'integrations' && getTransport().isConnected()) {
      loadFeeds();
      // Reset ingest state when switching to integrations tab
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
    setTimezone(settings.timezone || getBrowserTimeZone());
    setProvider(p || 'openrouter');
    setApiKey(settings.openrouter_api_key || '');
    setOpenrouterContextLength(settings.openrouter_context_length || '');
    setAutoTaggingEnabled(settings.auto_tagging_enabled !== 'false');
    setEmbeddingModel(settings.embedding_model || 'openai/text-embedding-3-small');
    setTaggingModel(settings.tagging_model || 'openai/gpt-4o-mini');
    setWikiModel(settings.wiki_model || 'anthropic/claude-sonnet-4.6');
    setWikiStrategy(settings.wiki_strategy || 'centroid');
    setWikiGenerationPrompt(settings.wiki_generation_prompt || '');
    setWikiUpdatePrompt(settings.wiki_update_prompt || '');
    setChatPrompt(settings.chat_prompt || '');
    setTaggingPrompt(settings.tagging_prompt || '');
    setChatModel(settings.chat_model || 'anthropic/claude-sonnet-4.6');
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

  // Check Ollama connection when provider is ollama or host changes.
  // Debounced so typing into the host field doesn't fire a request per keystroke.
  useEffect(() => {
    if (!isOpen || provider !== 'ollama') return;
    const handle = setTimeout(() => checkOllamaConnection(ollamaHost), 400);
    return () => clearTimeout(handle);
  }, [isOpen, provider, ollamaHost, checkOllamaConnection]);

  useEffect(() => {
    const handleEscape = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        if (document.querySelector('[data-modal="true"]')) return;
        onClose();
      }
    };

    if (isOpen) {
      document.addEventListener('keydown', handleEscape);
      document.body.style.overflow = 'hidden';
    }

    return () => {
      document.removeEventListener('keydown', handleEscape);
      document.body.style.overflow = '';
    };
  }, [isOpen, onClose]);

  const handleOverlayClick = (e: React.MouseEvent) => {
    if (e.target === overlayRef.current) {
      onClose();
    }
  };

  // Auto-save a single setting (non-setup mode only)
  const autoSave = useCallback(async (key: string, value: string) => {
    try {
      await setSetting(key, value);
    } catch (e) {
      console.error(`Failed to save ${key}:`, e);
      setSaveError(`Failed to save setting`);
      setTimeout(() => setSaveError(null), 3000);
    }
  }, [setSetting]);

  // Handle changes that trigger re-embedding — ask for confirmation
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

  // Handle provider change — test connection automatically
  const handleProviderChange = async (value: 'openrouter' | 'ollama' | 'openai_compat') => {
    setProvider(value);
    await autoSave('provider', value);
    // Test connection for new provider
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

  // API key — local state updates immediately, auto-save on blur
  const handleApiKeyChange = (value: string) => {
    setApiKey(value);
    setTestResult(null);
    setTestError(null);
  };

  const handleApiKeyBlur = async () => {
    if (!apiKey.trim()) return;
    await autoSave('openrouter_api_key', apiKey);
    // Test connection with new key
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

  // Toggle a section in the Integrations tab. When opening the MCP section,
  // also lazy-load the bridge config.
  const toggleIntegration = async (key: 'markdown' | 'apple-notes' | 'mcp') => {
    const opening = expandedIntegration !== key;
    setExpandedIntegration(opening ? key : null);
    if (!(opening && key === 'mcp')) return;

    const nowLocal = isDesktopApp() && isLocalServer();
    const configIsStdio = mcpConfig && 'command' in (mcpConfig as any).mcpServers.atomic;
    if ((nowLocal && !configIsStdio && mcpConfig) || (!nowLocal && configIsStdio)) {
      setMcpConfig(null);
      setMcpTokenError(null);
    }

    if (nowLocal && !mcpConfig) {
      const bridgePath = await getMcpBridgePath();
      if (bridgePath) {
        setMcpConfig(getMcpStdioConfig(bridgePath));
      } else {
        setMcpTokenError('Could not locate atomic-mcp-bridge. Ensure the app bundle is complete.');
      }
    }
  };

  const handleCreateMcpToken = async () => {
    setIsCreatingMcpToken(true);
    setMcpTokenError(null);
    try {
      const result = await createApiToken('mcp-integration');
      const transport = getTransport() as import('../../lib/transport/http').HttpTransport;
      const config = getMcpHttpConfig(transport.getConfig().baseUrl, result.token);
      setMcpConfig(config);
    } catch (e) {
      setMcpTokenError(String(e));
    } finally {
      setIsCreatingMcpToken(false);
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
  const fetchAtoms = useAtomsStore((state) => state.fetchAtoms);
  const fetchTags = useTagsStore((state) => state.fetchTags);

  const handleObsidianImport = async () => {
    setImportResult(null);
    setImportError(null);
    setImportProgress(null);

    try {
      // Open folder picker dialog
      const selected = await pickDirectory('Select Markdown Folder');

      if (!selected) {
        return; // User cancelled or not available in web mode
      }

      setIsImporting(true);

      const result = await importMarkdownFolder(selected, {
        importTags,
        onProgress: setImportProgress,
      });
      setImportResult(result);

      // Refresh atoms and tags to show imported content
      if (result.imported > 0) {
        await Promise.all([fetchAtoms(), fetchTags()]);
      }
    } catch (e) {
      setImportError(String(e));
    } finally {
      setIsImporting(false);
    }
  };

  const [appleNotesNeedsFda, setAppleNotesNeedsFda] = useState(false);

  const handleAppleNotesImport = async () => {
    setImportResult(null);
    setImportError(null);
    setImportProgress(null);
    setAppleNotesNeedsFda(false);

    try {
      setIsImporting(true);
      const result = await importAppleNotes({
        importTags,
        onProgress: setImportProgress,
      });
      setImportResult(result);

      if (result.imported > 0) {
        await Promise.all([fetchAtoms(), fetchTags()]);
      }
    } catch (e) {
      if (e instanceof AppleNotesImportError && e.kind === 'permissionDenied') {
        setAppleNotesNeedsFda(true);
      } else if (e instanceof AppleNotesImportError && e.kind === 'notFound') {
        setImportError(
          'Apple Notes data folder not found. Open the Apple Notes app at least once, then try again.',
        );
      } else {
        setImportError(e instanceof Error ? e.message : String(e));
      }
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

  return createPortal(
    <div
      ref={overlayRef}
      onClick={handleOverlayClick}
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm safe-area-padding"
    >
      <div className="relative bg-[var(--color-bg-panel)] rounded-lg shadow-xl border border-[var(--color-border)] w-[min(1120px,calc(100vw-2rem))] h-[84vh] flex flex-col animate-in fade-in zoom-in-95 duration-200">
        {/* Header */}
        <div className="px-5 py-3 border-b border-[var(--color-border)] flex items-center justify-between">
          <h2 className="text-base font-semibold text-[var(--color-text-primary)]">
            Settings
          </h2>
          <button
            onClick={onClose}
            className="p-1.5 text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
            title="Close"
          >
            <X className="w-5 h-5" strokeWidth={2} />
          </button>
        </div>

        <div className="flex min-h-0 flex-1 flex-col md:flex-row">
          <nav className="md:hidden border-b border-[var(--color-border)] bg-[var(--color-bg-main)]/35 overflow-x-auto">
            <div className="flex gap-1 p-2 min-w-max">
              {visibleTabs.map((tab) => (
                <button
                  key={tab.id}
                  onClick={() => setActiveTab(tab.id)}
                  className={`px-3 py-2 text-sm rounded-md transition-colors whitespace-nowrap ${
                    activeTab === tab.id
                      ? 'bg-[var(--color-bg-hover)] text-[var(--color-text-primary)]'
                      : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]/70'
                  }`}
                >
                  {tab.label}
                </button>
              ))}
            </div>
          </nav>

          <nav className="hidden md:block w-48 flex-shrink-0 border-r border-[var(--color-border)] bg-[var(--color-bg-main)]/35 p-2 overflow-y-auto">
            <div className="space-y-1">
              {visibleTabs.map((tab) => (
                <button
                  key={tab.id}
                  onClick={() => setActiveTab(tab.id)}
                  className={`w-full px-3 py-2 text-left text-sm rounded-md transition-colors ${
                    activeTab === tab.id
                      ? 'bg-[var(--color-bg-hover)] text-[var(--color-text-primary)]'
                      : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]/70'
                  }`}
                >
                  {tab.label}
                </button>
              ))}
            </div>
          </nav>

          {/* Content */}
          <div className="px-4 py-4 md:px-6 md:py-5 space-y-6 overflow-y-auto flex-1 min-w-0">
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

                  {/* Time Zone */}
                  <div className="space-y-2">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      Time Zone
                    </label>
                    <input
                      type="text"
                      list="settings-timezones"
                      value={timezone}
                      onChange={(e) => setTimezone(e.target.value)}
                      onBlur={() => autoSave('timezone', timezone || getBrowserTimeZone())}
                      className="w-full px-3 py-2 rounded-md bg-[var(--color-bg-card)] border border-[var(--color-border)] text-sm text-[var(--color-text-primary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent"
                    />
                    <datalist id="settings-timezones">
                      {supportedTimeZones.map((tz) => (
                        <option key={tz} value={tz} />
                      ))}
                    </datalist>
                  </div>

                  {/* Briefing schedule + custom prompt moved onto the
                      Daily Briefing report row in phase 3. Phase 4 will
                      surface report authoring UI here; until then, the
                      schedule and prompt are editable via
                      `PUT /api/reports/:id`. */}

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
                    {/* Connection status — shown inline after provider */}
                    {provider === 'openrouter' && isTesting && (
                      <div className="flex items-center gap-2 text-sm text-[var(--color-text-secondary)]">
                        <Loader2 className="w-4 h-4 animate-spin" strokeWidth={2} />
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
                    <OverrideControls settingKey="provider" />
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
                              <EyeOff className="w-5 h-5" strokeWidth={2} />
                            ) : (
                              <Eye className="w-5 h-5" strokeWidth={2} />
                            )}
                          </button>
                        </div>
                      </div>

                      {/* Model Configuration for OpenRouter — always visible */}
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
                          <SearchableSelect
                            value={embeddingModel}
                            onChange={handleEmbeddingModelChange}
                            options={openrouterEmbeddingModels.map(m => ({
                              id: m.id,
                              name: `${m.name} (${m.dimension})`,
                            }))}
                            placeholder="Select embedding model..."
                          />
                          <OverrideControls settingKey="embedding_model" />
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
                          <OverrideControls settingKey="tagging_model" />
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
                          <OverrideControls settingKey="wiki_model" />
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
                          <OverrideControls settingKey="wiki_strategy" />
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
                          <OverrideControls settingKey="chat_model" />
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
                          <OverrideControls settingKey="openrouter_context_length" />
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
                          onBlur={() => autoSave('ollama_host', ollamaHost)}
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
                            <OverrideControls settingKey="ollama_embedding_model" />
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
                            <OverrideControls settingKey="ollama_llm_model" />
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
                            <OverrideControls settingKey="ollama_context_length" />
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
                            <OverrideControls settingKey="ollama_timeout_secs" />
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
                            <Loader2 className="w-4 h-4 animate-spin" strokeWidth={2} />
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
                              <EyeOff className="w-5 h-5" strokeWidth={2} />
                            ) : (
                              <Eye className="w-5 h-5" strokeWidth={2} />
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
                          <OverrideControls settingKey="openai_compat_embedding_model" />
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
                          <OverrideControls settingKey="openai_compat_embedding_dimension" />
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
                          <OverrideControls settingKey="openai_compat_llm_model" />
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
                          <OverrideControls settingKey="openai_compat_context_length" />
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
                          <OverrideControls settingKey="openai_compat_timeout_secs" />
                        </div>
                      </div>
                    </>
                  )}
                </>
              )}


              {/* ===== PROMPTS TAB ===== */}
              {activeTab === 'prompts' && (
                <div className="space-y-6">

                  {/* Wiki Generation Prompt */}
                  <div className="space-y-1">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      Wiki Generation Prompt
                    </label>
                    <p className="text-xs text-[var(--color-text-secondary)]">
                      System prompt for generating new wiki articles. Leave empty to use the default.
                    </p>
                    <textarea
                      value={wikiGenerationPrompt}
                      onChange={(e) => setWikiGenerationPrompt(e.target.value)}
                      onBlur={() => autoSave('wiki_generation_prompt', wikiGenerationPrompt)}
                      placeholder={"You are synthesizing a wiki article based on the user's personal knowledge base. Write a well-structured, informative article that summarizes what is known about the topic.\n\nGuidelines:\n- Use markdown formatting with ## for main sections and ### for subsections\n- Every factual claim MUST have a citation using [N] notation\n- Place citations immediately after the relevant statement\n- If sources contain contradictions, note them\n- Structure logically: overview first, then thematic sections\n- Keep tone informative and neutral\n- Do not invent information not present in the sources\n- When mentioning topics that have their own articles in the knowledge base, use [[Topic Name]] wiki-link notation to cross-reference them\n- Only use [[wiki links]] for topics listed in the EXISTING WIKI ARTICLES section provided\n- Do not force wiki links where they don't fit naturally"}
                      rows={8}
                      className="w-full px-3 py-2 rounded-md bg-[var(--color-bg-main)] border border-[var(--color-border)] text-sm text-[var(--color-text-primary)] font-mono resize-y placeholder:text-[var(--color-text-secondary)]/40"
                    />
                    {wikiGenerationPrompt && (
                      <button
                        onClick={() => { setWikiGenerationPrompt(''); autoSave('wiki_generation_prompt', ''); }}
                        className="text-xs text-[var(--color-accent)] hover:underline"
                      >
                        Reset to default
                      </button>
                    )}
                    <OverrideControls settingKey="wiki_generation_prompt" />
                  </div>

                  {/* Wiki Update Prompt */}
                  <div className="space-y-1">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      Wiki Update Prompt
                    </label>
                    <p className="text-xs text-[var(--color-text-secondary)]">
                      Custom instructions prepended to the update prompt. Controls tone, style, or focus. Leave empty to use the default.
                    </p>
                    <textarea
                      value={wikiUpdatePrompt}
                      onChange={(e) => setWikiUpdatePrompt(e.target.value)}
                      onBlur={() => autoSave('wiki_update_prompt', wikiUpdatePrompt)}
                      placeholder={"e.g. Write in a casual, conversational tone. Focus on practical implications rather than theory."}
                      rows={4}
                      className="w-full px-3 py-2 rounded-md bg-[var(--color-bg-main)] border border-[var(--color-border)] text-sm text-[var(--color-text-primary)] font-mono resize-y placeholder:text-[var(--color-text-secondary)]/40"
                    />
                    {wikiUpdatePrompt && (
                      <button
                        onClick={() => { setWikiUpdatePrompt(''); autoSave('wiki_update_prompt', ''); }}
                        className="text-xs text-[var(--color-accent)] hover:underline"
                      >
                        Reset to default
                      </button>
                    )}
                    <OverrideControls settingKey="wiki_update_prompt" />
                  </div>

                  {/* Briefing prompt moved to the Daily Briefing report's
                      `research_prompt` field in phase 3. Edit via the
                      reports API (`PUT /api/reports/:id`) until the
                      report-authoring UI ships in phase 4. */}

                  {/* Chat Prompt */}
                  <div className="space-y-1">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      Chat Prompt
                    </label>
                    <p className="text-xs text-[var(--color-text-secondary)]">
                      Prepended to the chat assistant system prompt. Leave empty to use the default.
                    </p>
                    <textarea
                      value={chatPrompt}
                      onChange={(e) => setChatPrompt(e.target.value)}
                      onBlur={() => autoSave('chat_prompt', chatPrompt)}
                      placeholder={"You are a helpful AI assistant with access to the user's personal knowledge base.\n\nGuidelines:\n- Use search_atoms to find relevant information before answering\n- Cite sources using [N] notation\n- Be honest if you cannot find information\n- Keep responses concise but informative"}
                      rows={6}
                      className="w-full px-3 py-2 rounded-md bg-[var(--color-bg-main)] border border-[var(--color-border)] text-sm text-[var(--color-text-primary)] font-mono resize-y placeholder:text-[var(--color-text-secondary)]/40"
                    />
                    {chatPrompt && (
                      <button
                        onClick={() => { setChatPrompt(''); autoSave('chat_prompt', ''); }}
                        className="text-xs text-[var(--color-accent)] hover:underline"
                      >
                        Reset to default
                      </button>
                    )}
                    <OverrideControls settingKey="chat_prompt" />
                  </div>

                  {/* Tagging Prompt */}
                  <div className="space-y-1">
                    <label className="block text-sm font-medium text-[var(--color-text-primary)]">
                      Tagging Prompt
                    </label>
                    <p className="text-xs text-[var(--color-text-secondary)]">
                      System prompt for auto-tagging. Leave empty to use the default.
                    </p>
                    <textarea
                      value={taggingPrompt}
                      onChange={(e) => setTaggingPrompt(e.target.value)}
                      onBlur={() => autoSave('tagging_prompt', taggingPrompt)}
                      placeholder={"You are a knowledge management assistant that categorizes text with tags.\n\nGuidelines:\n- Each tag MUST have a parent_name set to one of the existing top-level categories\n- Prefer broad tags rather than overly specific ones\n- If none of the categories feel like a natural fit, return an empty tag list"}
                      rows={4}
                      className="w-full px-3 py-2 rounded-md bg-[var(--color-bg-main)] border border-[var(--color-border)] text-sm text-[var(--color-text-primary)] font-mono resize-y placeholder:text-[var(--color-text-secondary)]/40"
                    />
                    {taggingPrompt && (
                      <button
                        onClick={() => { setTaggingPrompt(''); autoSave('tagging_prompt', ''); }}
                        className="text-xs text-[var(--color-accent)] hover:underline"
                      >
                        Reset to default
                      </button>
                    )}
                    <OverrideControls settingKey="tagging_prompt" />
                  </div>

                </div>
              )}
              {/* ===== TAG CATEGORIES TAB ===== */}
              {activeTab === 'tag-categories' && (
                <>
                  {/* Auto-tagging master toggle — gates everything below;
                      tag categories only matter when this is on. */}
                  <div className="space-y-1">
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
                    <OverrideControls settingKey="auto_tagging_enabled" />
                  </div>

                  <TagCategoriesTab />
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
                      {localServerConfig && (
                        <div className="space-y-2 pt-2">
                          <div className="space-y-1">
                            <label className="block text-xs font-medium text-[var(--color-text-secondary)]">
                              Local Server URL
                            </label>
                            <div className="flex gap-2">
                              <code className="flex-1 px-3 py-2 bg-[var(--color-bg-main)] border border-[var(--color-border)] rounded-md text-xs text-[var(--color-text-primary)] truncate">
                                {localServerConfig.baseUrl}
                              </code>
                              <Button variant="secondary" size="sm" onClick={handleCopyLocalUrl}>
                                {localUrlCopied ? 'Copied' : 'Copy'}
                              </Button>
                            </div>
                          </div>
                          <div className="space-y-1">
                            <label className="block text-xs font-medium text-[var(--color-text-secondary)]">
                              Local API Token
                            </label>
                            <div className="flex gap-2">
                              <code className="flex-1 px-3 py-2 bg-[var(--color-bg-main)] border border-[var(--color-border)] rounded-md text-xs text-[var(--color-text-primary)] truncate">
                                {localServerConfig.authToken ? `${localServerConfig.authToken.substring(0, 12)}...` : 'N/A'}
                              </code>
                              <Button
                                variant="secondary"
                                size="sm"
                                onClick={handleCopyLocalToken}
                                disabled={!localServerConfig.authToken}
                              >
                                {localTokenCopied ? 'Copied' : 'Copy'}
                              </Button>
                            </div>
                          </div>
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Use these values for local integrations such as the Obsidian plugin.
                          </p>
                        </div>
                      )}
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

                  {/* API Tokens Section — remote/web only (auto-managed for local sidecar) */}
                  {!isLocalServer() && getTransport().isConnected() && (
                    <div className="space-y-3 pt-4 border-t border-[var(--color-border)]">
                      <button
                        type="button"
                        onClick={() => setShowTokenSection(!showTokenSection)}
                        className="flex items-center gap-2 text-sm font-medium text-[var(--color-text-primary)] hover:text-white transition-colors w-full"
                      >
                        <ChevronRight
                          className={`w-4 h-4 transition-transform ${showTokenSection ? 'rotate-90' : ''}`}
                          strokeWidth={2}
                        />
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
                              <Loader2 className="w-4 h-4 animate-spin" strokeWidth={2} />
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

                          {/* Created token display (shown once after creation) */}
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
                                    <Check className="w-4 h-4 text-green-500" strokeWidth={2} />
                                  ) : (
                                    <Copy className="w-4 h-4" strokeWidth={2} />
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

              {/* ===== INTEGRATIONS TAB ===== */}
              {activeTab === 'integrations' && (
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
                            <Loader2 className="w-4 h-4 animate-spin mr-1" strokeWidth={2} />
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
                        <Loader2 className="w-4 h-4 animate-spin" strokeWidth={2} />
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
                                    <Loader2 className="w-4 h-4 animate-spin" strokeWidth={2} />
                                  ) : (
                                    <RefreshCw className="w-4 h-4" strokeWidth={2} />
                                  )}
                                </button>
                                <button
                                  type="button"
                                  onClick={() => handleToggleFeedPause(feed)}
                                  className="p-1.5 text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] rounded transition-colors"
                                  title={feed.is_paused ? 'Resume' : 'Pause'}
                                >
                                  {feed.is_paused ? (
                                    <Play className="w-4 h-4" strokeWidth={2} />
                                  ) : (
                                    <Pause className="w-4 h-4" strokeWidth={2} />
                                  )}
                                </button>
                                <button
                                  type="button"
                                  onClick={() => handleDeleteFeed(feed.id)}
                                  disabled={deletingFeedId === feed.id}
                                  className="p-1.5 text-[var(--color-text-secondary)] hover:text-red-400 hover:bg-[var(--color-bg-hover)] rounded transition-colors disabled:opacity-50"
                                  title="Delete feed"
                                >
                                  <Trash2 className="w-4 h-4" strokeWidth={2} />
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
                            <Loader2 className="w-4 h-4 animate-spin mr-1" strokeWidth={2} />
                            Adding...
                          </>
                        ) : 'Add Feed'}
                      </Button>
                    </div>
                  </div>

                  {/* Shared import status (used by both Markdown and Apple Notes) */}
                  {(importResult || importError) && (
                    <div className="space-y-2">
                      {importResult && (
                        <div className="p-3 bg-green-500/10 border border-green-500/30 rounded-md text-sm">
                          <div className="text-green-400 font-medium mb-1">Import complete!</div>
                          <div className="text-[var(--color-text-secondary)] space-y-0.5">
                            <div>Imported: {importResult.imported} notes</div>
                            {importResult.tags_created > 0 && (
                              <div>Tags created: {importResult.tags_created}</div>
                            )}
                            {importResult.errors > 0 && (
                              <div>Errors: {importResult.errors} (failed to create)</div>
                            )}
                            {importResult.skipped > 0 && (
                              <div>Skipped: {importResult.skipped} (duplicates/empty)</div>
                            )}
                          </div>
                        </div>
                      )}
                      {importError && (
                        <div className="p-3 bg-red-500/10 border border-red-500/30 rounded-md text-sm">
                          <div className="text-red-400 font-medium mb-1">Import failed</div>
                          <div className="text-[var(--color-text-secondary)]">{importError}</div>
                        </div>
                      )}
                    </div>
                  )}

                  {/* Markdown folder — desktop only */}
                  {isDesktopApp() && (
                    <div className="space-y-3">
                      <button
                        type="button"
                        onClick={() => toggleIntegration('markdown')}
                        className="flex items-center gap-2 text-sm font-medium text-[var(--color-text-primary)] hover:text-white transition-colors w-full"
                      >
                        <ChevronRight
                          className={`w-4 h-4 transition-transform ${expandedIntegration === 'markdown' ? 'rotate-90' : ''}`}
                          strokeWidth={2}
                        />
                        Markdown Folder
                      </button>

                      {expandedIntegration === 'markdown' && (
                        <div className="space-y-3 pl-6 border-l-2 border-[var(--color-border)]">
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Import a folder of <code className="text-[var(--color-text-primary)]">.md</code> files
                            (Obsidian vault, Bear export, etc.). Folder structure becomes hierarchical tags.
                          </p>

                          <label className="flex items-center gap-2 text-sm text-[var(--color-text-secondary)] cursor-pointer">
                            <input
                              type="checkbox"
                              checked={importTags}
                              onChange={(e) => setImportTags(e.target.checked)}
                              disabled={isImporting}
                              className="rounded border-[var(--color-border)]"
                            />
                            Import tags from folders and frontmatter
                          </label>

                          <Button
                            variant="secondary"
                            onClick={handleObsidianImport}
                            disabled={isImporting}
                            className="w-full justify-center"
                          >
                            {isImporting ? (
                              <>
                                <Loader2 className="w-4 h-4 animate-spin mr-2" strokeWidth={2} />
                                {importProgress
                                  ? `Importing ${importProgress.current}/${importProgress.total}...`
                                  : 'Importing...'}
                              </>
                            ) : (
                              <>
                                <Upload className="w-4 h-4 mr-2" strokeWidth={2} />
                                Choose Folder...
                              </>
                            )}
                          </Button>
                        </div>
                      )}
                    </div>
                  )}

                  {/* Apple Notes — desktop macOS only */}
                  {isDesktopApp() && isMacOS() && (
                    <div className="space-y-3">
                      <button
                        type="button"
                        onClick={() => toggleIntegration('apple-notes')}
                        className="flex items-center gap-2 text-sm font-medium text-[var(--color-text-primary)] hover:text-white transition-colors w-full"
                      >
                        <ChevronRight
                          className={`w-4 h-4 transition-transform ${expandedIntegration === 'apple-notes' ? 'rotate-90' : ''}`}
                          strokeWidth={2}
                        />
                        Apple Notes
                      </button>

                      {expandedIntegration === 'apple-notes' && (
                        <div className="space-y-3 pl-6 border-l-2 border-[var(--color-border)]">
                          <p className="text-xs text-[var(--color-text-secondary)]">
                            Import notes directly from the Apple Notes app. Folders become hierarchical tags.
                          </p>

                          <label className="flex items-center gap-2 text-sm text-[var(--color-text-secondary)] cursor-pointer">
                            <input
                              type="checkbox"
                              checked={importTags}
                              onChange={(e) => setImportTags(e.target.checked)}
                              disabled={isImporting}
                              className="rounded border-[var(--color-border)]"
                            />
                            Import tags from Apple Notes folders
                          </label>

                          <Button
                            variant="secondary"
                            onClick={handleAppleNotesImport}
                            disabled={isImporting}
                            className="w-full justify-center"
                          >
                            {isImporting ? (
                              <>
                                <Loader2 className="w-4 h-4 animate-spin mr-2" strokeWidth={2} />
                                {importProgress
                                  ? `Importing ${importProgress.current}/${importProgress.total}...`
                                  : 'Importing...'}
                              </>
                            ) : (
                              <>
                                <Upload className="w-4 h-4 mr-2" strokeWidth={2} />
                                Import from Apple Notes
                              </>
                            )}
                          </Button>

                          {appleNotesNeedsFda && (
                            <div className="p-3 bg-amber-500/10 border border-amber-500/30 rounded-md text-sm space-y-2">
                              <div className="text-amber-400 font-medium">Full Disk Access required</div>
                              <p className="text-xs text-[var(--color-text-secondary)]">
                                Grant Atomic access to read your Apple Notes data, then try the import again.
                                Atomic appears in the Full Disk Access list after you click the button below.
                              </p>
                              <div className="flex gap-2">
                                <Button
                                  variant="secondary"
                                  size="sm"
                                  onClick={() => openExternalUrl(MACOS_FULL_DISK_ACCESS_URL)}
                                >
                                  Open System Settings
                                </Button>
                                <Button
                                  variant="secondary"
                                  size="sm"
                                  onClick={handleAppleNotesImport}
                                  disabled={isImporting}
                                >
                                  Try again
                                </Button>
                              </div>
                            </div>
                          )}
                        </div>
                      )}
                    </div>
                  )}

                  {/* MCP Server Setup Section — available when connected */}
                  {getTransport().isConnected() && (
                    <div className="space-y-3">
                      <button
                        type="button"
                        onClick={() => toggleIntegration('mcp')}
                        className="flex items-center gap-2 text-sm font-medium text-[var(--color-text-primary)] hover:text-white transition-colors w-full"
                      >
                        <ChevronRight
                          className={`w-4 h-4 transition-transform ${showMcpSetup ? 'rotate-90' : ''}`}
                          strokeWidth={2}
                        />
                        MCP Integration
                      </button>

                      {showMcpSetup && (
                        <div className="space-y-4 pl-6 border-l-2 border-[var(--color-border)]">
                          {isDesktopApp() && isLocalServer() ? (
                            <>
                              <p className="text-xs text-[var(--color-text-secondary)]">
                                The Atomic MCP bridge is bundled with the desktop app. It connects to the local server automatically — no token configuration needed.
                              </p>

                              <div className="space-y-2">
                                <div className="text-sm font-medium text-[var(--color-text-primary)]">Setup Instructions</div>
                                <ol className="text-xs text-[var(--color-text-secondary)] space-y-2 list-decimal list-inside">
                                  <li>Open your MCP client settings (e.g. Claude Desktop &gt; <span className="text-[var(--color-text-primary)]">Developer &gt; Edit Config</span>)</li>
                                  <li>Add the following configuration:</li>
                                </ol>
                              </div>

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
                                    <Check className="w-4 h-4 text-green-500" strokeWidth={2} />
                                  ) : (
                                    <Copy className="w-4 h-4" strokeWidth={2} />
                                  )}
                                </button>
                              </div>

                              <ol start={3} className="text-xs text-[var(--color-text-secondary)] space-y-2 list-decimal list-inside">
                                <li>Save the config file and restart your MCP client</li>
                              </ol>

                              <div className="p-3 bg-green-500/10 border border-green-500/30 rounded-md text-xs text-green-400">
                                <strong>Note:</strong> The Atomic desktop app must be running for the MCP bridge to connect.
                              </div>
                            </>
                          ) : (
                            <>
                              <p className="text-xs text-[var(--color-text-secondary)]">
                                Connect your MCP client to this Atomic server's HTTP endpoint. A dedicated API token is required for authentication.
                              </p>

                              {!mcpConfig ? (
                                <div className="space-y-2">
                                  <Button variant="secondary" size="sm" onClick={handleCreateMcpToken} disabled={isCreatingMcpToken}>
                                    {isCreatingMcpToken ? 'Creating...' : 'Create MCP Token'}
                                  </Button>
                                  {mcpTokenError && <p className="text-xs text-red-500">{mcpTokenError}</p>}
                                </div>
                              ) : (
                                <>
                                  <div className="p-3 bg-amber-500/10 border border-amber-500/30 rounded-md text-xs text-amber-400">
                                    Save this config now — the token won't be shown again.
                                  </div>

                                  <div className="space-y-2">
                                    <div className="text-sm font-medium text-[var(--color-text-primary)]">Setup Instructions</div>
                                    <ol className="text-xs text-[var(--color-text-secondary)] space-y-2 list-decimal list-inside">
                                      <li>Open your MCP client settings (e.g. Claude Desktop &gt; <span className="text-[var(--color-text-primary)]">Developer &gt; Edit Config</span>)</li>
                                      <li>Add the following configuration:</li>
                                    </ol>
                                  </div>

                                  <div className="relative">
                                    <pre className="p-3 bg-[var(--color-bg-main)] border border-[var(--color-border)] rounded-md text-xs text-[var(--color-text-primary)] overflow-x-auto">
                                      {JSON.stringify(mcpConfig, null, 2)}
                                    </pre>
                                    <button
                                      type="button"
                                      onClick={handleCopyMcpConfig}
                                      className="absolute top-2 right-2 p-1.5 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors"
                                      title="Copy to clipboard"
                                    >
                                      {mcpConfigCopied ? (
                                        <Check className="w-4 h-4 text-green-500" strokeWidth={2} />
                                      ) : (
                                        <Copy className="w-4 h-4" strokeWidth={2} />
                                      )}
                                    </button>
                                  </div>

                                  <ol start={3} className="text-xs text-[var(--color-text-secondary)] space-y-2 list-decimal list-inside">
                                    <li>Save the config file and restart your MCP client</li>
                                  </ol>

                                  <div className="p-3 bg-green-500/10 border border-green-500/30 rounded-md text-xs text-green-400">
                                    <strong>Note:</strong> The Atomic server must be running and reachable for MCP clients to connect.
                                  </div>
                                </>
                              )}
                            </>
                          )}
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
        </div>

        {saveError && (
          <div className="px-6 py-3 border-t border-[var(--color-border)]">
            <div className="flex items-start gap-2 text-sm text-red-500">
              <AlertCircle className="w-4 h-4 mt-0.5 flex-shrink-0" strokeWidth={2} />
              <span>{saveError}</span>
            </div>
          </div>
        )}

        <Modal
          isOpen={!!pendingEmbeddingChange}
          onClose={cancelEmbeddingChange}
          title="Re-embed all atoms?"
          confirmLabel="Re-embed"
          onConfirm={confirmEmbeddingChange}
        >
          <p className="text-sm text-[var(--color-text-secondary)]">
            Changing the embedding model to <span className="font-medium text-[var(--color-text-primary)]">{pendingEmbeddingChange?.label}</span> will
            re-embed all atoms. This may take a while and will use API credits.
          </p>
        </Modal>
      </div>
    </div>,
    document.body
  );
}
