import { useEffect, useState } from 'react';
import { FileText, Link2Off, X } from 'lucide-react';
import { Section } from '../Section';
import { getTransport } from '../../../lib/transport';
import { useUIStore } from '../../../stores/ui';
import type { BrokenInternalLinkEvidence, KnowledgeSignal } from '../../../types/knowledgeSignals';
import { BrokenLinksHelp } from '../signalHelpContent';
import { useDashboardSignals } from '../DashboardSignalsContext';

const MAX_ITEMS = 5;

type BrokenLinkSignal = KnowledgeSignal<BrokenInternalLinkEvidence>;

function targetLabel(evidence: BrokenInternalLinkEvidence): string {
  if (evidence.label?.trim()) return evidence.label;
  return evidence.raw_target;
}

function reasonText(evidence: BrokenInternalLinkEvidence): string {
  const target = targetLabel(evidence);
  if (evidence.status === 'missing' && evidence.target_kind === 'atom_id') {
    return `Missing atom link: ${target}`;
  }
  return `Unresolved note link: ${target}`;
}

export function BrokenLinksWidget() {
  const openReader = useUIStore(s => s.openReader);
  const dashboardSignals = useDashboardSignals();
  const hasDashboardSignals = dashboardSignals !== null;
  const [localSignals, setLocalSignals] = useState<BrokenLinkSignal[]>([]);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const signals = dashboardSignals
    ? dashboardSignals.getProviderSignals<BrokenInternalLinkEvidence>('broken_internal_link').slice(0, MAX_ITEMS)
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
        const result = await getTransport().invoke<BrokenLinkSignal[]>('list_knowledge_signals', {
          providerId: 'broken_internal_link',
          limit: MAX_ITEMS,
        });
        if (!cancelled) {
          setLocalSignals(result);
          setIsLoading(false);
        }
      } catch (err) {
        if (!cancelled) {
          console.error('Failed to load broken link suggestions:', err);
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
      console.error('Failed to dismiss broken link suggestion:', err);
      if (dashboardSignals) {
        void dashboardSignals.refreshProvider('broken_internal_link', MAX_ITEMS);
      } else {
        setLocalSignals(previous);
      }
    }
  };

  return (
    <Section label="Broken links" action={<BrokenLinksHelp />}>
      {loading ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Loading broken links...</div>
      ) : loadError ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Could not load broken links.</div>
      ) : signals.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">No broken-link suggestions.</div>
      ) : (
        <ul className="-mx-2">
          {signals.map(signal => {
            const evidence = signal.evidence;
            if (!evidence) return null;
            return (
              <li key={signal.id} className="group flex items-start gap-2 rounded px-2 py-1.5 hover:bg-[var(--color-bg-hover)]/60">
                <div className="mt-0.5 shrink-0 text-[var(--color-text-tertiary)]">
                  <Link2Off className="h-3.5 w-3.5" strokeWidth={2} />
                </div>
                <div className="min-w-0 flex-1">
                  <span className="block truncate text-sm text-[var(--color-text-secondary)]">
                    {evidence.source_atom_title || 'Untitled'}
                  </span>
                  <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                    {reasonText(evidence)}
                  </span>
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  <button
                    onClick={() => openReader(evidence.source_atom_id)}
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
