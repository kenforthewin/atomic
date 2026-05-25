import { useEffect, useState } from 'react';
import { FileText, Loader2, X } from 'lucide-react';
import { toast } from 'sonner';
import { Section } from '../Section';
import { useUIStore } from '../../../stores/ui';
import { getTransport } from '../../../lib/transport';
import type { KnowledgeSignal, KnowledgeSignalActionResult, WikiUpdateEvidence } from '../../../types/knowledgeSignals';
import { RevisionsHelp } from '../signalHelpContent';
import { useDashboardSignals } from '../DashboardSignalsContext';

const MAX_ITEMS = 5;

export function RevisionsWidget() {
  const openWikiReader = useUIStore(s => s.openWikiReader);
  const dashboardSignals = useDashboardSignals();
  const hasDashboardSignals = dashboardSignals !== null;
  const [localSignals, setLocalSignals] = useState<KnowledgeSignal<WikiUpdateEvidence>[]>([]);
  const [applyingSignalId, setApplyingSignalId] = useState<string | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const signals = dashboardSignals
    ? dashboardSignals.getProviderSignals<WikiUpdateEvidence>('wiki_update').slice(0, MAX_ITEMS)
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
        const result = await getTransport().invoke<KnowledgeSignal<WikiUpdateEvidence>[]>('list_knowledge_signals', {
          providerId: 'wiki_update',
          limit: MAX_ITEMS,
        });
        if (!cancelled) {
          setLocalSignals(result);
          setIsLoading(false);
        }
      } catch (err) {
        if (!cancelled) {
          console.error('Failed to load wiki revision suggestions:', err);
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

  const refreshSignals = async () => {
    if (dashboardSignals) {
      await dashboardSignals.refreshProvider('wiki_update', MAX_ITEMS);
      return;
    }
    const result = await getTransport().invoke<KnowledgeSignal<WikiUpdateEvidence>[]>('list_knowledge_signals', {
      providerId: 'wiki_update',
      limit: MAX_ITEMS,
    });
    setLocalSignals(result);
  };

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
      console.error('Failed to dismiss wiki revision suggestion:', err);
      if (dashboardSignals) {
        void dashboardSignals.refreshProvider('wiki_update', MAX_ITEMS);
      } else {
        setLocalSignals(previous);
      }
    }
  };

  const createUpdateProposal = async (signal: KnowledgeSignal<WikiUpdateEvidence>) => {
    const tagId = signal.evidence?.tag_id ?? signal.target.id;
    const tagName = signal.evidence?.tag_name ?? signal.target.label;
    if (applyingSignalId) return;
    setApplyingSignalId(signal.id);
    try {
      const result = await getTransport().invoke<KnowledgeSignalActionResult<{ status?: string }>>('apply_knowledge_signal_action', {
        signalKey: signal.id,
        action: 'update_wiki',
      });
      await refreshSignals();
      openWikiReader(tagId, tagName);
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey: signal.id } }));
      toast.success(
        result.result?.status === 'no_update_needed' ? 'Wiki already up to date' : 'Wiki update proposal ready',
        { description: tagName }
      );
    } catch (err) {
      console.error('Failed to create wiki update proposal:', err);
      toast.error('Failed to create update proposal', { description: String(err) });
    } finally {
      setApplyingSignalId(null);
    }
  };

  return (
    <Section label="Revision suggestions" action={<RevisionsHelp />}>
      {loading ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          Loading revision suggestions...
        </div>
      ) : loadError ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          Could not load revision suggestions.
        </div>
      ) : signals.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          All wikis are up to date.
        </div>
      ) : (
        <ul className="-mx-2">
          {signals.map(signal => {
            const tagName = signal.evidence?.tag_name ?? signal.target.label;
            const newAtomCount = signal.evidence?.new_atom_count ?? 0;
            const isApplying = applyingSignalId === signal.id;

            return (
              <li key={signal.id} className="group flex items-start gap-2 px-2 py-1.5 rounded hover:bg-[var(--color-bg-hover)]/60">
                <div className="min-w-0 flex-1">
                  <span className="flex items-baseline gap-3">
                    <span className="flex-1 min-w-0 truncate text-sm text-[var(--color-text-secondary)]">
                      {tagName}
                    </span>
                    {newAtomCount > 0 && (
                      <span className="text-[11px] text-amber-400/90 tabular-nums shrink-0">
                        +{newAtomCount}
                      </span>
                    )}
                  </span>
                  {signal.reasons.length > 0 && (
                    <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                      {signal.reasons.slice(0, 2).map(reason => reason.label).join(' / ')}
                    </span>
                  )}
                </div>
                <button
                  onClick={() => createUpdateProposal(signal)}
                  disabled={applyingSignalId !== null}
                  title={isApplying ? 'Creating update proposal' : 'Create update proposal'}
                  className="mt-0.5 rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)] disabled:cursor-wait disabled:opacity-60"
                >
                  {isApplying ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={2} /> : <FileText className="w-3.5 h-3.5" strokeWidth={2} />}
                </button>
                <button
                  onClick={() => dismissSignal(signal.id)}
                  title="Dismiss suggestion"
                  className="mt-0.5 rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
                >
                  <X className="w-3.5 h-3.5" strokeWidth={2} />
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </Section>
  );
}
