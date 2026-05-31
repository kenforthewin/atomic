import { useState } from 'react';
import { Section } from '../Section';
import { useWikiStore } from '../../../stores/wiki';
import { BookOpen, Loader2, X } from 'lucide-react';
import { toast } from 'sonner';
import { getTransport } from '../../../lib/transport';
import type { KnowledgeSignal, KnowledgeSignalActionResult, WikiCandidateEvidence } from '../../../types/knowledgeSignals';
import { NewWikisHelp } from '../signalHelpContent';
import { useDashboardSignals } from '../DashboardSignalsContext';

const MAX_ITEMS = 5;

export function NewWikisWidget() {
  const dashboardSignals = useDashboardSignals();
  const suggestedArticles = useWikiStore(s => s.suggestedArticles);
  const dismissSuggestedArticle = useWikiStore(s => s.dismissSuggestedArticle);
  const fetchAllArticles = useWikiStore(s => s.fetchAllArticles);
  const fetchSuggestedArticles = useWikiStore(s => s.fetchSuggestedArticles);
  const openArticle = useWikiStore(s => s.openArticle);
  const [generatingSignalId, setGeneratingSignalId] = useState<string | null>(null);

  const signalItems = dashboardSignals
    ? dashboardSignals.getProviderSignals<WikiCandidateEvidence>('wiki_candidate').slice(0, MAX_ITEMS)
    : null;
  const items = signalItems ?? suggestedArticles.slice(0, MAX_ITEMS);

  const itemKey = (item: KnowledgeSignal<WikiCandidateEvidence> | typeof suggestedArticles[number]) => {
    return 'id' in item ? item.id : item.tag_id;
  };

  const itemSignalId = (item: KnowledgeSignal<WikiCandidateEvidence> | typeof suggestedArticles[number]) => {
    return 'id' in item ? item.id : item.signal_id;
  };

  const itemTagId = (item: KnowledgeSignal<WikiCandidateEvidence> | typeof suggestedArticles[number]) => {
    return 'id' in item ? (item.evidence?.tag_id ?? item.target.id) : item.tag_id;
  };

  const itemTagName = (item: KnowledgeSignal<WikiCandidateEvidence> | typeof suggestedArticles[number]) => {
    return 'id' in item ? (item.evidence?.tag_name ?? item.target.label) : item.tag_name;
  };

  const itemReasons = (item: KnowledgeSignal<WikiCandidateEvidence> | typeof suggestedArticles[number]) => {
    if ('id' in item) {
      return item.reasons.slice(0, 2).map(reason => reason.label);
    }
    return (item.reasons ?? [`${item.atom_count} atoms`]).slice(0, 2);
  };

  const generateWiki = async (signalId: string | undefined, tagId: string, tagName: string) => {
    if (!signalId || generatingSignalId) return;
    setGeneratingSignalId(signalId);
    try {
      await getTransport().invoke<KnowledgeSignalActionResult>('apply_knowledge_signal_action', {
        signalKey: signalId,
        action: 'generate_wiki',
      });
      if (dashboardSignals) {
        dashboardSignals.removeSignal(signalId);
        await fetchAllArticles({ refreshSuggestions: false });
      } else {
        await Promise.all([fetchAllArticles(), fetchSuggestedArticles()]);
      }
      openArticle(tagId, tagName);
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey: signalId } }));
      toast.success('Wiki generated', { description: tagName });
    } catch (err) {
      console.error('Failed to generate wiki from suggestion:', err);
      toast.error('Failed to generate wiki', { description: String(err) });
    } finally {
      setGeneratingSignalId(null);
    }
  };

  const dismissSignal = async (signalId: string | undefined) => {
    if (!signalId) return;
    if (!dashboardSignals) {
      await dismissSuggestedArticle(signalId);
      return;
    }
    dashboardSignals.removeSignal(signalId);
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey: signalId });
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey: signalId } }));
    } catch (err) {
      console.error('Failed to dismiss wiki suggestion:', err);
      void dashboardSignals.refreshProvider('wiki_candidate', MAX_ITEMS);
      toast.error('Failed to dismiss suggestion', { description: String(err) });
    }
  };

  return (
    <Section label="Ready to generate" action={<NewWikisHelp />}>
      {dashboardSignals?.isLoading ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          Loading wiki suggestions...
        </div>
      ) : dashboardSignals?.error ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          Could not load wiki suggestions.
        </div>
      ) : items.length === 0 ? (
        <div className="py-6 text-sm text-[var(--color-text-tertiary)]">
          No wiki suggestions yet. Tag more atoms to build up candidates.
        </div>
      ) : (
        <ul className="-mx-2">
          {items.map(s => {
            const signalId = itemSignalId(s);
            const tagId = itemTagId(s);
            const tagName = itemTagName(s);
            const isGenerating = generatingSignalId === signalId;
            return (
              <li key={itemKey(s)} className="group flex items-start gap-2 px-2 py-1.5 rounded hover:bg-[var(--color-bg-hover)]/60">
                <div className="min-w-0 flex-1">
                  <span className="flex items-baseline gap-3">
                    <span className="flex-1 min-w-0 truncate text-sm text-[var(--color-text-secondary)]">
                      {tagName}
                    </span>
                  </span>
                  <span className="mt-0.5 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                    {itemReasons(s).join(' / ')}
                  </span>
                </div>
                <button
                  onClick={() => generateWiki(signalId, tagId, tagName)}
                  disabled={!signalId || generatingSignalId !== null}
                  title={isGenerating ? 'Generating wiki' : 'Generate wiki'}
                  className="mt-0.5 rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)] disabled:cursor-wait disabled:opacity-60"
                >
                  {isGenerating ? <Loader2 className="w-3.5 h-3.5 animate-spin" strokeWidth={2} /> : <BookOpen className="w-3.5 h-3.5" strokeWidth={2} />}
                </button>
                {signalId && (
                <button
                  onClick={() => dismissSignal(signalId)}
                  title="Dismiss suggestion"
                  className="mt-0.5 rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
                >
                  <X className="w-3.5 h-3.5" strokeWidth={2} />
                </button>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </Section>
  );
}
