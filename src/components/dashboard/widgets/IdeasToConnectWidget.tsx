import { useEffect, useState } from 'react';
import { Check, FileText, X } from 'lucide-react';
import { toast } from 'sonner';
import { Section } from '../Section';
import { getTransport } from '../../../lib/transport';
import { useAtomsStore } from '../../../stores/atoms';
import { useTagsStore } from '../../../stores/tags';
import { useUIStore } from '../../../stores/ui';
import type { KnowledgeSignal, KnowledgeSignalActionResult, MissingTagOverlapEvidence } from '../../../types/knowledgeSignals';
import { IdeasToConnectHelp } from '../signalHelpContent';
import { useDashboardSignals } from '../DashboardSignalsContext';

const MAX_ITEMS = 5;

type MissingTagSignal = KnowledgeSignal<MissingTagOverlapEvidence>;

function pct(value: number): string {
  return `${Math.round(value * 100)}%`;
}

export function IdeasToConnectWidget() {
  const openReader = useUIStore(s => s.openReader);
  const fetchAtoms = useAtomsStore(s => s.fetchAtoms);
  const fetchTags = useTagsStore(s => s.fetchTags);
  const dashboardSignals = useDashboardSignals();
  const hasDashboardSignals = dashboardSignals !== null;
  const [localSignals, setLocalSignals] = useState<MissingTagSignal[]>([]);
  const [isLoading, setIsLoading] = useState(true);
  const [isApplying, setIsApplying] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const signals = dashboardSignals
    ? dashboardSignals.getProviderSignals<MissingTagOverlapEvidence>('missing_tag_overlap').slice(0, MAX_ITEMS)
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
        const result = await getTransport().invoke<MissingTagSignal[]>('list_knowledge_signals', {
          providerId: 'missing_tag_overlap',
          limit: MAX_ITEMS,
        });
        if (!cancelled) {
          setLocalSignals(result);
          setIsLoading(false);
        }
      } catch (err) {
        if (!cancelled) {
          console.error('Failed to load connection suggestions:', err);
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

  const removeSignal = (signalKey: string) => {
    if (dashboardSignals) {
      dashboardSignals.removeSignal(signalKey);
    } else {
      setLocalSignals(current => current.filter(signal => signal.id !== signalKey));
    }
  };

  const dismissSignal = async (signalKey: string) => {
    const previous = signals;
    removeSignal(signalKey);
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey } }));
    } catch (err) {
      console.error('Failed to dismiss connection suggestion:', err);
      if (dashboardSignals) {
        void dashboardSignals.refreshProvider('missing_tag_overlap', MAX_ITEMS);
      } else {
        setLocalSignals(previous);
      }
    }
  };

  const addTag = async (signal: MissingTagSignal) => {
    const evidence = signal.evidence;
    if (!evidence) return;
    const previous = signals;
    setIsApplying(signal.id);
    removeSignal(signal.id);
    try {
      const result = await getTransport().invoke<KnowledgeSignalActionResult>('apply_knowledge_signal_action', {
        signalKey: signal.id,
        action: 'add_tag_to_atom',
      });
      await Promise.all([fetchAtoms(), fetchTags()]);
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey: signal.id } }));
      toast.success('Tag added', {
        description: `${evidence.suggested_tag.name} added to ${evidence.atom_title}`,
        action: result.undo_supported
          ? {
              label: 'Undo',
              onClick: async () => {
                try {
                  await getTransport().invoke('undo_knowledge_signal_action', {
                    actionLogId: result.action_log_id,
                  });
                  await Promise.all([fetchAtoms(), fetchTags()]);
                  toast.success('Tag removed');
                } catch (err) {
                  toast.error('Failed to undo tag add', { description: String(err) });
                }
              },
            }
          : undefined,
      });
    } catch (err) {
      console.error('Failed to add suggested tag:', err);
      if (dashboardSignals) {
        void dashboardSignals.refreshProvider('missing_tag_overlap', MAX_ITEMS);
      } else {
        setLocalSignals(previous);
      }
      toast.error('Failed to add tag', { description: String(err) });
    } finally {
      setIsApplying(null);
    }
  };

  return (
    <Section label="Ideas to connect" action={<IdeasToConnectHelp />}>
      {loading ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Loading connection ideas...</div>
      ) : loadError ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">Could not load connection ideas.</div>
      ) : signals.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">No connection suggestions.</div>
      ) : (
        <ul className="-mx-2">
          {signals.map(signal => {
            const evidence = signal.evidence;
            if (!evidence) return null;
            return (
              <li key={signal.id} className="group flex items-start gap-2 rounded px-2 py-1.5 hover:bg-[var(--color-bg-hover)]/60">
                <div className="min-w-0 flex-1">
                  <span className="flex min-w-0 items-center gap-2 text-sm">
                    <span className="shrink-0 text-[var(--color-text-tertiary)]">Add</span>
                    <span className="min-w-0 truncate rounded border border-[var(--color-border)] bg-[var(--color-bg-tertiary)] px-1.5 py-0.5 text-xs font-medium text-[var(--color-text-primary)]">
                      {evidence.suggested_tag.name}
                    </span>
                  </span>
                  <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                    {evidence.atom_title || 'Untitled'} / {evidence.nearby_tagged_atom_count} nearby atoms / {pct(evidence.average_similarity)} avg similarity
                  </span>
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  <button
                    onClick={() => addTag(signal)}
                    disabled={isApplying === signal.id}
                    title="Add suggested tag"
                    className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)] disabled:opacity-60"
                  >
                    <Check className="h-3.5 w-3.5" strokeWidth={2} />
                  </button>
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
