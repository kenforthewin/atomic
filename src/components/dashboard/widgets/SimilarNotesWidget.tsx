import { useEffect, useState } from 'react';
import { FileText, Search, X } from 'lucide-react';
import { Section } from '../Section';
import { getTransport } from '../../../lib/transport';
import { useUIStore } from '../../../stores/ui';
import type {
  KnowledgeSignal,
  NearDuplicateAtomEvidence,
  SourceDuplicateEvidence,
} from '../../../types/knowledgeSignals';
import { SimilarNotesHelp } from '../signalHelpContent';
import { useDashboardSignals } from '../DashboardSignalsContext';

const MAX_ITEMS = 5;

type SimilarNotesEvidence = NearDuplicateAtomEvidence | SourceDuplicateEvidence;
type SimilarNotesSignal = KnowledgeSignal<SimilarNotesEvidence>;

function pct(value: number): string {
  return `${Math.round(value * 100)}%`;
}

function isSourceDuplicateEvidence(evidence: SimilarNotesEvidence): evidence is SourceDuplicateEvidence {
  return evidence.schema === 'source_duplicate' || 'normalized_source_url' in evidence;
}

function reasonText(signal: SimilarNotesSignal): string {
  const evidence = signal.evidence;
  if (!evidence) return signal.reasons.slice(0, 2).map(reason => reason.label).join(' / ');

  if (isSourceDuplicateEvidence(evidence)) {
    const reasons = ['Same source'];
    if (evidence.duplicate_count > 2) {
      reasons.push(`${evidence.duplicate_count} captures`);
    }
    reasons.push(`${pct(evidence.content_length_ratio)} length match`);
    return reasons.join(' / ');
  }

  const reasons = ['Very similar meaning'];
  if (evidence.source_match === 'same_source') {
    reasons.push('Same source');
  }
  if (evidence.shared_tag_count > 0) {
    reasons.push(`${evidence.shared_tag_count} shared ${evidence.shared_tag_count === 1 ? 'tag' : 'tags'}`);
  }
  return `${reasons.slice(0, 3).join(' / ')} / ${pct(evidence.semantic_similarity)} similarity`;
}

export function SimilarNotesWidget() {
  const openReader = useUIStore(s => s.openReader);
  const dashboardSignals = useDashboardSignals();
  const hasDashboardSignals = dashboardSignals !== null;
  const [localSignals, setLocalSignals] = useState<SimilarNotesSignal[]>([]);
  const [reviewSignal, setReviewSignal] = useState<SimilarNotesSignal | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const signals = dashboardSignals
    ? [
        ...dashboardSignals.getProviderSignals<SimilarNotesEvidence>('near_duplicate_atom'),
        ...dashboardSignals.getProviderSignals<SimilarNotesEvidence>('source_duplicate'),
      ].sort((a, b) => b.score - a.score || b.confidence - a.confidence).slice(0, MAX_ITEMS)
    : localSignals;
  const loading = dashboardSignals ? dashboardSignals.isLoading : isLoading;
  const loadError = dashboardSignals ? dashboardSignals.error : error;

  useEffect(() => {
    if (hasDashboardSignals) return;
    let cancelled = false;

    async function fetchSignals() {
      setIsLoading(true);
      setError(null);
      try {
        const [nearDuplicates, sourceDuplicates] = await Promise.all([
          getTransport().invoke<SimilarNotesSignal[]>('list_knowledge_signals', {
            providerId: 'near_duplicate_atom',
            limit: MAX_ITEMS,
          }),
          getTransport().invoke<SimilarNotesSignal[]>('list_knowledge_signals', {
            providerId: 'source_duplicate',
            limit: MAX_ITEMS,
          }),
        ]);
        if (!cancelled) {
          setLocalSignals(
            [...nearDuplicates, ...sourceDuplicates]
              .sort((a, b) => b.score - a.score || b.confidence - a.confidence)
              .slice(0, MAX_ITEMS)
          );
          setIsLoading(false);
        }
      } catch (err) {
        if (!cancelled) {
          console.error('Failed to load similar notes:', err);
          setError(String(err));
          setIsLoading(false);
        }
      }
    }

    fetchSignals();
    return () => {
      cancelled = true;
    };
  }, [hasDashboardSignals]);

  const dismissSignal = async (signalKey: string) => {
    const previous = localSignals;
    if (dashboardSignals) {
      dashboardSignals.removeSignal(signalKey);
    } else {
      setLocalSignals(current => current.filter(signal => signal.id !== signalKey));
    }
    if (reviewSignal?.id === signalKey) {
      setReviewSignal(null);
    }
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey } }));
    } catch (err) {
      console.error('Failed to dismiss similar note suggestion:', err);
      if (dashboardSignals) {
        void Promise.all([
          dashboardSignals.refreshProvider('near_duplicate_atom', MAX_ITEMS),
          dashboardSignals.refreshProvider('source_duplicate', MAX_ITEMS),
        ]);
      } else {
        setLocalSignals(previous);
      }
    }
  };

  return (
    <>
      <Section label="Similar notes" action={<SimilarNotesHelp />}>
        {loading ? (
          <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Loading similar notes...</div>
        ) : loadError ? (
          <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Could not load similar notes.</div>
        ) : signals.length === 0 ? (
          <div className="py-6 text-sm text-[var(--color-text-tertiary)]">No similar-note suggestions.</div>
        ) : (
          <ul className="-mx-2">
            {signals.map(signal => {
              const evidence = signal.evidence;
              if (!evidence) return null;
              return (
                <li key={signal.id} className="group flex items-start gap-2 rounded px-2 py-1.5 hover:bg-[var(--color-bg-hover)]/60">
                  <div className="min-w-0 flex-1">
                    <div className="flex min-w-0 items-center gap-1.5 text-sm text-[var(--color-text-secondary)]">
                      <span className="min-w-0 truncate">{evidence.primary_atom.title || 'Untitled'}</span>
                      <span className="shrink-0 text-[var(--color-text-tertiary)]">/</span>
                      <span className="min-w-0 truncate">{evidence.secondary_atom.title || 'Untitled'}</span>
                    </div>
                    <div className="mt-0.5 truncate text-[11px] text-[var(--color-text-tertiary)]">
                      {reasonText(signal)}
                    </div>
                  </div>
                  <div className="flex shrink-0 items-center gap-1">
                    <button
                      onClick={() => setReviewSignal(signal)}
                      title="Review similar atoms"
                      className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
                    >
                      <Search className="h-3.5 w-3.5" strokeWidth={2} />
                    </button>
                    <button
                      onClick={() => dismissSignal(signal.id)}
                      title="Keep separate"
                      className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
                    >
                      <X className="h-3.5 w-3.5" strokeWidth={2} />
                    </button>
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </Section>

      {reviewSignal?.evidence && (
        <SimilarNotesReviewModal
          signal={reviewSignal}
          onClose={() => setReviewSignal(null)}
          onOpenAtom={openReader}
          onKeepSeparate={() => dismissSignal(reviewSignal.id)}
        />
      )}
    </>
  );
}

interface SimilarNotesReviewModalProps {
  signal: SimilarNotesSignal;
  onClose: () => void;
  onOpenAtom: (atomId: string) => void;
  onKeepSeparate: () => void;
}

function SimilarNotesReviewModal({ signal, onClose, onOpenAtom, onKeepSeparate }: SimilarNotesReviewModalProps) {
  const evidence = signal.evidence;
  if (!evidence) return null;
  const sourceDuplicate = isSourceDuplicateEvidence(evidence);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 px-4 py-6 backdrop-blur-sm" role="dialog" aria-modal="true">
      <div className="flex max-h-full w-full max-w-3xl flex-col overflow-hidden rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-panel)] shadow-2xl">
        <div className="flex items-start justify-between gap-4 border-b border-[var(--color-border)] px-5 py-4">
          <div className="min-w-0">
            <h2 className="text-base font-semibold text-[var(--color-text-primary)]">Review similar atoms</h2>
            <p className="mt-1 text-sm text-[var(--color-text-tertiary)]">{signal.summary}</p>
          </div>
          <button
            onClick={onClose}
            title="Close"
            className="rounded p-1 text-[var(--color-text-tertiary)] hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
          >
            <X className="h-4 w-4" strokeWidth={2} />
          </button>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
          <div className="grid gap-3 md:grid-cols-2">
            <AtomSummary atom={evidence.primary_atom} />
            <AtomSummary atom={evidence.secondary_atom} />
          </div>

          <div className="mt-4 grid gap-2 text-sm sm:grid-cols-2">
            {sourceDuplicate ? (
              <Metric label="Match type" value="Same source URL" />
            ) : (
              <Metric label="Meaning similarity" value={pct(evidence.semantic_similarity)} />
            )}
            <Metric label="Title similarity" value={pct(evidence.title_similarity)} />
            <Metric label="Length match" value={pct(evidence.content_length_ratio)} />
            {sourceDuplicate ? (
              <>
                <Metric label="Source" value="Same source" />
                <Metric label="Captures" value={String(evidence.duplicate_count)} />
              </>
            ) : (
              <>
                <Metric label="Source" value={evidence.source_match === 'same_source' ? 'Same source' : 'Different or missing'} />
                <Metric label="Shared tags" value={String(evidence.shared_tag_count)} />
              </>
            )}
          </div>

          {!sourceDuplicate && evidence.shared_tags.length > 0 && (
            <div className="mt-4">
              <div className="text-xs uppercase text-[var(--color-text-tertiary)]">Shared tags</div>
              <div className="mt-2 flex flex-wrap gap-1.5">
                {evidence.shared_tags.map(tag => (
                  <span key={tag.id} className="rounded border border-[var(--color-border)] bg-[var(--color-bg-tertiary)] px-1.5 py-0.5 text-xs text-[var(--color-text-secondary)]">
                    {tag.name}
                  </span>
                ))}
              </div>
            </div>
          )}
        </div>

        <div className="flex flex-wrap justify-end gap-2 border-t border-[var(--color-border)] bg-[var(--color-bg-panel)] px-5 py-3 shadow-[0_-10px_30px_rgba(0,0,0,0.18)]">
          <button
            onClick={() => onOpenAtom(evidence.primary_atom.id)}
            className="inline-flex items-center gap-1.5 rounded border border-[var(--color-border)] px-3 py-1.5 text-sm text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
          >
            <FileText className="h-3.5 w-3.5" strokeWidth={2} />
            Open first
          </button>
          <button
            onClick={() => onOpenAtom(evidence.secondary_atom.id)}
            className="inline-flex items-center gap-1.5 rounded border border-[var(--color-border)] px-3 py-1.5 text-sm text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
          >
            <FileText className="h-3.5 w-3.5" strokeWidth={2} />
            Open second
          </button>
          <button
            onClick={onKeepSeparate}
            className="inline-flex items-center gap-1.5 rounded bg-[var(--color-bg-tertiary)] px-3 py-1.5 text-sm text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]"
          >
            <X className="h-3.5 w-3.5" strokeWidth={2} />
            Keep separate
          </button>
        </div>
      </div>
    </div>
  );
}

function AtomSummary({ atom }: { atom: NearDuplicateAtomEvidence['primary_atom'] }) {
  return (
    <div className="rounded border border-[var(--color-border)] bg-[var(--color-bg-tertiary)] p-3">
      <div className="truncate text-sm font-medium text-[var(--color-text-primary)]">{atom.title || 'Untitled'}</div>
      {atom.source_url && <div className="mt-1 truncate text-xs text-[var(--color-text-tertiary)]">{atom.source_url}</div>}
      <div className="mt-2 text-xs text-[var(--color-text-tertiary)]">{atom.content_length.toLocaleString()} characters</div>
    </div>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-3 rounded border border-[var(--color-border)] px-3 py-2">
      <span className="text-[var(--color-text-tertiary)]">{label}</span>
      <span className="font-medium text-[var(--color-text-secondary)]">{value}</span>
    </div>
  );
}
