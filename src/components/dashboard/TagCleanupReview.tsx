import { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { ArrowLeftRight, Circle, GitMerge, Radio, Trash2, X } from 'lucide-react';
import { toast } from 'sonner';
import { getTransport } from '../../lib/transport';
import { useTagsStore } from '../../stores/tags';
import type {
  EmptyTagEvidence,
  KnowledgeSignal,
  KnowledgeSignalActionResult,
  TagCleanupTagEvidence,
  TagRedundancyEvidence,
} from '../../types/knowledgeSignals';

interface TagCleanupReviewProps {
  signalKey: string;
  onClose: () => void;
}

interface MergeTagsResult {
  source_tag_id: string;
  target_tag_id: string;
  atoms_retagged: number;
  children_reparented: number;
  source_wiki_deleted: boolean;
}

type TagCleanupSignal = KnowledgeSignal<TagRedundancyEvidence | EmptyTagEvidence>;

function isRedundancyEvidence(value: TagRedundancyEvidence | EmptyTagEvidence | undefined): value is TagRedundancyEvidence {
  return !!value && 'primary_tag' in value && 'secondary_tag' in value;
}

function isEmptyTagEvidence(value: TagRedundancyEvidence | EmptyTagEvidence | undefined): value is EmptyTagEvidence {
  return !!value && 'tag' in value;
}

function formatPath(tag: TagCleanupTagEvidence): string {
  return tag.path?.length ? tag.path.join(' / ') : tag.name;
}

function pct(value: number): string {
  return `${Math.round(value * 100)}%`;
}

function emitSignalChanged(signalKey: string) {
  window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey } }));
}

export function TagCleanupReview({ signalKey, onClose }: TagCleanupReviewProps) {
  const [signal, setSignal] = useState<TagCleanupSignal | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [isApplying, setIsApplying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const fetchTags = useTagsStore(s => s.fetchTags);

  useEffect(() => {
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';

    const handleEscape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', handleEscape);

    return () => {
      document.body.style.overflow = previousOverflow;
      document.removeEventListener('keydown', handleEscape);
    };
  }, [onClose]);

  useEffect(() => {
    let cancelled = false;
    async function loadSignal() {
      setIsLoading(true);
      setError(null);
      try {
        const [redundancy, empty] = await Promise.all([
          getTransport().invoke<TagCleanupSignal[]>('list_knowledge_signals', {
            providerId: 'tag_redundancy',
            includeDismissed: true,
            limit: 100,
          }),
          getTransport().invoke<TagCleanupSignal[]>('list_knowledge_signals', {
            providerId: 'empty_tag',
            includeDismissed: true,
            limit: 100,
          }),
        ]);
        if (cancelled) return;
        const next = [...redundancy, ...empty].find(item => item.id === signalKey) ?? null;
        setSignal(next);
        setIsLoading(false);
      } catch (err) {
        if (!cancelled) {
          setError(String(err));
          setIsLoading(false);
        }
      }
    }
    loadSignal();
    return () => {
      cancelled = true;
    };
  }, [signalKey]);

  const redundancy = isRedundancyEvidence(signal?.evidence) ? signal.evidence : null;
  const empty = isEmptyTagEvidence(signal?.evidence) ? signal.evidence : null;
  const [targetTagId, setTargetTagId] = useState<string | null>(null);
  const [showMergeConfirm, setShowMergeConfirm] = useState(false);

  useEffect(() => {
    if (redundancy) {
      setTargetTagId(redundancy.primary_tag.id);
    }
  }, [redundancy]);

  const mergeChoice = useMemo(() => {
    if (!redundancy) return null;
    const primary = redundancy.primary_tag;
    const secondary = redundancy.secondary_tag;
    const target = targetTagId === secondary.id ? secondary : primary;
    const source = target.id === primary.id ? secondary : primary;
    return { source, target };
  }, [redundancy, targetTagId]);

  const sourceUniqueAtomCount = useMemo(() => {
    if (!redundancy || !mergeChoice) return 0;
    return mergeChoice.source.id === redundancy.primary_tag.id
      ? redundancy.primary_unique_atom_count
      : redundancy.secondary_unique_atom_count;
  }, [redundancy, mergeChoice]);

  useEffect(() => {
    setShowMergeConfirm(false);
  }, [mergeChoice?.source.id, mergeChoice?.target.id]);

  const handleKeep = async () => {
    setIsApplying(true);
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
      emitSignalChanged(signalKey);
      onClose();
    } catch (err) {
      toast.error('Failed to dismiss suggestion', { description: String(err) });
      setIsApplying(false);
    }
  };

  const handleMerge = async () => {
    if (!mergeChoice) return;
    setIsApplying(true);
    try {
      const actionResult = await getTransport().invoke<KnowledgeSignalActionResult<MergeTagsResult>>('apply_knowledge_signal_action', {
        signalKey,
        action: 'merge_tags',
        payload: {
          sourceTagId: mergeChoice.source.id,
          targetTagId: mergeChoice.target.id,
        },
      });
      await fetchTags();
      emitSignalChanged(signalKey);
      toast.success('Tags merged', {
        description: `${actionResult.result?.atoms_retagged ?? 0} atoms retagged`,
      });
      onClose();
    } catch (err) {
      toast.error('Failed to merge tags', { description: String(err) });
      setIsApplying(false);
    }
  };

  const handleDeleteEmpty = async () => {
    if (!empty) return;
    setIsApplying(true);
    try {
      await getTransport().invoke('apply_knowledge_signal_action', {
        signalKey,
        action: 'delete_empty_tag',
      });
      await fetchTags();
      emitSignalChanged(signalKey);
      toast.success('Tag deleted');
      onClose();
    } catch (err) {
      toast.error('Failed to delete tag', { description: String(err) });
      setIsApplying(false);
    }
  };

  const handleOverlayClick = (event: React.MouseEvent) => {
    if (event.target === overlayRef.current) onClose();
  };

  let content: React.ReactNode;
  let footer: React.ReactNode = null;

  if (isLoading) {
    content = (
      <div className="p-6 text-sm text-[var(--color-text-tertiary)]">
        Loading tag cleanup...
      </div>
    );
  } else if (error || !signal) {
    content = (
      <div className="p-6 text-sm text-[var(--color-text-tertiary)]">
        {error ? 'Could not load this tag cleanup suggestion.' : 'This tag cleanup suggestion is no longer available.'}
      </div>
    );
  } else if (empty) {
    content = (
      <>
        <Header title={signal.title} summary={signal.summary} />
        <div className="mt-8 border border-[var(--color-border)] bg-[var(--color-bg-secondary)]/40 rounded-md p-4">
          <div className="text-sm font-medium text-[var(--color-text-primary)]">{empty.tag.name}</div>
          <div className="mt-1 text-xs text-[var(--color-text-tertiary)]">{formatPath(empty.tag)}</div>
          <div className="mt-4 grid grid-cols-2 gap-3 text-sm">
            <Metric label="Atoms" value="0" />
            <Metric label="Child tags" value={String(empty.tag.child_count)} />
          </div>
        </div>
      </>
    );
    footer = (
      <ActionBar>
        <button
          onClick={handleDeleteEmpty}
          disabled={isApplying}
          className="inline-flex items-center gap-2 rounded-md bg-red-500/15 px-3 py-2 text-sm text-red-300 hover:bg-red-500/25 disabled:opacity-60"
        >
          <Trash2 className="h-4 w-4" strokeWidth={2} />
          Delete empty tag
        </button>
        <button
          onClick={handleKeep}
          disabled={isApplying}
          className="inline-flex items-center gap-2 rounded-md px-3 py-2 text-sm text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-hover)] disabled:opacity-60"
        >
          <X className="h-4 w-4" strokeWidth={2} />
          Keep
        </button>
      </ActionBar>
    );
  } else if (redundancy && mergeChoice) {
    content = (
      <>
        <Header title={signal.title} summary={signal.summary} />

        <section className="mt-8">
          <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-[0.14em] text-[var(--color-text-tertiary)]">
            <ArrowLeftRight className="h-3.5 w-3.5" strokeWidth={2} />
            Choose the tag to keep
          </div>
          <p className="mt-2 text-sm text-[var(--color-text-tertiary)]">
            The selected tag remains and receives any missing atom assignments. The other tag is removed.
          </p>

          <div className="mt-4 grid gap-4 md:grid-cols-2">
            {[redundancy.primary_tag, redundancy.secondary_tag].map(tag => {
              const isTarget = mergeChoice.target.id === tag.id;
              return (
                <button
                  key={tag.id}
                  onClick={() => setTargetTagId(tag.id)}
                  className={`rounded-md border p-4 text-left transition-colors ${
                    isTarget
                      ? 'border-[var(--color-text-primary)] bg-[var(--color-bg-tertiary)]'
                      : 'border-[var(--color-border)] bg-[var(--color-bg-secondary)]/40 hover:border-[var(--color-border-hover)]'
                  }`}
                >
                  <div className="flex items-center justify-between gap-3">
                    <div className="flex min-w-0 items-start gap-3">
                      {isTarget ? (
                        <Radio className="mt-0.5 h-4 w-4 shrink-0 text-[var(--color-text-primary)]" strokeWidth={2.25} />
                      ) : (
                        <Circle className="mt-0.5 h-4 w-4 shrink-0 text-[var(--color-text-tertiary)]" strokeWidth={2} />
                      )}
                      <div className="min-w-0">
                        <div className="truncate text-sm font-medium text-[var(--color-text-primary)]">{tag.name}</div>
                        <div className="mt-1 truncate text-xs text-[var(--color-text-tertiary)]">{formatPath(tag)}</div>
                      </div>
                    </div>
                    <span className={`shrink-0 rounded border px-2 py-1 text-[11px] ${isTarget ? 'border-[var(--color-border-hover)] bg-[var(--color-bg-primary)] text-[var(--color-text-primary)]' : 'border-transparent bg-[var(--color-bg-tertiary)] text-[var(--color-text-secondary)]'}`}>
                      {isTarget ? 'Will remain' : 'Will be removed'}
                    </span>
                  </div>
                  <div className="mt-4 grid grid-cols-2 gap-3">
                    <Metric label="Atoms" value={String(tag.atom_count)} />
                    <Metric label="Unique" value={String(tag.id === redundancy.primary_tag.id ? redundancy.primary_unique_atom_count : redundancy.secondary_unique_atom_count)} />
                  </div>
                </button>
              );
            })}
          </div>
        </section>

        <div className="mt-8 grid gap-4 md:grid-cols-4">
          <Metric label="Shared atoms" value={String(redundancy.shared_atom_count)} />
          <Metric label="Overall overlap" value={pct(redundancy.jaccard_overlap)} />
          <Metric label="Smaller tag covered" value={pct(redundancy.containment_overlap)} />
          <Metric label="Relationship" value={redundancy.hierarchy_relationship.replace('_', ' ')} />
        </div>

        <div className="mt-8 rounded-md border border-[var(--color-border)] bg-[var(--color-bg-secondary)]/35 p-4">
          <div className="font-medium text-[var(--color-text-primary)]">Merge impact</div>
          <div className="mt-2 text-sm text-[var(--color-text-secondary)]">
            Keep <span className="text-[var(--color-text-primary)]">{mergeChoice.target.name}</span> and remove{' '}
            <span className="text-[var(--color-text-primary)]">{mergeChoice.source.name}</span>.
          </div>
          <div className="mt-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <Metric label="Atom assignments added" value={String(sourceUniqueAtomCount)} />
            <Metric label="Already shared" value={String(redundancy.shared_atom_count)} />
            <Metric label="Child tags moved" value={String(mergeChoice.source.child_count)} />
            <Metric label="Removed tag wiki" value={mergeChoice.source.has_wiki ? 'Deleted' : 'None'} />
          </div>
        </div>
      </>
    );
    footer = showMergeConfirm ? (
      <ActionBar>
        <div className="min-w-0 flex-1 text-sm text-[var(--color-text-secondary)]">
          <div className="font-medium text-[var(--color-text-primary)]">Confirm merge</div>
          <div className="mt-1">
            Keep <span className="text-[var(--color-text-primary)]">{mergeChoice.target.name}</span>, add{' '}
            {sourceUniqueAtomCount} missing atom assignments, move {mergeChoice.source.child_count} child tags, and remove{' '}
            <span className="text-[var(--color-text-primary)]">{mergeChoice.source.name}</span>.
          </div>
        </div>
        <button
          onClick={handleMerge}
          disabled={isApplying}
          className="inline-flex items-center gap-2 rounded-md bg-[var(--color-accent)] px-3 py-2 text-sm font-medium text-white hover:brightness-110 disabled:opacity-60"
        >
          <GitMerge className="h-4 w-4" strokeWidth={2} />
          Confirm merge
        </button>
        <button
          onClick={() => setShowMergeConfirm(false)}
          disabled={isApplying}
          className="inline-flex items-center gap-2 rounded-md px-3 py-2 text-sm text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-hover)] disabled:opacity-60"
        >
          <X className="h-4 w-4" strokeWidth={2} />
          Cancel
        </button>
      </ActionBar>
    ) : (
      <ActionBar>
        <button
          onClick={() => setShowMergeConfirm(true)}
          disabled={isApplying}
          className="inline-flex items-center gap-2 rounded-md bg-[var(--color-accent)] px-3 py-2 text-sm font-medium text-white hover:brightness-110 disabled:opacity-60"
        >
          <GitMerge className="h-4 w-4" strokeWidth={2} />
          Merge tags
        </button>
        <button
          onClick={handleKeep}
          disabled={isApplying}
          className="inline-flex items-center gap-2 rounded-md px-3 py-2 text-sm text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-hover)] disabled:opacity-60"
        >
          <X className="h-4 w-4" strokeWidth={2} />
          Keep separate
        </button>
      </ActionBar>
    );
  } else {
    content = null;
  }

  return createPortal(
    <div
      ref={overlayRef}
      onClick={handleOverlayClick}
      data-modal="true"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-3 backdrop-blur-sm safe-area-padding md:p-6"
    >
      <div className="flex max-h-[92vh] w-full max-w-5xl flex-col overflow-hidden rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-panel)] shadow-xl">
        <div className="flex items-center justify-between gap-4 border-b border-[var(--color-border)] px-5 py-3">
          <div className="text-sm font-medium text-[var(--color-text-primary)]">Tag cleanup</div>
          <button
            onClick={onClose}
            disabled={isApplying}
            className="rounded-md p-1 text-[var(--color-text-secondary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)] disabled:opacity-60"
            aria-label="Close tag cleanup"
          >
            <X className="h-5 w-5" strokeWidth={2} />
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-6 md:px-8">
          {content}
        </div>
        {footer}
      </div>
    </div>,
    document.body,
  );
}

function Header({ title, summary }: { title: string; summary: string }) {
  return (
    <header>
      <div className="min-w-0">
        <div className="text-xs font-medium uppercase tracking-[0.14em] text-[var(--color-text-tertiary)]">Tag cleanup</div>
        <h1 className="mt-2 text-2xl font-semibold tracking-normal text-[var(--color-text-primary)]">{title}</h1>
        <p className="mt-2 max-w-2xl text-sm text-[var(--color-text-tertiary)]">{summary}</p>
      </div>
    </header>
  );
}

function ActionBar({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex flex-wrap items-center justify-end gap-2 border-t border-[var(--color-border)] bg-[var(--color-bg-panel)] px-5 py-3 shadow-[0_-10px_30px_rgba(0,0,0,0.18)] md:px-8">
      {children}
    </div>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-md border border-[var(--color-border)] bg-[var(--color-bg-secondary)]/35 px-3 py-2">
      <div className="text-[11px] uppercase tracking-[0.12em] text-[var(--color-text-tertiary)]">{label}</div>
      <div className="mt-1 truncate text-sm font-medium text-[var(--color-text-primary)]">{value}</div>
    </div>
  );
}
