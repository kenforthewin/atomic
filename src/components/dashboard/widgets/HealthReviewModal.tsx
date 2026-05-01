import { useState, useEffect, useCallback } from 'react';
import { createPortal } from 'react-dom';
import {
  X, GitMerge, Link, Loader2, CheckCircle,
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

type PairAction = 'merge_with_llm' | 'keep_both';
type PairStatus = 'idle' | 'loading' | 'done' | 'error';

// Atom preview (content_quality + boilerplate)
interface AtomPreview {
  id: string;
  title: string;
  created_at?: string;
}

// Boilerplate atom entry
interface BoilerplateEntry {
  id: string;
  title: string;
  clone_count: number;
}

// Contradiction pair
interface ContradictionPair {
  pair_id: string;
  atom_a: { id: string; title: string; source?: string };
  atom_b: { id: string; title: string; source?: string };
  similarity: number;
  shared_tag_count: number;
}

// Rootless tag
interface RootlessTag {
  id: string;
  name: string;
  atom_count: number;
}

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

function BoilerplateSection({ atoms }: { atoms: BoilerplateEntry[] }) {
  const [reembedStatus, setReembedStatus] = useState<Record<string, 'idle' | 'loading' | 'done' | 'error'>>({});

  const reembed = async (atomId: string) => {
    setReembedStatus(prev => ({ ...prev, [atomId]: 'loading' }));
    try {
      await getTransport().invoke('retry_embedding', { atomId });
      setReembedStatus(prev => ({ ...prev, [atomId]: 'done' }));
    } catch {
      setReembedStatus(prev => ({ ...prev, [atomId]: 'error' }));
    }
  };

  if (atoms.length === 0) {
    return <p className="text-xs text-gray-500 text-center py-8">No boilerplate pollution detected — all clear</p>;
  }

  return (
    <div className="space-y-3">
      <div className="bg-[#1e1a00] border border-yellow-900/30 rounded p-3 space-y-1.5">
        <p className="text-xs text-yellow-300/90 font-medium">Embedding quality issue</p>
        <p className="text-xs text-gray-400 leading-relaxed">
          These {atoms.length} atom{atoms.length !== 1 ? 's' : ''} share identical boilerplate sections
          that dominate their embeddings — semantic search cannot reliably distinguish them from
          each other. Edit each atom to remove or uniquify the boilerplate sections, then re-embed.
        </p>
      </div>
      <div className="space-y-2">
        {atoms
          .slice()
          .sort((a, b) => b.clone_count - a.clone_count)
          .map(atom => {
            const status = reembedStatus[atom.id] ?? 'idle';
            return (
              <div
                key={atom.id}
                className="flex items-center gap-3 p-2.5 bg-[#1e1e1e] rounded border border-white/5"
              >
                <div className="flex-1 min-w-0">
                  <p className="text-xs text-gray-200 truncate">
                    {atom.title || <span className="text-gray-500 italic">Untitled atom</span>}
                  </p>
                  <p className="text-xs text-gray-600 mt-0.5">
                    {atom.clone_count} near-identical edge{atom.clone_count !== 1 ? 's' : ''}
                  </p>
                </div>
                <div className="shrink-0">
                  {status === 'done' ? (
                    <span className="flex items-center gap-1 text-xs text-green-500">
                      <CheckCircle className="w-3 h-3" /> Queued
                    </span>
                  ) : status === 'error' ? (
                    <span className="text-xs text-red-400">Failed</span>
                  ) : (
                    <button
                      disabled={status === 'loading'}
                      onClick={() => reembed(atom.id)}
                      title="Reset embedding so it will be re-processed on next pipeline run"
                      className="flex items-center gap-1 px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40"
                    >
                      {status === 'loading'
                        ? <Loader2 className="w-3 h-3 animate-spin" />
                        : <RefreshCw className="w-3 h-3" />}
                      Re-embed
                    </button>
                  )}
                </div>
              </div>
            );
          })}
      </div>
    </div>
  );
}

// ==================== Contradiction section ====================

function ContradictionRow({ pair }: { pair: ContradictionPair }) {
  const [expanded, setExpanded] = useState(false);
  const [contents, setContents] = useState<[string, string] | null>(null);
  const [loadingContent, setLoadingContent] = useState(false);

  const toggleExpand = async () => {
    if (!expanded && !contents) {
      setLoadingContent(true);
      try {
        const [a, b] = await Promise.all([
          getTransport().invoke<{ content: string }>('get_atom', { id: pair.atom_a.id }),
          getTransport().invoke<{ content: string }>('get_atom', { id: pair.atom_b.id }),
        ]);
        setContents([a.content, b.content]);
      } catch {
        setContents(['(Failed to load)', '(Failed to load)']);
      } finally {
        setLoadingContent(false);
      }
    }
    setExpanded(v => !v);
  };

  const simPct = Math.round(pair.similarity * 100);
  const simColor = simPct >= 88 ? 'text-orange-400' : 'text-yellow-400';

  return (
    <div className="bg-[#1e1e1e] rounded border border-white/5">
      <div className="p-3 space-y-2.5">
        <div className="flex items-center justify-between gap-2">
          <span className={`text-xs font-medium ${simColor}`}>{simPct}% similarity</span>
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
                : expanded ? <ChevronUp className="w-3.5 h-3.5" /> : <ChevronDown className="w-3.5 h-3.5" />}
              {!expanded && !loadingContent && 'Compare'}
            </button>
          </div>
        </div>

        <div className="grid grid-cols-2 gap-3">
          {[pair.atom_a, pair.atom_b].map((atom, i) => (
            <div key={i} className="min-w-0">
              <p className="text-xs text-gray-200 line-clamp-2 leading-snug">{atom.title}</p>
              {atom.source && (
                <p className="text-xs text-gray-600 truncate mt-0.5">
                  {(() => { try { return new URL(atom.source).hostname; } catch { return atom.source; } })()}
                </p>
              )}
            </div>
          ))}
        </div>

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
      </div>
    </div>
  );
}

function ContradictionSection({ data }: { data: Record<string, unknown> }) {
  const pairs = (data.pairs as ContradictionPair[] | undefined) ?? [];
  const count = (data.potential_contradictions as number) ?? 0;

  if (pairs.length === 0) {
    return (
      <p className="text-xs text-gray-500 text-center py-8">No contradiction candidates — all clear</p>
    );
  }

  return (
    <div className="space-y-3">
      <div className="bg-[#1a1a2e] border border-purple-900/30 rounded p-3 space-y-1.5">
        <p className="text-xs text-purple-300/90 font-medium">Contradiction candidates</p>
        <p className="text-xs text-gray-400 leading-relaxed">
          {count} atom pair{count !== 1 ? 's' : ''} cover the same topic but may contain
          conflicting information (similarity 80–92%). Compare their content and merge or
          update them to align. Use <strong className="text-gray-300">Compare</strong> to view
          both atoms side-by-side.
        </p>
      </div>
      <div className="space-y-2">
        {pairs.map(pair => (
          <ContradictionRow key={pair.pair_id} pair={pair} />
        ))}
      </div>
    </div>
  );
}

// ==================== Content quality (no-source) section ====================

function ContentQualitySection({ data }: { data: Record<string, unknown> }) {
  const issues = data.issues as Record<string, {
    count: number;
    atoms?: Array<{ id: string; title: string; created_at?: string } | string>;
  }> | undefined;

  const noSourceItems = (issues?.no_source?.atoms ?? []) as Array<{ id: string; title: string; created_at?: string }>;
  const noSourceCount = issues?.no_source?.count ?? noSourceItems.length;

  if (noSourceCount === 0) {
    return <p className="text-xs text-gray-500 text-center py-8">No unsourced atoms — all clear</p>;
  }

  return (
    <div className="space-y-3">
      <div className="bg-[#1a1a1a] border border-white/5 rounded p-3 space-y-1.5">
        <p className="text-xs text-gray-300 font-medium">
          {noSourceCount} atom{noSourceCount !== 1 ? 's' : ''} missing a source URL
        </p>
        <p className="text-xs text-gray-400 leading-relaxed">
          These atoms have no <code className="text-gray-300 bg-[#2a2a2a] px-1 rounded">source_url</code>{' '}
          and no URL or{' '}
          <code className="text-gray-300 bg-[#2a2a2a] px-1 rounded">Source:</code> line in their content.
          Open each atom in the editor and add a source URL to resolve.
        </p>
      </div>
      <div className="space-y-1.5">
        {noSourceItems.map(atom => (
          <div
            key={atom.id}
            className="flex items-center gap-3 p-2.5 bg-[#1e1e1e] rounded border border-white/5"
          >
            <div className="flex-1 min-w-0">
              <p className="text-xs text-gray-200 truncate">
                {atom.title || <span className="italic text-gray-500">Untitled atom</span>}
              </p>
              {atom.created_at && (
                <p className="text-xs text-gray-600 mt-0.5">
                  Created {new Date(atom.created_at).toLocaleDateString()}
                </p>
              )}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ==================== Tag health (rootless) section ====================

function TagHealthSection({ data }: { data: Record<string, unknown> }) {
  const rootlessList = (data.rootless_tag_list as RootlessTag[] | undefined) ?? [];
  const rootlessCount = (data.rootless_tags as number) ?? rootlessList.length;
  const similarCount = (data.similar_name_pairs as number) ?? 0;

  return (
    <div className="space-y-4">
      {rootlessList.length > 0 && (
        <div className="space-y-2">
          <div className="bg-[#1a1a1a] border border-white/5 rounded p-3 space-y-1">
            <p className="text-xs text-gray-300 font-medium">
              {rootlessCount} root-level tag{rootlessCount !== 1 ? 's' : ''} with no parent
            </p>
            <p className="text-xs text-gray-500 leading-relaxed">
              These tags sit at the top level. Consider nesting them under a relevant
              category to keep the tag tree navigable.
            </p>
          </div>
          <div className="space-y-1.5">
            {rootlessList
              .slice()
              .sort((a, b) => b.atom_count - a.atom_count)
              .map(tag => (
                <div
                  key={tag.id}
                  className="flex items-center justify-between p-2.5 bg-[#1e1e1e] rounded border border-white/5"
                >
                  <div className="min-w-0">
                    <p className="text-xs text-gray-200 truncate font-medium">{tag.name}</p>
                    <p className="text-xs text-gray-600 mt-0.5">
                      {tag.atom_count} atom{tag.atom_count !== 1 ? 's' : ''}
                    </p>
                  </div>
                </div>
              ))}
          </div>
        </div>
      )}

      {similarCount > 0 && (
        <div className="bg-[#1a1a1a] border border-white/5 rounded p-3 space-y-1">
          <p className="text-xs text-gray-300 font-medium">
            {similarCount} similar-name pair{similarCount !== 1 ? 's' : ''}
          </p>
          <p className="text-xs text-gray-500 leading-relaxed">
            Tags with near-identical names (e.g. "React" and "ReactJS") may be duplicates.
            Review and merge in the tag tree if needed.
          </p>
        </div>
      )}

      {rootlessList.length === 0 && similarCount === 0 && (
        <p className="text-xs text-gray-500 text-center py-8">Tag structure is healthy — all clear</p>
      )}
    </div>
  );
}

// ==================== Main modal ====================

interface Props {
  report: {
    checks: Record<string, {
      data: Record<string, unknown>;
    }>;
  };
  checkName?: string;
  onClose: () => void;
  onResolved: () => void;
}

export function HealthReviewModal({ report, checkName, onClose, onResolved }: Props) {
  const overlapPairs: OverlapPair[] =
    (report.checks['content_overlap']?.data?.pairs as OverlapPair[]) ?? [];
  const boilerplateAtoms: BoilerplateEntry[] =
    (report.checks['boilerplate_pollution']?.data?.affected_atoms as BoilerplateEntry[] | undefined) ?? [];
  const contradictionData: Record<string, unknown> | null =
    (report.checks['contradiction_detection']?.data ?? null) as Record<string, unknown> | null;
  const contradictionCount = (contradictionData?.potential_contradictions as number ?? 0);
  const contentQualityData: Record<string, unknown> | null =
    (report.checks['content_quality']?.data ?? null) as Record<string, unknown> | null;
  const noSourceCount = (() => {
    const issues = contentQualityData?.issues as Record<string, { count?: number }> | undefined;
    return issues?.no_source?.count ?? 0;
  })();
  const tagHealthData: Record<string, unknown> | null =
    (report.checks['tag_health']?.data ?? null) as Record<string, unknown> | null;
  const rootlessCount = (tagHealthData?.rootless_tags as number) ?? 0;

  const tabs = [
    ...(overlapPairs.length > 0        ? [{ key: 'content_overlap',        label: 'Content overlap', count: overlapPairs.length }] : []),
    ...(boilerplateAtoms.length > 0    ? [{ key: 'boilerplate',             label: 'Boilerplate',     count: boilerplateAtoms.length }] : []),
    ...(contradictionCount > 0         ? [{ key: 'contradiction_detection', label: 'Contradictions',  count: contradictionCount }] : []),
    ...(noSourceCount > 0              ? [{ key: 'content_quality',         label: 'No source',       count: noSourceCount }] : []),
    ...(rootlessCount > 0              ? [{ key: 'tag_health',              label: 'Tag structure',   count: rootlessCount }] : []),
  ];

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
            <BoilerplateSection atoms={boilerplateAtoms} />
          )}

          {activeTab === 'contradiction_detection' && contradictionData && (
            <ContradictionSection data={contradictionData} />
          )}

          {activeTab === 'content_quality' && contentQualityData && (
            <ContentQualitySection data={contentQualityData} />
          )}

          {activeTab === 'tag_health' && tagHealthData && (
            <TagHealthSection data={tagHealthData} />
          )}

        </div>
      </div>
    </div>,
    document.body,
  );
}
