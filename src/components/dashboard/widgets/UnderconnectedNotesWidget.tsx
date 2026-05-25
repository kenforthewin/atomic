import { useEffect, useState } from 'react';
import { FileText, X } from 'lucide-react';
import { Section } from '../Section';
import { getTransport } from '../../../lib/transport';
import { useUIStore } from '../../../stores/ui';
import type { KnowledgeSignal, UnderconnectedAtomEvidence } from '../../../types/knowledgeSignals';
import { UnderconnectedNotesHelp } from '../signalHelpContent';
import { useDashboardSignals } from '../DashboardSignalsContext';

const MAX_ITEMS = 5;

type UnderconnectedSignal = KnowledgeSignal<UnderconnectedAtomEvidence>;

function reasonText(signal: UnderconnectedSignal): string {
  const evidence = signal.evidence;
  if (!evidence) return signal.reasons.slice(0, 2).map(reason => reason.label).join(' / ');

  const edgeText =
    evidence.strong_edge_count === 0
      ? 'No strong connections'
      : `${evidence.strong_edge_count} strong ${evidence.strong_edge_count === 1 ? 'connection' : 'connections'}`;
  const tagText = evidence.tag_count === 0 ? 'No tags' : `${evidence.tag_count} ${evidence.tag_count === 1 ? 'tag' : 'tags'}`;
  const closestText =
    typeof evidence.strongest_similarity === 'number'
      ? `${Math.round(evidence.strongest_similarity * 100)}% closest match`
      : 'No semantic matches';

  return `${edgeText} / ${tagText} / ${closestText}`;
}

export function UnderconnectedNotesWidget() {
  const openReader = useUIStore(s => s.openReader);
  const dashboardSignals = useDashboardSignals();
  const hasDashboardSignals = dashboardSignals !== null;
  const [localSignals, setLocalSignals] = useState<UnderconnectedSignal[]>([]);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const signals = dashboardSignals
    ? dashboardSignals.getProviderSignals<UnderconnectedAtomEvidence>('underconnected_atom').slice(0, MAX_ITEMS)
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
        const result = await getTransport().invoke<UnderconnectedSignal[]>('list_knowledge_signals', {
          providerId: 'underconnected_atom',
          limit: MAX_ITEMS,
        });
        if (!cancelled) {
          setLocalSignals(result);
          setIsLoading(false);
        }
      } catch (err) {
        if (!cancelled) {
          console.error('Failed to load underconnected notes:', err);
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
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey } }));
    } catch (err) {
      console.error('Failed to dismiss underconnected note suggestion:', err);
      if (dashboardSignals) {
        void dashboardSignals.refreshProvider('underconnected_atom', MAX_ITEMS);
      } else {
        setLocalSignals(previous);
      }
    }
  };

  return (
    <Section label="Underconnected notes" action={<UnderconnectedNotesHelp />}>
      {loading ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Loading underconnected notes...</div>
      ) : loadError ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Could not load underconnected notes.</div>
      ) : signals.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">No underconnected-note suggestions.</div>
      ) : (
        <ul className="-mx-2">
          {signals.map(signal => {
            const evidence = signal.evidence;
            if (!evidence) return null;
            return (
              <li key={signal.id} className="group flex items-start gap-2 rounded px-2 py-1.5 hover:bg-[var(--color-bg-hover)]/60">
                <div className="min-w-0 flex-1">
                  <span className="block truncate text-sm text-[var(--color-text-secondary)] group-hover:text-[var(--color-text-primary)]">
                    {evidence.atom_title || 'Untitled'}
                  </span>
                  <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                    {reasonText(signal)}
                  </span>
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  <button
                    onClick={() => openReader(evidence.atom_id)}
                    title="Open atom"
                    className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
                  >
                    <FileText className="h-3.5 w-3.5" strokeWidth={2} />
                  </button>
                  <button
                    onClick={() => dismissSignal(signal.id)}
                    title="Dismiss suggestion"
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
  );
}
