import { useEffect, useState } from 'react';
import { Search, X } from 'lucide-react';
import { Section } from '../Section';
import { getTransport } from '../../../lib/transport';
import { useUIStore } from '../../../stores/ui';
import type { EmptyTagEvidence, KnowledgeSignal, TagCleanupEvidence, TagRedundancyEvidence } from '../../../types/knowledgeSignals';
import { TagCleanupHelp } from '../signalHelpContent';
import { useDashboardSignals } from '../DashboardSignalsContext';

const MAX_ITEMS = 5;

type TagCleanupSignal = KnowledgeSignal<TagCleanupEvidence>;

function isRedundancyEvidence(value: TagCleanupEvidence | undefined): value is TagRedundancyEvidence {
  return !!value && 'primary_tag' in value && 'secondary_tag' in value;
}

function signalLabel(signal: TagCleanupSignal): string {
  if (isRedundancyEvidence(signal.evidence)) {
    return `${signal.evidence.primary_tag.name} / ${signal.evidence.secondary_tag.name}`;
  }
  const empty = signal.evidence as EmptyTagEvidence | undefined;
  return empty?.tag?.name ?? signal.target.label;
}

export function TagCleanupWidget() {
  const openTagCleanupReview = useUIStore(s => s.openTagCleanupReview);
  const dashboardSignals = useDashboardSignals();
  const hasDashboardSignals = dashboardSignals !== null;
  const [localSignals, setLocalSignals] = useState<TagCleanupSignal[]>([]);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const signals = dashboardSignals
    ? [
        ...dashboardSignals.getProviderSignals<TagCleanupEvidence>('tag_redundancy'),
        ...dashboardSignals.getProviderSignals<TagCleanupEvidence>('empty_tag'),
      ].sort((a, b) => b.score - a.score).slice(0, MAX_ITEMS)
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
        const [redundancy, empty] = await Promise.all([
          getTransport().invoke<TagCleanupSignal[]>('list_knowledge_signals', {
            providerId: 'tag_redundancy',
            limit: MAX_ITEMS,
          }),
          getTransport().invoke<TagCleanupSignal[]>('list_knowledge_signals', {
            providerId: 'empty_tag',
            limit: MAX_ITEMS,
          }),
        ]);
        if (!cancelled) {
          setLocalSignals([...redundancy, ...empty].sort((a, b) => b.score - a.score).slice(0, MAX_ITEMS));
          setIsLoading(false);
        }
      } catch (err) {
        if (!cancelled) {
          console.error('Failed to load tag cleanup suggestions:', err);
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

  useEffect(() => {
    if (hasDashboardSignals) return;
    const handleSignalChanged = (event: Event) => {
      const signalKey = event instanceof CustomEvent ? event.detail?.signalKey : null;
      if (typeof signalKey === 'string') {
        setLocalSignals(current => current.filter(signal => signal.id !== signalKey));
      }
    };
    window.addEventListener('knowledge-signals:changed', handleSignalChanged);
    return () => window.removeEventListener('knowledge-signals:changed', handleSignalChanged);
  }, [hasDashboardSignals]);

  const dismissSignal = async (signalKey: string) => {
    const previous = localSignals;
    if (dashboardSignals) {
      dashboardSignals.removeSignal(signalKey);
    } else {
      setLocalSignals(current => current.filter(signal => signal.id !== signalKey));
    }
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey } }));
    } catch (err) {
      console.error('Failed to dismiss tag cleanup suggestion:', err);
      if (dashboardSignals) {
        void Promise.all([
          dashboardSignals.refreshProvider('tag_redundancy', MAX_ITEMS),
          dashboardSignals.refreshProvider('empty_tag', MAX_ITEMS),
        ]);
      } else {
        setLocalSignals(previous);
      }
    }
  };

  return (
    <Section label="Tag cleanup" action={<TagCleanupHelp />}>
      {loading ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Loading tag cleanup...</div>
      ) : loadError ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Could not load tag cleanup.</div>
      ) : signals.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">No tag cleanup suggestions.</div>
      ) : (
        <ul className="-mx-2">
          {signals.map(signal => (
            <li key={signal.id} className="group flex items-start gap-2 px-2 py-1.5 rounded hover:bg-[var(--color-bg-hover)]/60">
              <div className="min-w-0 flex-1">
                <span className="block truncate text-sm text-[var(--color-text-secondary)] group-hover:text-[var(--color-text-primary)]">
                  {signalLabel(signal)}
                </span>
                {signal.reasons.length > 0 && (
                  <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                    {signal.reasons.slice(0, 2).map(reason => reason.label).join(' / ')}
                  </span>
                )}
              </div>
              <div className="flex shrink-0 items-center gap-1">
                <button
                  onClick={() => openTagCleanupReview(signal.id, signalLabel(signal))}
                  title="Review tag cleanup"
                  className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
                >
                  <Search className="h-3.5 w-3.5" strokeWidth={2} />
                </button>
                <button
                  onClick={() => dismissSignal(signal.id)}
                  title="Dismiss suggestion"
                  className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
                >
                  <X className="w-3.5 h-3.5" strokeWidth={2} />
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </Section>
  );
}
