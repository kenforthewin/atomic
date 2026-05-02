import { useState, useEffect, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { Loader2, Tags, RefreshCw, Check } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import { toast } from '../../stores/toasts';
import { useTagsStore } from '../../stores/tags';

// ==================== Types ====================

interface TagProposalAction {
  kind: 'merge' | 'rename' | 'reparent' | 'delete';
  // merge
  from_id?: string;
  into_id?: string;
  from_name?: string;
  into_name?: string;
  // rename
  tag_id?: string;
  old_name?: string;
  new_name?: string;
  // reparent
  tag_name?: string;
  new_parent_id?: string | null;
  new_parent_name?: string | null;
  // delete
  // tag_id / tag_name shared
  reason: string;
}

interface TagProposal {
  id: string;
  summary: string;
  actions: TagProposalAction[];
  generated_at: string;
}

interface FixAction {
  id: string;
  check: string;
  action: string;
  count: number;
  details: string[];
}

// ==================== Action label helpers ====================

function actionLabel(a: TagProposalAction): string {
  switch (a.kind) {
    case 'merge':   return `Merge "${a.from_name}" into "${a.into_name}"`;
    case 'rename':  return `Rename "${a.old_name}" → "${a.new_name}"`;
    case 'reparent':
      return a.new_parent_name
        ? `Move "${a.tag_name}" under "${a.new_parent_name}"`
        : `Move "${a.tag_name}" to root`;
    case 'delete':  return `Delete "${a.tag_name ?? a.old_name}"`;
  }
}

// ==================== Component ====================

export function TagStructureTab() {
  const [proposal, setProposal] = useState<TagProposal | null>(null);
  const [loading, setLoading]   = useState(true);
  const [generating, setGenerating] = useState(false);
  const [applying, setApplying] = useState(false);
  const [checked, setChecked]   = useState<Set<number>>(new Set());
  const fetchTags = useTagsStore(s => s.fetchTags);

  const parentRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: proposal?.actions.length ?? 0,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 72,
    overscan: 10,
  });

  // Load latest proposal on mount
  useEffect(() => {
    (async () => {
      setLoading(true);
      try {
        const p = await getTransport().invoke<TagProposal>('health_tag_proposal_latest', {});
        setProposal(p);
        setChecked(new Set(p.actions.map((_, i) => i)));
      } catch (err: unknown) {
        // 404 → no proposal yet, that's fine
        const status = (err as { status?: number })?.status ?? (err as { code?: number })?.code;
        if (status !== 404) {
          toast.error('Failed to load tag proposal', { detail: err instanceof Error ? err.message : String(err) });
        }
        setProposal(null);
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  const generateProposal = async () => {
    setGenerating(true);
    try {
      const p = await getTransport().invoke<TagProposal>('health_tag_proposal_create', {});
      setProposal(p);
      setChecked(new Set(p.actions.map((_, i) => i)));
    } catch (err) {
      toast.error('Failed to generate proposal', { detail: err instanceof Error ? err.message : String(err) });
    } finally {
      setGenerating(false);
    }
  };

  const applySelected = async () => {
    if (!proposal) return;
    setApplying(true);
    try {
      const accepted_indices = Array.from(checked).sort((a, b) => a - b);
      const results = await getTransport().invoke<FixAction[]>('health_tag_proposal_apply', {
        proposal_id: proposal.id,
        accepted_indices,
      });
      const count = results.reduce((n, r) => n + (r.count ?? 1), 0);
      toast.success(`Applied ${accepted_indices.length} tag restructure action${accepted_indices.length !== 1 ? 's' : ''} (${count} change${count !== 1 ? 's' : ''})`);
      setProposal(null);
      await fetchTags();
    } catch (err) {
      toast.error('Apply failed', { detail: err instanceof Error ? err.message : String(err) });
    } finally {
      setApplying(false);
    }
  };

  const toggleAll = () => {
    if (!proposal) return;
    if (checked.size === proposal.actions.length) {
      setChecked(new Set());
    } else {
      setChecked(new Set(proposal.actions.map((_, i) => i)));
    }
  };

  const toggleOne = (i: number) => {
    setChecked(prev => {
      const next = new Set(prev);
      if (next.has(i)) next.delete(i); else next.add(i);
      return next;
    });
  };

  const useVirtualList = (proposal?.actions.length ?? 0) > 20;

  // ---- Loading ----
  if (loading) {
    return (
      <div className="flex items-center justify-center py-16">
        <Loader2 className="w-5 h-5 text-[var(--color-text-secondary)] animate-spin" />
      </div>
    );
  }

  // ---- No proposal ----
  if (!proposal) {
    return (
      <div className="flex flex-col items-center justify-center py-20 gap-6 text-center">
        <div className="w-14 h-14 rounded-full bg-[var(--color-bg-card)] border border-[var(--color-border)] flex items-center justify-center">
          <Tags className="w-6 h-6 text-[var(--color-text-secondary)]" />
        </div>
        <div>
          <p className="text-sm font-medium text-white">No tag restructure proposal</p>
          <p className="text-xs text-[var(--color-text-secondary)] mt-1 max-w-xs">
            Generate a proposal to get AI-powered suggestions for merging, renaming, reparenting, and deleting tags.
          </p>
        </div>
        <button
          onClick={generateProposal}
          disabled={generating}
          className="flex items-center gap-2 px-4 py-2 bg-[var(--color-accent)] hover:bg-[var(--color-accent-hover)] disabled:opacity-50 rounded text-sm text-white font-medium transition-colors"
          aria-label="Propose tag restructure with LLM"
        >
          {generating ? <Loader2 className="w-4 h-4 animate-spin" /> : <Tags className="w-4 h-4" />}
          {generating ? 'Generating…' : 'Propose restructure with LLM'}
        </button>
      </div>
    );
  }

  // ---- Proposal view ----
  const actions = proposal.actions;

  return (
    <div className="space-y-4">
      {/* Proposal header */}
      <div className="flex items-start justify-between gap-4">
        <div className="flex-1 min-w-0">
          <p className="text-sm text-white font-medium">{proposal.summary}</p>
          <p className="text-xs text-[var(--color-text-secondary)] mt-0.5">
            {actions.length} action{actions.length !== 1 ? 's' : ''} proposed ·{' '}
            Generated {new Date(proposal.generated_at).toLocaleString()}
          </p>
        </div>
        <button
          onClick={generateProposal}
          disabled={generating}
          className="flex items-center gap-1.5 px-3 py-1.5 text-xs border border-[var(--color-border)] rounded-md text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors disabled:opacity-40 shrink-0"
          title="Generate a new proposal"
          aria-label="Generate new tag proposal"
        >
          {generating ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : <RefreshCw className="w-3.5 h-3.5" />}
          {generating ? 'Generating…' : 'Generate new proposal'}
        </button>
      </div>

      {/* Select-all row */}
      <div className="flex items-center gap-2 pb-1 border-b border-[var(--color-border)]">
        <input
          type="checkbox"
          id="select-all-actions"
          checked={checked.size === actions.length && actions.length > 0}
          onChange={toggleAll}
          className="h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none"
        />
        <label htmlFor="select-all-actions" className="text-xs text-[var(--color-text-secondary)] cursor-pointer">
          {checked.size === actions.length ? 'Deselect all' : `Select all (${actions.length})`}
        </label>
      </div>

      {/* Action list */}
      {useVirtualList ? (
        <div ref={parentRef} className="max-h-[60vh] overflow-y-auto">
          <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
            {virtualizer.getVirtualItems().map(vItem => (
              <div
                key={vItem.index}
                style={{ position: 'absolute', top: vItem.start, left: 0, right: 0 }}
              >
                <ActionRow
                  index={vItem.index}
                  action={actions[vItem.index]}
                  checked={checked.has(vItem.index)}
                  onToggle={toggleOne}
                />
              </div>
            ))}
          </div>
        </div>
      ) : (
        <div className="space-y-2">
          {actions.map((action, i) => (
            <ActionRow
              key={i}
              index={i}
              action={action}
              checked={checked.has(i)}
              onToggle={toggleOne}
            />
          ))}
        </div>
      )}

      {/* Footer */}
      <div className="sticky bottom-0 -mx-6 px-6 py-3 bg-[var(--color-bg-primary)] border-t border-[var(--color-border)] flex items-center justify-between">
        <span className="text-xs text-[var(--color-text-secondary)]">
          {checked.size} of {actions.length} selected
        </span>
        <button
          onClick={applySelected}
          disabled={applying || checked.size === 0}
          className="flex items-center gap-1.5 px-4 py-1.5 bg-[var(--color-accent)] hover:bg-[var(--color-accent-hover)] disabled:opacity-40 rounded text-xs text-white font-medium transition-colors"
          aria-label={`Apply ${checked.size} selected tag restructure actions`}
        >
          {applying ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : <Check className="w-3.5 h-3.5" />}
          {applying ? 'Applying…' : `Apply selected (${checked.size} of ${actions.length})`}
        </button>
      </div>
    </div>
  );
}

// ---- Action row sub-component ----

function ActionRow({
  index,
  action,
  checked,
  onToggle,
}: {
  index: number;
  action: TagProposalAction;
  checked: boolean;
  onToggle: (i: number) => void;
}) {
  const kindColors: Record<string, string> = {
    merge:    'bg-blue-900/40 text-blue-300',
    rename:   'bg-yellow-900/40 text-yellow-300',
    reparent: 'bg-green-900/40 text-green-300',
    delete:   'bg-red-900/40 text-red-300',
  };

  return (
    <label className="flex items-start gap-3 p-3 rounded-md border border-[var(--color-border)] bg-[var(--color-bg-card)] cursor-pointer hover:bg-[var(--color-bg-hover)] transition-colors">
      <input
        type="checkbox"
        checked={checked}
        onChange={() => onToggle(index)}
        className="mt-0.5 h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
      />
      <div className="flex-1 min-w-0 space-y-1">
        <div className="flex items-center gap-2 flex-wrap">
          <span className={`text-[10px] px-1.5 py-0.5 rounded font-medium uppercase tracking-wide ${kindColors[action.kind] ?? 'bg-gray-800 text-gray-400'}`}>
            {action.kind}
          </span>
          <span className="text-xs text-white">{actionLabel(action)}</span>
        </div>
        {action.reason && (
          <p className="text-xs text-[var(--color-text-secondary)] italic leading-snug">{action.reason}</p>
        )}
      </div>
    </label>
  );
}
