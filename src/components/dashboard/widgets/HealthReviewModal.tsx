import { useState, useEffect, useCallback, useMemo, useRef } from 'react';
import { createPortal } from 'react-dom';
import {
  X, GitMerge, Loader2, CheckCircle,
  ChevronDown, ChevronUp, RefreshCw, ChevronLeft, ChevronRight, Check, Clipboard,
} from 'lucide-react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { getTransport } from '../../../lib/transport';
import { useTagsStore } from '../../../stores/tags';
import { useDatabasesStore } from '../../../stores/databases';
import { NoSourceRow } from './review/NoSourceRow';
import { TagRootlessRow } from './review/TagRootlessRow';
import { BoilerplateAtomRow } from './review/BoilerplateAtomRow';
import { BrokenLinksSection } from './review/BrokenLinksSection';
import { sourceTrust, relativeAge } from './review/badges';
import { lineDiff, type DiffPart } from './review/diffUtil';

// ==================== Types ====================

export interface OverlapPair {
  pair_id: string;
  atom_a: { id: string; title: string; source?: string; created_at?: string };
  atom_b: { id: string; title: string; source?: string; created_at?: string };
  similarity: number;
  shared_tag_count: number;
  available_actions: string[];
}

interface AtomDetail {
  id: string;
  content: string;
  source_url?: string;
}

type PairAction = 'merge_with_llm' | 'keep_a' | 'keep_b' | 'merge_with_edited_content';
type PairStatus = 'idle' | 'loading' | 'done' | 'error';


// Boilerplate atom entry
interface BoilerplateEntry {
  id: string;
  title: string;
  clone_count: number;
}

// Contradiction pair
interface ContradictionPair {
  pair_id: string;
  atom_a: { id: string; title: string; source?: string; created_at?: string };
  atom_b: { id: string; title: string; source?: string; created_at?: string };
  similarity: number;
  shared_tag_count: number;
}

// Rootless tag
interface RootlessTag {
  id: string;
  name: string;
  atom_count: number;
}

// ==================== localStorage helpers ====================

function todayKey(): string {
  const d = new Date();
  return `${d.getFullYear()}-${(d.getMonth() + 1).toString().padStart(2, '0')}-${d.getDate().toString().padStart(2, '0')}`;
}

interface ResolvedRecord {
  date: string;
  counts: Record<string, number>;
}

function loadResolved(dbId: string): ResolvedRecord {
  try {
    const raw = localStorage.getItem(`health-resolved:${dbId}`);
    if (!raw) return { date: todayKey(), counts: {} };
    const parsed = JSON.parse(raw) as ResolvedRecord;
    if (parsed.date !== todayKey()) return { date: todayKey(), counts: {} };
    return parsed;
  } catch {
    return { date: todayKey(), counts: {} };
  }
}

function saveResolved(dbId: string, rec: ResolvedRecord): void {
  try {
    localStorage.setItem(`health-resolved:${dbId}`, JSON.stringify(rec));
  } catch { /* ignore quota errors */ }
}


function similarityLabel(s: number): { text: string; color: string } {
  if (s >= 0.80) return { text: `${(s * 100).toFixed(0)}% overlap`, color: 'text-orange-400' };
  if (s >= 0.65) return { text: `${(s * 100).toFixed(0)}% overlap`, color: 'text-yellow-400' };
  return { text: `${(s * 100).toFixed(0)}% overlap`, color: 'text-gray-400' };
}

// ==================== Tab header ====================

function TabHeader({
  label,
  scannedAt,
  rescanning,
  onRescan,
  resolvedToday,
  initialQueueSize,
}: {
  label: string;
  scannedAt: string | undefined;
  rescanning: boolean;
  onRescan: () => void;
  resolvedToday: number;
  initialQueueSize: number;
}) {
  const [, forceTick] = useState(0);
  useEffect(() => {
    if (!scannedAt) return;
    const id = window.setInterval(() => forceTick(n => n + 1), 30_000);
    return () => window.clearInterval(id);
  }, [scannedAt]);

  const rel = useMemo(() => {
    if (!scannedAt) return 'not scanned yet';
    const delta = Date.now() - new Date(scannedAt).getTime();
    const mins = Math.round(delta / 60_000);
    if (mins < 1) return 'just now';
    if (mins < 60) return `${mins}m ago`;
    const hrs = Math.round(mins / 60);
    if (hrs < 24) return `${hrs}h ago`;
    return `${Math.round(hrs / 24)}d ago`;
  }, [scannedAt]);

  const progressPct = initialQueueSize > 0
    ? Math.min(100, Math.round((resolvedToday / initialQueueSize) * 100))
    : 0;

  return (
    <div className="flex items-center justify-between gap-3 pb-2 border-b border-white/5">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="text-xs text-gray-300 font-medium truncate">{label}</span>
          {resolvedToday > 0 && (
            <span className="text-xs text-green-400">• {resolvedToday} resolved today</span>
          )}
        </div>
        {initialQueueSize > 0 && resolvedToday > 0 && (
          <div className="mt-1.5 h-1 bg-[#2a2a2a] rounded overflow-hidden">
            <div
              className="h-full bg-green-500/60 transition-all duration-500"
              style={{ width: `${progressPct}%` }}
            />
          </div>
        )}
      </div>
      <button
        type="button"
        onClick={onRescan}
        disabled={rescanning}
        className="shrink-0 inline-flex items-center gap-1 px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40"
        title="Re-run this check against current data"
      >
        {rescanning
          ? <Loader2 className="w-3 h-3 animate-spin" />
          : <RefreshCw className="w-3 h-3" />}
        <span>{rescanning ? 'Scanning…' : 'Re-scan'}</span>
      </button>
      {scannedAt && !rescanning && (
        <span className="shrink-0 text-xs text-gray-600">{rel}</span>
      )}
    </div>
  );
}

// ==================== Overlap pair row ====================

function DiffView({ a, b }: { a: string; b: string }) {
  const parts = useMemo(() => lineDiff(a, b), [a, b]);
  return (
    <pre className="text-xs bg-[#161616] rounded p-2 max-h-72 overflow-y-auto whitespace-pre-wrap leading-relaxed font-sans border-t border-white/5">
      {parts.map((p: DiffPart, i: number) => (
        <span
          key={i}
          className={
            p.type === 'insert' ? 'bg-green-900/30 text-green-300' :
            p.type === 'delete' ? 'bg-red-900/30 text-red-300' :
            'text-gray-400'
          }
        >{p.text}</span>
      ))}
    </pre>
  );
}

function PairRow({
  pair,
  onResolve,
}: {
  pair: OverlapPair;
  onResolve: (pair: OverlapPair) => void;
}) {
  const [status, setStatus] = useState<PairStatus>('idle');
  const [appliedAction, setAppliedAction] = useState<PairAction | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState(false);
  const [contents, setContents] = useState<[string, string] | null>(null);
  const [loadingContent, setLoadingContent] = useState(false);
  const [mergeOpen, setMergeOpen] = useState(false);
  const [mergeDraft, setMergeDraft] = useState('');
  const [diffMode, setDiffMode] = useState(false);
  const sim = similarityLabel(pair.similarity);

  function buildDraft(a: string, b: string, titleA: string, titleB: string): string {
    return `# ${titleA}\n\n${a}\n\n---\n\n# ${titleB}\n\n${b}`.trim();
  }

  const fetchContents = async () => {
    setLoadingContent(true);
    try {
      const [a, b] = await Promise.all([
        getTransport().invoke<AtomDetail>('get_atom', { id: pair.atom_a.id }),
        getTransport().invoke<AtomDetail>('get_atom', { id: pair.atom_b.id }),
      ]);
      setContents([a.content, b.content]);
      return [a.content, b.content] as [string, string];
    } finally {
      setLoadingContent(false);
    }
  };

  const toggleExpand = async () => {
    if (!expanded && !contents) {
      await fetchContents();
    }
    setExpanded(v => !v);
  };

  const openMerge = async () => {
    let c = contents;
    if (!c) {
      c = await fetchContents();
    }
    if (c) {
      setMergeDraft(prev => prev || buildDraft(c![0], c![1], pair.atom_a.title, pair.atom_b.title));
    }
    setMergeOpen(true);
  };

  const applyDirect = async (action: 'keep_a' | 'keep_b') => {
    setStatus('loading');
    setAppliedAction(action);
    setError(null);
    try {
      await getTransport().invoke('apply_health_item_fix', {
        check: 'content_overlap',
        item_id: `${pair.atom_a.id <= pair.atom_b.id ? pair.atom_a.id : pair.atom_b.id}__${pair.atom_a.id <= pair.atom_b.id ? pair.atom_b.id : pair.atom_a.id}`,
        action,
      });
      setStatus('done');
      onResolve(pair);
    } catch (e) {
      setStatus('error');
      setError(e instanceof Error ? e.message : 'Action failed');
    }
  };

  const applyEditedMerge = async () => {
    const aDate = pair.atom_a.created_at ? Date.parse(pair.atom_a.created_at) : 0;
    const bDate = pair.atom_b.created_at ? Date.parse(pair.atom_b.created_at) : 0;
    const [winner, loser] = aDate >= bDate
      ? [pair.atom_a.id, pair.atom_b.id]
      : [pair.atom_b.id, pair.atom_a.id];
    setStatus('loading');
    setAppliedAction('merge_with_edited_content');
    setError(null);
    try {
      await getTransport().invoke('apply_health_item_fix', {
        check: 'content_overlap',
        item_id: `${pair.atom_a.id <= pair.atom_b.id ? pair.atom_a.id : pair.atom_b.id}__${pair.atom_a.id <= pair.atom_b.id ? pair.atom_b.id : pair.atom_a.id}`,
        action: 'merge_with_edited_content',
        winner_atom_id: winner,
        loser_atom_id: loser,
        content: mergeDraft,
      });
      setStatus('done');
      onResolve(pair);
    } catch (e) {
      setStatus('error');
      setError(e instanceof Error ? e.message : 'Merge failed');
    }
  };

  if (status === 'done') {
    const labels: Record<string, string> = {
      merge_with_llm: 'Merged — LLM synthesised both atoms into one',
      merge_with_edited_content: 'Merged — edited content applied',
      keep_a: 'Kept A; removed B',
      keep_b: 'Kept B; removed A',
    };
    return (
      <div className="flex items-center gap-2 p-3 rounded border border-white/5 bg-[#1e1e1e] text-xs text-gray-500">
        <CheckCircle className="w-3.5 h-3.5 text-green-500 shrink-0" />
        {labels[appliedAction!] ?? 'Resolved'}
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
            {expanded && contents && (
              <button onClick={() => setDiffMode(v => !v)} className="hover:text-gray-300">
                {diffMode ? 'Side-by-side' : 'Diff'}
              </button>
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
              <div className="flex items-center gap-1.5 mt-0.5 text-xs">
                {(() => {
                  const trust = sourceTrust(atom.source);
                  const toneClass = trust.tone === 'official'
                    ? 'text-blue-400 bg-blue-500/10'
                    : trust.tone === 'manual'
                      ? 'text-gray-500 bg-[#2a2a2a]'
                      : 'text-gray-400 bg-[#2a2a2a]';
                  return <span className={`px-1.5 py-0.5 rounded ${toneClass} truncate max-w-[160px]`}>{trust.label}</span>;
                })()}
                {relativeAge(atom.created_at) && (
                  <span className="text-gray-600">· {relativeAge(atom.created_at)}</span>
                )}
              </div>
            </div>
          ))}
        </div>

        {/* Side-by-side or diff content */}
        {expanded && contents && (
          diffMode
            ? <DiffView a={contents[0]} b={contents[1]} />
            : <div className="grid grid-cols-2 gap-2 border-t border-white/5 pt-2">
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
            icon={<ChevronLeft className="w-3 h-3" />}
            label="Keep A"
            title="Delete the right atom; keep the left one"
            loading={status === 'loading' && appliedAction === 'keep_a'}
            disabled={status === 'loading'}
            onClick={() => applyDirect('keep_a')}
          />
          <ActionBtn
            icon={<ChevronRight className="w-3 h-3" />}
            label="Keep B"
            title="Delete the left atom; keep the right one"
            loading={status === 'loading' && appliedAction === 'keep_b'}
            disabled={status === 'loading'}
            onClick={() => applyDirect('keep_b')}
          />
          <ActionBtn
            icon={<GitMerge className="w-3 h-3" />}
            label="Merge…"
            title="Open an editor to combine both atoms, then delete the loser"
            loading={loadingContent && !expanded}
            disabled={status === 'loading'}
            onClick={openMerge}
          />
        </div>

        {/* Merge editor */}
        {mergeOpen && (
          <div className="space-y-2 border-t border-white/5 pt-2">
            <textarea
              value={mergeDraft}
              onChange={e => setMergeDraft(e.target.value)}
              rows={14}
              className="w-full bg-[#161616] border border-white/10 rounded p-2 text-xs text-gray-200 font-mono leading-relaxed focus:outline-none focus:border-purple-500"
            />
            <div className="flex items-center justify-between">
              <p className="text-xs text-gray-600">More recent atom will be kept; the older will be deleted.</p>
              <div className="flex gap-1.5">
                <button onClick={() => setMergeOpen(false)} className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5">Cancel</button>
                <button onClick={applyEditedMerge} disabled={!mergeDraft.trim() || status === 'loading'} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40 inline-flex items-center gap-1">
                  {status === 'loading' ? <Loader2 className="w-3 h-3 animate-spin" /> : <Check className="w-3 h-3" />} Apply merge
                </button>
              </div>
            </div>
          </div>
        )}
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

// ==================== Virtualized pair list ====================

function VirtualizedPairList({
  pairs,
  onResolve,
}: {
  pairs: OverlapPair[];
  onResolve: (pair: OverlapPair) => void;
}) {
  const parentRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: pairs.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 140,
    overscan: 5,
    gap: 8,
  });

  return (
    <div ref={parentRef} className="max-h-[calc(100vh-280px)] overflow-auto">
      <div
        style={{
          height: virtualizer.getTotalSize(),
          width: '100%',
          position: 'relative',
        }}
      >
        {virtualizer.getVirtualItems().map(vi => (
          <div
            key={vi.key}
            data-index={vi.index}
            ref={virtualizer.measureElement}
            style={{
              position: 'absolute',
              top: 0,
              left: 0,
              width: '100%',
              transform: `translateY(${vi.start}px)`,
            }}
          >
            <PairRow pair={pairs[vi.index]} onResolve={onResolve} />
          </div>
        ))}
      </div>
    </div>
  );
}

// ==================== Boilerplate section ====================

function BoilerplateSection({ atoms, onResolved }: { atoms: BoilerplateEntry[]; onResolved: () => void }) {
  const [removed, setRemoved] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState(0);
  const visible = atoms.filter(a => !removed.has(a.id));

  const handleResolved = (id: string) => {
    setRemoved(prev => new Set(prev).add(id));
    setSelected(prev => { const n = new Set(prev); n.delete(id); return n; });
    onResolved();
  };

  const bulkDismiss = async () => {
    setBusy(true);
    setProgress(0);
    try {
      const items = Array.from(selected).map(id => ({ check: 'boilerplate_pollution', item_id: id, action: 'dismiss' }));
      const resp = await getTransport().invoke<{ results: Array<{ check: string; item_id: string; ok: boolean; error?: string }> }>('health_fix_batch', { items });
      const okIds = new Set(resp.results.filter(r => r.ok).map(r => r.item_id));
      setProgress(okIds.size);
      setRemoved(prev => { const next = new Set(prev); okIds.forEach(id => next.add(id)); return next; });
      okIds.forEach(() => onResolved());
      setSelected(new Set());
      const failed = resp.results.filter(r => !r.ok);
      if (failed.length) console.warn('Batch fix partial failure:', failed);
    } finally {
      setBusy(false);
    }
  };

  if (visible.length === 0) {
    return <p className="text-xs text-gray-500 text-center py-8">No boilerplate pollution — all clear</p>;
  }

  return (
    <div className="relative space-y-3">
      <div className="bg-[#1e1a00] border border-yellow-900/30 rounded p-3 space-y-1.5">
        <p className="text-xs text-yellow-300/90 font-medium">Embedding quality issue</p>
        <p className="text-xs text-gray-400 leading-relaxed">
          These {visible.length} atom{visible.length !== 1 ? 's' : ''} have near-identical semantic edges.
          Their unique content is drowned out by shared template text. Edit the atoms to make unique
          content more prominent, then Re-embed to refresh their vectors.
        </p>
      </div>
      <div className="space-y-2 pb-12">
        {visible.slice().sort((a, b) => b.clone_count - a.clone_count).map(atom => (
          <label key={atom.id} className="flex items-start gap-2">
            <input
              type="checkbox"
              checked={selected.has(atom.id)}
              onChange={e => {
                setSelected(prev => { const n = new Set(prev); if (e.target.checked) n.add(atom.id); else n.delete(atom.id); return n; });
              }}
              className="mt-3 peer h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
            />
            <div className="flex-1 min-w-0">
              <BoilerplateAtomRow atom={atom} onResolved={handleResolved} />
            </div>
          </label>
        ))}
      </div>
      {selected.size > 0 && (
        <div className="sticky bottom-0 -mx-5 px-5 py-2 bg-[#1a1a1a] border-t border-white/10 flex items-center justify-between">
          <span className="text-xs text-gray-400">{selected.size} selected</span>
          <div className="flex gap-1.5">
            <button onClick={() => setSelected(new Set())} className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5">Clear</button>
            <button onClick={bulkDismiss} disabled={busy} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40">
              {busy ? `Dismissing ${progress}/${selected.size}…` : `Dismiss ${selected.size}`}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

// ==================== Contradiction section ====================

function ContradictionRow({ pair, onDismissed }: { pair: ContradictionPair; onDismissed?: () => void }) {
  const [expanded, setExpanded] = useState(false);
  const [contents, setContents] = useState<[string, string] | null>(null);
  const [loadingContent, setLoadingContent] = useState(false);
  const [summary, setSummary] = useState<string | null>(null);
  const [loadingSummary, setLoadingSummary] = useState(false);
  const [dismissed, setDismissed] = useState(false);

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

  const defer = async () => {
    await getTransport().invoke('apply_health_item_fix', {
      check: 'contradiction_detection',
      item_id: `${pair.atom_a.id <= pair.atom_b.id ? pair.atom_a.id : pair.atom_b.id}__${pair.atom_a.id <= pair.atom_b.id ? pair.atom_b.id : pair.atom_a.id}`,
      action: 'defer',
    });
    setDismissed(true);
    onDismissed?.();
  };

  const simPct = Math.round(pair.similarity * 100);
  const simColor = simPct >= 88 ? 'text-orange-400' : 'text-yellow-400';

  if (dismissed) {
    return (
      <div className="flex items-center gap-2 p-3 rounded border border-white/5 bg-[#1e1e1e] text-xs text-gray-500">
        <CheckCircle className="w-3.5 h-3.5 text-yellow-500 shrink-0" />
        Flagged for later — hidden for 7 days
      </div>
    );
  }

  return (
    <div className="bg-[#1e1e1e] rounded border border-white/5">
      <div className="p-3 space-y-2.5">
        {summary && (
          <div className="bg-[#1a1a2e] border border-purple-900/30 rounded px-2 py-1.5 text-xs text-purple-200 mb-2">
            {summary}
          </div>
        )}
        <div className="flex items-center justify-between gap-2">
          <span className={`text-xs font-medium ${simColor}`}>{simPct}% similarity</span>
          <div className="flex items-center gap-2 text-xs text-gray-600">
            {pair.shared_tag_count > 0 && (
              <span>{pair.shared_tag_count} shared tag{pair.shared_tag_count !== 1 ? 's' : ''}</span>
            )}
            <button
              onClick={async () => {
                setLoadingSummary(true);
                try {
                  const res = await getTransport().invoke<{ summary: string }>('health_contradiction_summary', {
                    atom_a: pair.atom_a.id,
                    atom_b: pair.atom_b.id,
                  });
                  setSummary(res.summary);
                } catch (e) {
                  setSummary(e instanceof Error ? `Error: ${e.message}` : 'Error loading summary');
                } finally {
                  setLoadingSummary(false);
                }
              }}
              disabled={loadingSummary || !!summary}
              className="hover:text-gray-300 disabled:opacity-40"
              title="Ask the LLM to describe the conflict in one sentence"
            >
              {loadingSummary ? 'Summarising…' : summary ? 'Summary below' : 'Summarise'}
            </button>
            <button onClick={defer} className="hover:text-gray-300" title="Hide this pair for 7 days">Flag for later</button>
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
              <div className="flex items-center gap-1.5 mt-0.5 text-xs">
                {(() => {
                  const trust = sourceTrust(atom.source);
                  const toneClass = trust.tone === 'official'
                    ? 'text-blue-400 bg-blue-500/10'
                    : trust.tone === 'manual'
                      ? 'text-gray-500 bg-[#2a2a2a]'
                      : 'text-gray-400 bg-[#2a2a2a]';
                  return <span className={`px-1.5 py-0.5 rounded ${toneClass} truncate max-w-[160px]`}>{trust.label}</span>;
                })()}
                {relativeAge(atom.created_at) && (
                  <span className="text-gray-600">· {relativeAge(atom.created_at)}</span>
                )}
              </div>
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

function ContradictionSection({ data, onResolved }: { data: Record<string, unknown>; onResolved: () => void }) {
  const pairs = (data.pairs as ContradictionPair[] | undefined) ?? [];
  const count = (data.potential_contradictions as number) ?? 0;
  const [dismissed, setDismissed] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState(0);
  const visible = pairs.filter(p => !dismissed.has(p.pair_id));

  const bulkDefer = async () => {
    setBusy(true);
    setProgress(0);
    try {
      const items = Array.from(selected).map(id => ({ check: 'contradiction_detection', item_id: id, action: 'defer' }));
      const resp = await getTransport().invoke<{ results: Array<{ check: string; item_id: string; ok: boolean; error?: string }> }>('health_fix_batch', { items });
      const okIds = new Set(resp.results.filter(r => r.ok).map(r => r.item_id));
      setProgress(okIds.size);
      setDismissed(prev => { const next = new Set(prev); okIds.forEach(id => next.add(id)); return next; });
      okIds.forEach(() => onResolved());
      setSelected(new Set());
      const failed = resp.results.filter(r => !r.ok);
      if (failed.length) console.warn('Batch fix partial failure:', failed);
    } finally {
      setBusy(false);
    }
  };

  if (visible.length === 0) {
    return (
      <p className="text-xs text-gray-500 text-center py-8">No contradiction candidates — all clear</p>
    );
  }

  return (
    <div className="relative space-y-3">
      <div className="bg-[#1a1a2e] border border-purple-900/30 rounded p-3 space-y-1.5">
        <p className="text-xs text-purple-300/90 font-medium">Contradiction candidates</p>
        <p className="text-xs text-gray-400 leading-relaxed">
          {count} atom pair{count !== 1 ? 's' : ''} cover the same topic but may contain
          conflicting information (similarity 80–92%). Compare their content and merge or
          update them to align. Use <strong className="text-gray-300">Compare</strong> to view
          both atoms side-by-side.
        </p>
      </div>
      <div className="space-y-2 pb-12">
        {visible.map(pair => (
          <label key={pair.pair_id} className="flex items-start gap-2">
            <input
              type="checkbox"
              checked={selected.has(pair.pair_id)}
              onChange={e => {
                setSelected(prev => { const n = new Set(prev); if (e.target.checked) n.add(pair.pair_id); else n.delete(pair.pair_id); return n; });
              }}
              className="mt-3 peer h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
            />
            <div className="flex-1 min-w-0">
              <ContradictionRow key={pair.pair_id} pair={pair} onDismissed={() => setDismissed(prev => new Set(prev).add(pair.pair_id))} />
            </div>
          </label>
        ))}
      </div>
      {selected.size > 0 && (
        <div className="sticky bottom-0 -mx-5 px-5 py-2 bg-[#1a1a1a] border-t border-white/10 flex items-center justify-between">
          <span className="text-xs text-gray-400">{selected.size} selected</span>
          <div className="flex gap-1.5">
            <button onClick={() => setSelected(new Set())} className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5">Clear</button>
            <button onClick={bulkDefer} disabled={busy} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40">
              {busy ? `Flagging ${progress}/${selected.size}…` : `Flag for later ${selected.size}`}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

// ==================== Content quality (no-source) section ====================

function ContentQualitySection({ data, onResolved }: { data: Record<string, unknown>; onResolved: () => void }) {
  const issues = data.issues as Record<string, {
    count: number;
    atoms?: Array<{ id: string; title: string; created_at?: string } | string>;
  }> | undefined;

  const noSourceItems = (issues?.no_source?.atoms ?? []) as Array<{ id: string; title: string; created_at?: string }>;
  const [removed, setRemoved] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState(0);
  const visible = noSourceItems.filter(a => !removed.has(a.id));

  const handleResolved = (id: string) => {
    setRemoved(prev => new Set(prev).add(id));
    setSelected(prev => { const n = new Set(prev); n.delete(id); return n; });
    onResolved();
  };

  const bulkMarkIntentional = async () => {
    setBusy(true);
    setProgress(0);
    try {
      const items = Array.from(selected).map(id => ({ check: 'content_quality', item_id: id, action: 'mark_intentional' }));
      const resp = await getTransport().invoke<{ results: Array<{ check: string; item_id: string; ok: boolean; error?: string }> }>('health_fix_batch', { items });
      const okIds = new Set(resp.results.filter(r => r.ok).map(r => r.item_id));
      setProgress(okIds.size);
      setRemoved(prev => { const next = new Set(prev); okIds.forEach(id => next.add(id)); return next; });
      okIds.forEach(() => onResolved());
      setSelected(new Set());
      const failed = resp.results.filter(r => !r.ok);
      if (failed.length) console.warn('Batch fix partial failure:', failed);
    } finally {
      setBusy(false);
    }
  };

  if (visible.length === 0) {
    return <p className="text-xs text-gray-500 text-center py-8">No unsourced atoms — all clear</p>;
  }

  return (
    <div className="relative space-y-3">
      <div className="bg-[#1a1a1a] border border-white/5 rounded p-3 space-y-1.5">
        <p className="text-xs text-gray-300 font-medium">
          {visible.length} atom{visible.length !== 1 ? 's' : ''} missing a source URL
        </p>
        <p className="text-xs text-gray-400 leading-relaxed">
          Add a source URL for each, or Mark intentional if the atom doesn’t have one
          (e.g. meeting notes, personal writing).
        </p>
      </div>
      <div className="space-y-1.5 pb-12">
        {visible.map(atom => (
          <label key={atom.id} className="flex items-start gap-2">
            <input
              type="checkbox"
              checked={selected.has(atom.id)}
              onChange={e => {
                setSelected(prev => { const n = new Set(prev); if (e.target.checked) n.add(atom.id); else n.delete(atom.id); return n; });
              }}
              className="mt-3 peer h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
            />
            <div className="flex-1 min-w-0">
              <NoSourceRow atom={atom} onResolved={handleResolved} />
            </div>
          </label>
        ))}
      </div>
      {selected.size > 0 && (
        <div className="sticky bottom-0 -mx-5 px-5 py-2 bg-[#1a1a1a] border-t border-white/10 flex items-center justify-between">
          <span className="text-xs text-gray-400">{selected.size} selected</span>
          <div className="flex gap-1.5">
            <button onClick={() => setSelected(new Set())} className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5">Clear</button>
            <button onClick={bulkMarkIntentional} disabled={busy} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40">
              {busy ? `Marking ${progress}/${selected.size}…` : `Mark intentional ${selected.size}`}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

// ==================== Tag health (rootless) section ====================

function TagHealthSection({ data, onResolved }: { data: Record<string, unknown>; onResolved: () => void }) {
  const rootlessList = (data.rootless_tag_list as RootlessTag[] | undefined) ?? [];
  const similarPairs = (data.similar_name_pair_list as Array<{ pair_id: string; a_id: string; a_name: string; b_id: string; b_name: string }> | undefined) ?? [];
  const singleAtomTags = (data.single_atom_tag_list as Array<{ id: string; name: string; is_autotag: boolean }> | undefined) ?? [];
  const [removedSingleAtom, setRemovedSingleAtom] = useState<Set<string>>(new Set());
  const [mergeTargets, setMergeTargets] = useState<Record<string, string>>({});
  const [removed, setRemoved] = useState<Set<string>>(new Set());
  const [removedPairs, setRemovedPairs] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState(0);
  const visible = rootlessList.filter(t => !removed.has(t.id));
  const visiblePairs = similarPairs.filter(p => !removedPairs.has(p.pair_id));
  const visibleSingleAtomTags = singleAtomTags.filter(t => !removedSingleAtom.has(t.id));

  const allTags = useTagsStore(s => s.tags);
  const parentOptions = useMemo(() => {
    const rootlessIds = new Set(rootlessList.map(t => t.id));
    return allTags
      .filter(t => !rootlessIds.has(t.id))
      .map(t => ({ id: t.id, name: t.name }));
  }, [allTags, rootlessList]);

  const handleResolved = (id: string) => {
    setRemoved(prev => new Set(prev).add(id));
    setSelected(prev => { const n = new Set(prev); n.delete(id); return n; });
    onResolved();
  };

  const mergeInto = async (p: { pair_id: string; a_id: string; a_name: string; b_id: string; b_name: string }, winner_id: string) => {
    await getTransport().invoke('apply_health_item_fix', {
      check: 'tag_health',
      item_id: p.pair_id,
      action: 'merge_tags',
      into_tag_id: winner_id,
    });
    setRemovedPairs(prev => new Set(prev).add(p.pair_id));
    onResolved();
  };

  const ignorePair = async (p: { pair_id: string }) => {
    await getTransport().invoke('apply_health_item_fix', {
      check: 'tag_health',
      item_id: p.pair_id,
      action: 'dismiss',
    });
    setRemovedPairs(prev => new Set(prev).add(p.pair_id));
    onResolved();
  };

  const deleteSingleAtomTag = async (tagId: string) => {
    await getTransport().invoke('apply_health_item_fix', {
      check: 'tag_health',
      item_id: tagId,
      action: 'delete_tag',
    });
    setRemovedSingleAtom(prev => new Set(prev).add(tagId));
    setSelected(prev => { const n = new Set(prev); n.delete(tagId); return n; });
    onResolved();
  };

  const mergeSingleAtomTagIntoParent = async (tagId: string) => {
    const into = mergeTargets[tagId];
    if (!into) return;
    await getTransport().invoke('apply_health_item_fix', {
      check: 'tag_health',
      item_id: tagId,
      action: 'merge_into_parent',
      into_tag_id: into,
    });
    setRemovedSingleAtom(prev => new Set(prev).add(tagId));
    setSelected(prev => { const n = new Set(prev); n.delete(tagId); return n; });
    onResolved();
  };

  const dismissSingleAtomTag = async (tagId: string) => {
    await getTransport().invoke('apply_health_item_fix', {
      check: 'tag_health',
      item_id: tagId,
      action: 'dismiss',
    });
    setRemovedSingleAtom(prev => new Set(prev).add(tagId));
    setSelected(prev => { const n = new Set(prev); n.delete(tagId); return n; });
    onResolved();
  };

  const bulkDismiss = async () => {
    setBusy(true);
    setProgress(0);
    try {
      const tagItems = Array.from(selected).map(id => ({ check: 'tag_health', item_id: id, action: 'dismiss' }));
      const pairItems = Array.from(selected).filter(id => id.includes('__')).map(id => ({ check: 'tag_health', item_id: id, action: 'dismiss' }));
      const items = [...tagItems, ...pairItems.filter(pi => !tagItems.find(ti => ti.item_id === pi.item_id))];
      const resp = await getTransport().invoke<{ results: Array<{ check: string; item_id: string; ok: boolean; error?: string }> }>('health_fix_batch', { items });
      const okIds = new Set(resp.results.filter(r => r.ok).map(r => r.item_id));
      setProgress(okIds.size);
      setRemoved(prev => { const next = new Set(prev); okIds.forEach(id => { if (!id.includes('__')) next.add(id); }); return next; });
      setRemovedPairs(prev => { const next = new Set(prev); okIds.forEach(id => { if (id.includes('__')) next.add(id); }); return next; });
      okIds.forEach(() => onResolved());
      setSelected(new Set());
      const failed = resp.results.filter(r => !r.ok);
      if (failed.length) console.warn('Batch fix partial failure:', failed);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="relative space-y-4">
      {visible.length > 0 && (
        <div className="space-y-2 pb-12">
          <div className="bg-[#1a1a1a] border border-white/5 rounded p-3 space-y-1">
            <p className="text-xs text-gray-300 font-medium">
              {visible.length} root-level tag{visible.length !== 1 ? 's' : ''} with no parent
            </p>
            <p className="text-xs text-gray-500 leading-relaxed">
              Pick a parent to nest them under, or Dismiss to leave at root.
            </p>
          </div>
          <div className="space-y-1.5">
            {visible.slice().sort((a, b) => b.atom_count - a.atom_count).map(tag => (
              <label key={tag.id} className="flex items-start gap-2">
                <input
                  type="checkbox"
                  checked={selected.has(tag.id)}
                  onChange={e => {
                    setSelected(prev => { const n = new Set(prev); if (e.target.checked) n.add(tag.id); else n.delete(tag.id); return n; });
                  }}
                  className="mt-3 peer h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
                />
                <div className="flex-1 min-w-0">
                  <TagRootlessRow
                    tag={tag}
                    parentOptions={parentOptions}
                    onResolved={handleResolved}
                  />
                </div>
              </label>
            ))}
          </div>
        </div>
      )}

      {visiblePairs.length > 0 && (
        <div className="space-y-2 pb-12">
          <div className="bg-[#1a1a1a] border border-white/5 rounded p-3 space-y-1">
            <p className="text-xs text-gray-300 font-medium">
              {visiblePairs.length} similar-name pair{visiblePairs.length !== 1 ? 's' : ''}
            </p>
            <p className="text-xs text-gray-500 leading-relaxed">
              Tags with near-identical names may be duplicates. Merge to keep one, or Ignore to dismiss.
            </p>
          </div>
          <div className="space-y-1.5">
            {visiblePairs.map(p => (
              <label key={p.pair_id} className="flex items-start gap-2">
                <input
                  type="checkbox"
                  checked={selected.has(p.pair_id)}
                  onChange={e => {
                    setSelected(prev => { const n = new Set(prev); if (e.target.checked) n.add(p.pair_id); else n.delete(p.pair_id); return n; });
                  }}
                  className="mt-3 peer h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
                />
                <div className="flex-1 min-w-0">
                  <div className="flex items-center justify-between py-2">
                    <div>
                      <span className="text-xs text-gray-300">{p.a_name}</span>
                      <span className="text-xs text-gray-600 mx-1">~</span>
                      <span className="text-xs text-gray-300">{p.b_name}</span>
                    </div>
                    <div className="flex gap-1.5">
                      <button onClick={() => mergeInto(p, p.a_id)} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500">Keep {p.a_name}</button>
                      <button onClick={() => mergeInto(p, p.b_id)} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500">Keep {p.b_name}</button>
                      <button onClick={() => ignorePair(p)} className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5">Ignore</button>
                    </div>
                  </div>
                </div>
              </label>
            ))}
          </div>
        </div>
      )}

      {visibleSingleAtomTags.length > 0 && (
        <div className="space-y-2 pb-12">
          <div className="bg-[#1a1a1a] border border-white/5 rounded p-3 space-y-1">
            <p className="text-xs text-gray-300 font-medium">
              {visibleSingleAtomTags.length} tag{visibleSingleAtomTags.length !== 1 ? 's' : ''} used by only one atom
            </p>
            <p className="text-xs text-gray-500 leading-relaxed">
              These tags are exclusive to a single atom — they may be too narrow. Delete auto-extracted ones or merge into a broader tag.
            </p>
          </div>
          <div className="space-y-1.5">
            {visibleSingleAtomTags.map(tag => (
              <label key={tag.id} className="flex items-start gap-2">
                <input
                  type="checkbox"
                  checked={selected.has(tag.id)}
                  onChange={e => {
                    setSelected(prev => { const n = new Set(prev); if (e.target.checked) n.add(tag.id); else n.delete(tag.id); return n; });
                  }}
                  className="mt-3 peer h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
                />
                <div className="flex-1 min-w-0">
                  <div className="flex items-center justify-between py-2 gap-2">
                    <div className="flex items-center gap-2 min-w-0">
                      <span className="text-xs text-gray-300 truncate">{tag.name}</span>
                      {tag.is_autotag && (
                        <span className="px-1.5 py-0.5 rounded text-[10px] bg-purple-900/40 text-purple-300 shrink-0">auto</span>
                      )}
                    </div>
                    <div className="flex gap-1.5 shrink-0">
                      {tag.is_autotag ? (
                        <button
                          onClick={() => deleteSingleAtomTag(tag.id)}
                          className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500"
                        >
                          Delete
                        </button>
                      ) : (
                        <div className="flex items-center gap-1.5">
                          <select
                            value={mergeTargets[tag.id] ?? ''}
                            onChange={e => setMergeTargets(prev => ({ ...prev, [tag.id]: e.target.value }))}
                            className="text-xs bg-[#2a2a2a] border border-white/10 rounded px-1.5 py-1 text-gray-300 max-w-[140px]"
                          >
                            <option value="">Merge into…</option>
                            {allTags.filter(t => t.id !== tag.id).map(t => (
                              <option key={t.id} value={t.id}>{t.name}</option>
                            ))}
                          </select>
                          <button
                            onClick={() => mergeSingleAtomTagIntoParent(tag.id)}
                            disabled={!mergeTargets[tag.id]}
                            className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40"
                          >
                            Merge
                          </button>
                        </div>
                      )}
                      <button
                        onClick={() => dismissSingleAtomTag(tag.id)}
                        className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5"
                      >
                        Ignore
                      </button>
                    </div>
                  </div>
                </div>
              </label>
            ))}
          </div>
        </div>
      )}

      {selected.size > 0 && (
        <div className="sticky bottom-0 -mx-5 px-5 py-2 bg-[#1a1a1a] border-t border-white/10 flex items-center justify-between">
          <span className="text-xs text-gray-400">{selected.size} selected</span>
          <div className="flex gap-1.5">
            <button onClick={() => setSelected(new Set())} className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5">Clear</button>
            <button onClick={bulkDismiss} disabled={busy} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40">
              {busy ? `Dismissing ${progress}/${selected.size}…` : `Dismiss ${selected.size}`}
            </button>
          </div>
        </div>
      )}

      {visible.length === 0 && visiblePairs.length === 0 && visibleSingleAtomTags.length === 0 && (
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

export function HealthReviewModal({ report: initialReport, checkName, onClose, onResolved }: Props) {
  const [report, setReport] = useState(initialReport);
  const [lastScannedAt, setLastScannedAt] = useState<Record<string, string>>({});
  const [rescanning, setRescanning] = useState<string | null>(null);

  // Re-sync when prop changes (e.g. widget fetched a new full report)
  useEffect(() => {
    setReport(initialReport);
  }, [initialReport]);

  const dbId = useDatabasesStore(s => s.activeId) ?? 'default';
  const [resolvedByTab, setResolvedByTab] = useState<Record<string, number>>(() => loadResolved(dbId).counts);

  const bumpResolved = useCallback((check: string) => {
    setResolvedByTab(prev => {
      const next = { ...prev, [check]: (prev[check] ?? 0) + 1 };
      saveResolved(dbId, { date: todayKey(), counts: next });
      return next;
    });
  }, [dbId]);

  const resolvedCount = useMemo(
    () => Object.values(resolvedByTab).reduce((a, b) => a + b, 0),
    [resolvedByTab],
  );

  const rescanTab = useCallback(async (checkNameToScan: string) => {
    setRescanning(checkNameToScan);
    try {
      const result = await getTransport().invoke<{
        status: string;
        score: number;
        auto_fixable: boolean;
        requires_review: boolean;
        fix_action?: unknown;
        data: Record<string, unknown>;
      }>('health_check_single', { check_name: checkNameToScan });

      setReport(prev => ({
        ...prev,
        checks: { ...prev.checks, [checkNameToScan]: result },
      }));
      setLastScannedAt(prev => ({ ...prev, [checkNameToScan]: new Date().toISOString() }));
    } catch (e) {
      console.error('Re-scan failed:', e);
    } finally {
      setRescanning(null);
    }
  }, []);

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
  const similarPairsCount = (tagHealthData?.similar_name_pairs as number) ?? 0;
  const singleAtomTagCount = (tagHealthData?.single_atom_tags as number) ?? (tagHealthData?.single_atom_tag_list as unknown[] | undefined)?.length ?? 0;
  const brokenLinksData: Record<string, unknown> | null =
    (report.checks['broken_internal_links']?.data ?? null) as Record<string, unknown> | null;
  const brokenLinksCount = (brokenLinksData?.broken_link_list as unknown[] | undefined)?.length ?? (brokenLinksData?.broken_links as number) ?? 0;

  // Snapshot initial queue sizes once per report load for progress bar
  const initialSizes = useMemo(() => ({
    content_overlap: overlapPairs.length + (resolvedByTab['content_overlap'] ?? 0),
    boilerplate_pollution: boilerplateAtoms.length + (resolvedByTab['boilerplate_pollution'] ?? 0),
    contradiction_detection: contradictionCount + (resolvedByTab['contradiction_detection'] ?? 0),
    content_quality: noSourceCount + (resolvedByTab['content_quality'] ?? 0),
    tag_health: rootlessCount + similarPairsCount + singleAtomTagCount + (resolvedByTab['tag_health'] ?? 0),
    broken_internal_links: brokenLinksCount + (resolvedByTab['broken_internal_links'] ?? 0),
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }), []); // intentionally empty deps — snapshot on mount only

  const tabs = [
    ...(overlapPairs.length > 0        ? [{ key: 'content_overlap',        label: 'Content overlap', count: overlapPairs.length }] : []),
    ...(boilerplateAtoms.length > 0    ? [{ key: 'boilerplate_pollution',    label: 'Boilerplate',     count: boilerplateAtoms.length }] : []),
    ...(contradictionCount > 0         ? [{ key: 'contradiction_detection', label: 'Contradictions',  count: contradictionCount }] : []),
    ...(noSourceCount > 0              ? [{ key: 'content_quality',         label: 'No source',       count: noSourceCount }] : []),
    ...(rootlessCount > 0 || similarPairsCount > 0 || singleAtomTagCount > 0 ? [{ key: 'tag_health', label: 'Tag structure', count: rootlessCount + similarPairsCount + singleAtomTagCount }] : []),
    ...(brokenLinksCount > 0 ? [{ key: 'broken_internal_links', label: 'Broken links', count: brokenLinksCount }] : []),
  ];

  const [selectedTab, setSelectedTab] = useState<string | null>(checkName ?? null);
  const activeTab = tabs.find(t => t.key === selectedTab)?.key ?? tabs[0]?.key ?? null;

  useEffect(() => {
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    document.addEventListener('keydown', handler);
    document.body.style.overflow = 'hidden';
    return () => {
      document.removeEventListener('keydown', handler);
      document.body.style.overflow = '';
    };
  }, [onClose]);


  // Overlap batch selection
  const [overlapSelected, setOverlapSelected] = useState<Set<string>>(new Set());
  const [overlapRemoved, setOverlapRemoved] = useState<Set<string>>(new Set());
  const [overlapBusy, setOverlapBusy] = useState(false);
  const [overlapProgress, setOverlapProgress] = useState(0);
  const visibleOverlapPairs = overlapPairs.filter(p => !overlapRemoved.has(p.pair_id));

  const handlePairResolveWithRemove = useCallback((pair: OverlapPair) => {
    setOverlapRemoved(prev => new Set(prev).add(pair.pair_id));
    setOverlapSelected(prev => { const n = new Set(prev); n.delete(pair.pair_id); return n; });
    bumpResolved('content_overlap');
    onResolved();
  }, [onResolved, bumpResolved]);

  const bulkDismissOverlap = async () => {
    setOverlapBusy(true);
    setOverlapProgress(0);
    try {
      const items = Array.from(overlapSelected).map(id => ({ check: 'content_overlap', item_id: id, action: 'dismiss' }));
      const resp = await getTransport().invoke<{ results: Array<{ check: string; item_id: string; ok: boolean; error?: string }> }>('health_fix_batch', { items });
      const okIds = new Set(resp.results.filter(r => r.ok).map(r => r.item_id));
      setOverlapProgress(okIds.size);
      setOverlapRemoved(prev => { const next = new Set(prev); okIds.forEach(id => next.add(id)); return next; });
      okIds.forEach(() => { bumpResolved('content_overlap'); onResolved(); });
      setOverlapSelected(new Set());
      const failed = resp.results.filter(r => !r.ok);
      if (failed.length) console.warn('Batch fix partial failure:', failed);
    } finally {
      setOverlapBusy(false);
    }
  };

  // Markdown export
  const [copiedFlash, setCopiedFlash] = useState(false);
  const copyAsMarkdown = async () => {
    const lines: string[] = ['# Health Review Queue', ''];
    if (activeTab === 'content_overlap') {
      lines.push(`## Content overlap (${visibleOverlapPairs.length})`, '');
      visibleOverlapPairs.forEach(p => {
        lines.push(`- **${p.atom_a.title}** vs **${p.atom_b.title}** — ${(p.similarity * 100).toFixed(0)}% similarity`);
        if (p.atom_a.source) lines.push(`  - A: <${p.atom_a.source}>`);
        if (p.atom_b.source) lines.push(`  - B: <${p.atom_b.source}>`);
      });
    } else if (activeTab === 'boilerplate_pollution') {
      lines.push(`## Boilerplate pollution (${boilerplateAtoms.length})`, '');
      boilerplateAtoms.forEach(a => lines.push(`- ${a.title} — ${a.clone_count} clone edges`));
    } else if (activeTab === 'contradiction_detection' && contradictionData) {
      const pairs = (contradictionData.pairs as Array<{ atom_a: { title: string }; atom_b: { title: string }; similarity: number }>);
      lines.push(`## Contradictions (${pairs?.length ?? 0})`, '');
      pairs?.forEach(p => lines.push(`- **${p.atom_a.title}** ↔ **${p.atom_b.title}** — ${(p.similarity * 100).toFixed(0)}%`));
    } else if (activeTab === 'content_quality' && contentQualityData) {
      const atoms = ((contentQualityData.issues as Record<string, { atoms?: Array<{ title: string }> }> | undefined)?.no_source?.atoms) ?? [];
      lines.push(`## No source URL (${atoms.length})`, '');
      atoms.forEach(a => lines.push(`- ${a.title}`));
    } else if (activeTab === 'tag_health' && tagHealthData) {
      const tags = ((tagHealthData as { rootless_tag_list?: Array<{ name: string; atom_count: number }> }).rootless_tag_list) ?? [];
      lines.push(`## Rootless tags (${tags.length})`, '');
      tags.forEach(t => lines.push(`- ${t.name} (${t.atom_count} atoms)`));
    } else if (activeTab === 'broken_internal_links' && brokenLinksData) {
      const atoms = (brokenLinksData.broken_link_list as Array<{ atom_id: string; atom_title: string; links: Array<{ raw: string; target: string; kind: string }> }> | undefined) ?? [];
      lines.push(`## Broken links (${atoms.length})`, '');
      atoms.forEach(a => {
        lines.push(`- **${a.atom_title}** (${a.atom_id})`);
        a.links.forEach(l => lines.push(`  - \`${l.raw}\` → ${l.target} [${l.kind}]`));
      });
    }
    const md = lines.join('\n');
    try {
      await navigator.clipboard.writeText(md);
      setCopiedFlash(true);
      window.setTimeout(() => setCopiedFlash(false), 1500);
    } catch {
      alert('Copy failed; please select and copy manually.');
    }
  };
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
          <div className="flex items-center gap-1">
            <button onClick={copyAsMarkdown} title="Copy queue as markdown" className="text-gray-500 hover:text-gray-300 transition-colors p-1">
              {copiedFlash ? <Check className="w-4 h-4 text-green-500" /> : <Clipboard className="w-4 h-4" />}
            </button>
            <button onClick={onClose} className="text-gray-500 hover:text-gray-300 transition-colors p-1">
              <X className="w-5 h-5" />
            </button>
          </div>
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
              <TabHeader
                label="Content overlap"
                scannedAt={lastScannedAt['content_overlap']}
                rescanning={rescanning === 'content_overlap'}
                onRescan={() => rescanTab('content_overlap')}
                resolvedToday={resolvedByTab['content_overlap'] ?? 0}
                initialQueueSize={initialSizes['content_overlap'] ?? 0}
              />
              <p className="text-xs text-gray-500 leading-relaxed">
                Atoms from different sources with 55–85% similarity and at least 2 shared tags.
                These likely cover the same topic from different angles.
                Use <strong className="text-gray-300">Keep both</strong> for complementary perspectives,{' '}
                <strong className="text-gray-300">Merge</strong> for true duplicates.
              </p>
              <div className="relative pb-12">
                <div className="space-y-2">
                  {visibleOverlapPairs.length > 50
                    ? <VirtualizedPairList pairs={visibleOverlapPairs} onResolve={handlePairResolveWithRemove} />
                    : visibleOverlapPairs.map(pair => (
                        <label key={pair.pair_id} className="flex items-start gap-2">
                          <input
                            type="checkbox"
                            checked={overlapSelected.has(pair.pair_id)}
                            onChange={e => {
                              setOverlapSelected(prev => { const n = new Set(prev); if (e.target.checked) n.add(pair.pair_id); else n.delete(pair.pair_id); return n; });
                            }}
                            className="mt-3 peer h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
                          />
                          <div className="flex-1 min-w-0">
                            <PairRow pair={pair} onResolve={handlePairResolveWithRemove} />
                          </div>
                        </label>
                      ))
                  }
                </div>
                {overlapSelected.size > 0 && (
                  <div className="sticky bottom-0 -mx-5 px-5 py-2 bg-[#1a1a1a] border-t border-white/10 flex items-center justify-between">
                    <span className="text-xs text-gray-400">{overlapSelected.size} selected</span>
                    <div className="flex gap-1.5">
                      <button onClick={() => setOverlapSelected(new Set())} className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5">Clear</button>
                      <button onClick={bulkDismissOverlap} disabled={overlapBusy} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40">
                        {overlapBusy ? `Dismissing ${overlapProgress}/${overlapSelected.size}…` : `Dismiss ${overlapSelected.size}`}
                      </button>
                    </div>
                  </div>
                )}
              </div>
            </>
          )}

          {activeTab === 'boilerplate_pollution' && (
            <>
              <TabHeader
                label="Boilerplate"
                scannedAt={lastScannedAt['boilerplate_pollution']}
                rescanning={rescanning === 'boilerplate_pollution'}
                onRescan={() => rescanTab('boilerplate_pollution')}
                resolvedToday={resolvedByTab['boilerplate_pollution'] ?? 0}
                initialQueueSize={initialSizes['boilerplate_pollution'] ?? 0}
              />
              <BoilerplateSection atoms={boilerplateAtoms} onResolved={() => bumpResolved('boilerplate_pollution')} />
            </>
          )}

          {activeTab === 'contradiction_detection' && contradictionData && (
            <>
              <TabHeader
                label="Contradictions"
                scannedAt={lastScannedAt['contradiction_detection']}
                rescanning={rescanning === 'contradiction_detection'}
                onRescan={() => rescanTab('contradiction_detection')}
                resolvedToday={resolvedByTab['contradiction_detection'] ?? 0}
                initialQueueSize={initialSizes['contradiction_detection'] ?? 0}
              />
              <ContradictionSection data={contradictionData} onResolved={() => bumpResolved('contradiction_detection')} />
            </>
          )}

          {activeTab === 'content_quality' && contentQualityData && (
            <>
              <TabHeader
                label="No source"
                scannedAt={lastScannedAt['content_quality']}
                rescanning={rescanning === 'content_quality'}
                onRescan={() => rescanTab('content_quality')}
                resolvedToday={resolvedByTab['content_quality'] ?? 0}
                initialQueueSize={initialSizes['content_quality'] ?? 0}
              />
              <ContentQualitySection data={contentQualityData} onResolved={() => bumpResolved('content_quality')} />
            </>
          )}

          {activeTab === 'tag_health' && tagHealthData && (
            <>
              <TabHeader
                label="Tag structure"
                scannedAt={lastScannedAt['tag_health']}
                rescanning={rescanning === 'tag_health'}
                onRescan={() => rescanTab('tag_health')}
                resolvedToday={resolvedByTab['tag_health'] ?? 0}
                initialQueueSize={initialSizes['tag_health'] ?? 0}
              />
              <TagHealthSection data={tagHealthData} onResolved={() => bumpResolved('tag_health')} />
            </>
          )}

          {activeTab === 'broken_internal_links' && brokenLinksData && (
            <>
              <TabHeader
                label="Broken links"
                scannedAt={lastScannedAt['broken_internal_links']}
                rescanning={rescanning === 'broken_internal_links'}
                onRescan={() => rescanTab('broken_internal_links')}
                resolvedToday={resolvedByTab['broken_internal_links'] ?? 0}
                initialQueueSize={initialSizes['broken_internal_links'] ?? 0}
              />
              <p className="text-xs text-gray-500 leading-relaxed">
                Internal links in your atoms that point to atoms that no longer exist.
                Remove the link or dismiss to ignore.
              </p>
              <BrokenLinksSection data={brokenLinksData as { broken_link_list: import('./review/BrokenLinksSection').BrokenLinkAtom[] }} onResolved={() => bumpResolved('broken_internal_links')} />
            </>
          )}

        </div>
      </div>
    </div>,
    document.body,
  );
}
