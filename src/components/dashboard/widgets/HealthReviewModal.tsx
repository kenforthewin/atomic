import { useState, useEffect, useCallback } from 'react';
import { createPortal } from 'react-dom';
import {
  X, GitMerge, Link, Trash2, Loader2, CheckCircle,
  ChevronDown, ChevronUp, ExternalLink, RefreshCw,
} from 'lucide-react';
import { getTransport } from '../../../lib/transport';

// ==================== Types ====================

export interface OverlapPair {
  pair_id: string;
  atom_a: { id: string; title: string; source?: string };
  atom_b: { id: string; title: string; source?: string };
  similarity: number;
  shared_tag_count: number;
  available_actions: string[];
}

interface AtomDetail {
  id: string;
  content: string;
  source_url?: string;
}

type PairAction = 'merge_with_llm' | 'keep_both' | 'delete_older';
type PairStatus = 'idle' | 'loading' | 'done' | 'error';

// ==================== Helpers ====================

function sourceLabel(source?: string): string {
  if (!source) return 'manual';
  try { return new URL(source).hostname; } catch { return source.split('/').slice(0, 2).join('/'); }
}

function similarityLabel(s: number): { text: string; color: string } {
  if (s >= 0.80) return { text: `${(s * 100).toFixed(0)}% overlap`, color: 'text-orange-400' };
  if (s >= 0.65) return { text: `${(s * 100).toFixed(0)}% overlap`, color: 'text-yellow-400' };
  return { text: `${(s * 100).toFixed(0)}% overlap`, color: 'text-gray-400' };
}

// ==================== Overlap pair row ====================

function PairRow({
  pair,
  onApply,
}: {
  pair: OverlapPair;
  onApply: (pair: OverlapPair, action: PairAction) => Promise<void>;
}) {
  const [status, setStatus] = useState<PairStatus>('idle');
  const [appliedAction, setAppliedAction] = useState<PairAction | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [contents, setContents] = useState<[string, string] | null>(null);
  const [loadingContent, setLoadingContent] = useState(false);
  const sim = similarityLabel(pair.similarity);

  const apply = async (action: PairAction) => {
    setStatus('loading');
    setAppliedAction(action);
    setError(null);
    try {
      await onApply(pair, action);
      setStatus('done');
    } catch (e) {
      setStatus('error');
      setError(e instanceof Error ? e.message : 'Action failed');
    }
  };

  const toggleExpand = async () => {
    if (!expanded && !contents) {
      setLoadingContent(true);
      try {
        const [a, b] = await Promise.all([
          getTransport().invoke<AtomDetail>('get_atom', { id: pair.atom_a.id }),
          getTransport().invoke<AtomDetail>('get_atom', { id: pair.atom_b.id }),
        ]);
        setContents([a.content, b.content]);
      } finally {
        setLoadingContent(false);
      }
    }
    setExpanded(v => !v);
  };

  if (status === 'done') {
    const labels: Record<PairAction, string> = {
      merge_with_llm: 'Merged — LLM synthesised both atoms into one',
      keep_both: 'Kept both — no changes made',
      delete_older: 'Older atom deleted',
    };
    return (
      <div className="flex items-center gap-2 p-3 rounded border border-white/5 bg-[#1e1e1e] text-xs text-gray-500">
        <CheckCircle className="w-3.5 h-3.5 text-green-500 shrink-0" />
        {labels[appliedAction!]}
      </div>
    );
  }

  return (
    <div className="bg-[#1e1e1e] rounded border border-white/5">
      <div className="p-3 space-y-2.5">
        {/* Header row */}
        <div className="flex items-center justify-between gap-2">
          <span className={`text-xs font-medium ${sim.color}`}>{sim.text}</span>
          <div className="flex items-center gap-2 text-xs text-gray-600">
            {pair.shared_tag_count > 0 && (
              <span>{pair.shared_tag_count} shared tag{pair.shared_tag_count !== 1 ? 's' : ''}</span>
            )}
            <button
              onClick={toggleExpand}
              className="flex items-center gap-0.5 hover:text-gray-400 transition-colors"
            >
              {loadingContent
                ? <Loader2 className="w-3.5 h-3.5 animate-spin" />
                : expanded ? <ChevronUp className="w-3.5 h-3.5" /> : <ChevronDown className="w-3.5 h-3.5" />
              }
              {!expanded && !loadingContent && 'Compare'}
            </button>
          </div>
        </div>

        {/* Atom summaries */}
        <div className="grid grid-cols-2 gap-3">
          {[pair.atom_a, pair.atom_b].map((atom, i) => (
            <div key={i} className="min-w-0">
              <p className="text-xs text-gray-200 line-clamp-2 leading-snug">{atom.title}</p>
              <p className="text-xs text-gray-600 truncate mt-0.5">{sourceLabel(atom.source)}</p>
            </div>
          ))}
        </div>

        {/* Side-by-side content */}
        {expanded && contents && (
          <div className="grid grid-cols-2 gap-2 border-t border-white/5 pt-2">
            {[pair.atom_a, pair.atom_b].map((atom, i) => (
              <div key={i} className="space-y-1 min-w-0">
                <p className="text-xs text-gray-500 truncate">{atom.title}</p>
                <pre className="text-xs text-gray-400 bg-[#161616] rounded p-2 max-h-56 overflow-y-auto whitespace-pre-wrap leading-relaxed font-sans">
                  {contents[i as 0 | 1]}
                </pre>
              </div>
            ))}
          </div>
        )}

        {/* Error */}
        {error && <p className="text-xs text-red-400">{error}</p>}

        {/* Actions */}
        <div className="flex gap-1.5 flex-wrap">
          <ActionBtn
            icon={<GitMerge className="w-3 h-3" />}
            label="Merge"
            title="LLM synthesises both into one atom, preserving all unique content"
            loading={status === 'loading' && appliedAction === 'merge_with_llm'}
            disabled={status === 'loading'}
            onClick={() => apply('merge_with_llm')}
          />
          <ActionBtn
            icon={<Link className="w-3 h-3" />}
            label="Keep both"
            title="Leave both atoms — different perspectives on the same topic"
            loading={status === 'loading' && appliedAction === 'keep_both'}
            disabled={status === 'loading'}
            onClick={() => apply('keep_both')}
          />
          <ActionBtn
            icon={<Trash2 className="w-3 h-3" />}
            label="Delete older"
            title="Delete the older atom"
            loading={status === 'loading' && appliedAction === 'delete_older'}
            disabled={status === 'loading'}
            variant="danger"
            onClick={() => apply('delete_older')}
          />
        </div>
      </div>
    </div>
  );
}

function ActionBtn({
  icon, label, title, loading, disabled, onClick, variant = 'default',
}: {
  icon: React.ReactNode; label: string; title: string;
  loading: boolean; disabled: boolean; onClick: () => void;
  variant?: 'default' | 'danger';
}) {
  return (
    <button
      title={title}
      disabled={disabled}
      onClick={onClick}
      className={[
        'flex items-center gap-1 px-2 py-1 rounded text-xs transition-colors',
        'disabled:opacity-40 disabled:cursor-not-allowed',
        variant === 'danger'
          ? 'bg-[#2a1a1a] hover:bg-red-900/30 text-red-400 border border-red-900/20'
          : 'bg-[#2a2a2a] hover:bg-[#333] text-gray-300 border border-white/5',
      ].join(' ')}
    >
      {loading ? <Loader2 className="w-3 h-3 animate-spin" /> : icon}
      {label}
    </button>
  );
}

// ==================== Boilerplate section ====================

interface BoilerplateAtom {
  id: string;
  title: string;
  source_url: string | null;
  reembedStatus: 'idle' | 'loading' | 'done' | 'error';
}

function BoilerplateSection({ atomIds }: { atomIds: string[] }) {
  const [atoms, setAtoms] = useState<BoilerplateAtom[]>([]);
  const [loadingAtoms, setLoadingAtoms] = useState(true);

  useEffect(() => {
    let cancelled = false;
    const fetchAll = async () => {
      setLoadingAtoms(true);
      const results = await Promise.allSettled(
        atomIds.map(id => getTransport().invoke<{ id: string; content: string; source_url?: string }>('get_atom', { id }))
      );
      if (cancelled) return;
      setAtoms(results.map((r, i) => {
        if (r.status === 'fulfilled') {
          const first_line = r.value.content.split('\n').find(l => l.trim()) ?? atomIds[i];
          const title = first_line.replace(/^#+\s*/, '').trim().slice(0, 80);
          return { id: atomIds[i], title, source_url: r.value.source_url ?? null, reembedStatus: 'idle' };
        }
        return { id: atomIds[i], title: atomIds[i], source_url: null, reembedStatus: 'idle' };
      }));
      setLoadingAtoms(false);
    };
    fetchAll();
    return () => { cancelled = true; };
  }, [atomIds]);

  const reembed = async (atomId: string) => {
    setAtoms(prev => prev.map(a => a.id === atomId ? { ...a, reembedStatus: 'loading' } : a));
    try {
            await getTransport().invoke('retry_embedding', { atomId: atomId });
      setAtoms(prev => prev.map(a => a.id === atomId ? { ...a, reembedStatus: 'done' } : a));
    } catch {
      setAtoms(prev => prev.map(a => a.id === atomId ? { ...a, reembedStatus: 'error' } : a));
    }
  };

  if (loadingAtoms) {
    return (
      <div className="flex items-center justify-center py-8">
        <Loader2 className="w-4 h-4 animate-spin text-gray-600" />
      </div>
    );
  }

  return (
    <div className="space-y-3">
      <div className="bg-[#1e1a00] border border-yellow-900/30 rounded p-3 space-y-1.5">
        <p className="text-xs text-yellow-300/90 font-medium">Embedding quality issue</p>
        <p className="text-xs text-gray-400 leading-relaxed">
          These {atomIds.length} atoms share identical boilerplate sections that dominate their
          embeddings — semantic search cannot reliably distinguish them from each other.
          Edit each atom to remove or uniquify the boilerplate sections, then re-embed.
        </p>
      </div>

      <div className="space-y-2">
        {atoms.map(atom => (
          <div
            key={atom.id}
            className="flex items-center gap-3 p-2.5 bg-[#1e1e1e] rounded border border-white/5"
          >
            <div className="flex-1 min-w-0">
              <p className="text-xs text-gray-200 truncate">{atom.title}</p>
              {atom.source_url && (
                <p className="text-xs text-gray-600 truncate mt-0.5">{sourceLabel(atom.source_url)}</p>
              )}
            </div>
            <div className="flex items-center gap-1.5 shrink-0">
              {atom.source_url && (
                <a
                  href={atom.source_url}
                  target="_blank"
                  rel="noopener noreferrer"
                  title="Open original source"
                  className="flex items-center gap-1 px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors"
                >
                  <ExternalLink className="w-3 h-3" />
                  Source
                </a>
              )}
              {atom.reembedStatus === 'done' ? (
                <span className="flex items-center gap-1 text-xs text-green-500">
                  <CheckCircle className="w-3 h-3" /> Queued
                </span>
              ) : (
                <button
                  disabled={atom.reembedStatus === 'loading'}
                  onClick={() => reembed(atom.id)}
                  title="Reset embedding so it will be re-processed on next pipeline run"
                  className="flex items-center gap-1 px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40"
                >
                  {atom.reembedStatus === 'loading'
                    ? <Loader2 className="w-3 h-3 animate-spin" />
                    : <RefreshCw className="w-3 h-3" />}
                  Re-embed
                </button>
              )}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ==================== Modal ====================

interface Props {
  report: {
    checks: Record<string, {
      data: Record<string, unknown>;
    }>;
  };
  checkName?: string;  // If provided, pre-select this tab on open
  onClose: () => void;
  onResolved: () => void;
}

export function HealthReviewModal({ report, checkName, onClose, onResolved }: Props) {
  // Compute once — stable references for the lifetime of this modal mount
  const overlapPairs: OverlapPair[] =
    (report.checks['content_overlap']?.data?.pairs as OverlapPair[]) ?? [];
  const boilerplateIds: string[] =
    (report.checks['boilerplate_pollution']?.data?.affected_atoms as string[]) ?? [];

  // Build tab list from available data
  const tabs = [
    ...(overlapPairs.length > 0 ? [{ key: 'content_overlap', label: 'Content overlap', count: overlapPairs.length }] : []),
    ...(boilerplateIds.length > 0 ? [{ key: 'boilerplate', label: 'Boilerplate', count: boilerplateIds.length }] : []),
  ];

  // selectedTab = user choice; falls back to first available tab
  const [selectedTab, setSelectedTab] = useState<string | null>(checkName ?? null);
  const activeTab = tabs.find(t => t.key === selectedTab)?.key ?? tabs[0]?.key ?? null;

  const [resolvedCount, setResolvedCount] = useState(0);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', handler);
    document.body.style.overflow = 'hidden';
    return () => {
      document.removeEventListener('keydown', handler);
      document.body.style.overflow = '';
    };
  }, [onClose]);

  const applyPairFix = useCallback(async (pair: OverlapPair, action: PairAction) => {
    if (action === 'keep_both') {
      setResolvedCount(n => n + 1);
      return;
    }
    const itemId = `${pair.atom_a.id}_${pair.atom_b.id}`;
    await getTransport().invoke('apply_health_item_fix', {
      check: 'duplicate_detection',
      item_id: itemId,
      action,
    });
    setResolvedCount(n => n + 1);
    onResolved();
  }, [onResolved]);

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-start justify-end bg-black/50 backdrop-blur-sm"
      onClick={e => { if (e.target === e.currentTarget) onClose(); }}
    >
      <div className="h-full w-full max-w-2xl bg-[#1a1a1a] flex flex-col shadow-2xl border-l border-white/5 animate-in slide-in-from-right duration-200">

        {/* Header */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-white/5 shrink-0">
          <div>
            <h2 className="text-sm font-semibold text-white">Review Queue</h2>
            <p className="text-xs text-gray-500 mt-0.5">
              {resolvedCount > 0
                ? `${resolvedCount} resolved this session`
                : 'Items that need a judgment call'}
            </p>
          </div>
          <button onClick={onClose} className="text-gray-500 hover:text-gray-300 transition-colors p-1">
            <X className="w-5 h-5" />
          </button>
        </div>

        {/* Tabs */}
        {tabs.length > 1 && (
          <div className="flex border-b border-white/5 shrink-0">
            {tabs.map(t => (
              <button
                key={t.key}
                onClick={() => setSelectedTab(t.key)}
                className={[
                  'px-4 py-2.5 text-xs font-medium transition-colors border-b-2',
                  activeTab === t.key
                    ? 'border-purple-500 text-white'
                    : 'border-transparent text-gray-500 hover:text-gray-300',
                ].join(' ')}
              >
                {t.label}
                <span className="ml-1.5 px-1.5 py-0.5 bg-[#2a2a2a] rounded text-gray-400">{t.count}</span>
              </button>
            ))}
          </div>
        )}

        {/* Content */}
        <div className="flex-1 overflow-y-auto p-5 space-y-3">

          {activeTab === null && (
            <p className="text-xs text-gray-500 text-center py-8">Nothing to review — all clear</p>
          )}

          {activeTab === 'content_overlap' && (
            <>
              <p className="text-xs text-gray-500 leading-relaxed">
                Atoms from different sources with 55–85% similarity and at least 2 shared tags.
                These likely cover the same topic from different angles.
                Use <strong className="text-gray-300">Keep both</strong> for complementary perspectives,{' '}
                <strong className="text-gray-300">Merge</strong> for true duplicates.
              </p>
              {overlapPairs.map(pair => (
                <PairRow key={pair.pair_id} pair={pair} onApply={applyPairFix} />
              ))}
            </>
          )}

          {activeTab === 'boilerplate' && (
            <BoilerplateSection atomIds={boilerplateIds} />
          )}

        </div>
      </div>
    </div>,
    document.body,
  );
}
