import { useEffect, useMemo, useState } from 'react';
import { BookOpen, ChevronLeft, ChevronRight, Lightbulb, Plus, RefreshCw, X } from 'lucide-react';
import { SigmaCanvas } from '../../canvas/SigmaCanvas';
import { CitationPopover } from '../../wiki/CitationPopover';
import { BriefingContent } from './BriefingContent';
import { CaptureOptions } from '../CaptureOptions';
import { useIsMobile } from '../../../hooks';
import { useAtomsStore } from '../../../stores/atoms';
import { useWikiStore } from '../../../stores/wiki';
import { useUIStore } from '../../../stores/ui';
import { useCanvasStore } from '../../../stores/canvas';
import {
  useFeaturedReportStore,
  type FindingCitation,
} from '../../../stores/featuredReport';
import { FeaturedDropdown } from '../../reports/FeaturedDropdown';
import { getTransport } from '../../../lib/transport';
import { formatRelativeDate } from '../../../lib/date';
import type { KnowledgeSignal } from '../../../types/knowledgeSignals';

type AttentionSignalEvidence = {
  tag_id?: string;
  tag_name?: string;
};

function greeting(date: Date): string {
  const h = date.getHours();
  if (h < 5) return 'Working late';
  if (h < 12) return 'Good morning';
  if (h < 18) return 'Good afternoon';
  return 'Good evening';
}

function withinHours(iso: string, hours: number): boolean {
  return Date.now() - new Date(iso).getTime() < hours * 60 * 60 * 1000;
}

function formatToday(date: Date): string {
  return date
    .toLocaleDateString(undefined, { weekday: 'long', month: 'long', day: 'numeric' })
    .toUpperCase();
}

/**
 * Dashboard widget showing the most recent finding from the database's
 * featured report. Phase 3 replaced the legacy briefing path with this
 * reports-driven flow; the visual contract is unchanged so the rewire is
 * invisible to users.
 *
 * Real-time refresh: subscribes to the standard `atom-created` event and
 * re-fetches when a kind=report atom for the active report lands. The
 * runNow path also schedules a fallback fetch in case the websocket missed.
 */
export function BriefingWidget() {
  const atoms = useAtomsStore(s => s.atoms);
  const createAtom = useAtomsStore(s => s.createAtom);
  const suggestedArticles = useWikiStore(s => s.suggestedArticles);
  const articles = useWikiStore(s => s.articles);
  const fetchAllArticles = useWikiStore(s => s.fetchAllArticles);
  const openReader = useUIStore(s => s.openReader);
  const openReaderEditing = useUIStore(s => s.openReaderEditing);
  const openWikiReader = useUIStore(s => s.openWikiReader);
  const setViewMode = useUIStore(s => s.setViewMode);
  const isMobile = useIsMobile();
  const [attentionSignals, setAttentionSignals] = useState<KnowledgeSignal<AttentionSignalEvidence>[]>([]);
  const [activeAttentionActionId, setActiveAttentionActionId] = useState<string | null>(null);

  const handleCreateAtom = async () => {
    try {
      const atom = await createAtom('');
      openReaderEditing(atom.id);
    } catch (err) {
      console.error('Failed to create atom:', err);
    }
  };

  const reportId = useFeaturedReportStore(s => s.reportId);
  const active = useFeaturedReportStore(s => s.active);
  const history = useFeaturedReportStore(s => s.history);
  const activeIndex = useFeaturedReportStore(s => s.activeIndex);
  const isLoading = useFeaturedReportStore(s => s.isLoading);
  const isRunning = useFeaturedReportStore(s => s.isRunning);
  const fetchLatest = useFeaturedReportStore(s => s.fetchLatest);
  const navigate = useFeaturedReportStore(s => s.navigate);
  const runNow = useFeaturedReportStore(s => s.runNow);

  // Load on mount and re-fetch whenever a new report finding atom lands.
  // We filter on `kind === 'report'` so a normal capture doesn't trigger a
  // pointless refresh. The event-normalizer delivers AtomWithTags as the
  // payload, and AtomWithTags uses `serde(flatten)` for its inner atom —
  // so `kind` lives at the top of the payload, not under `.atom`.
  useEffect(() => {
    fetchLatest();
    const unsubAtomCreated = getTransport().subscribe('atom-created', (payload) => {
      const kind = (payload as { kind?: string } | undefined)?.kind;
      if (kind === 'report') {
        fetchLatest();
      }
    });
    // Cross-window featured-pointer changes (this client toggling the
    // star, another client picking a different report, a delete that
    // cleared the pointer server-side) — refetch so the widget tracks
    // the new featured report without polling.
    const unsubFeatured = getTransport().subscribe('dashboard-featured-changed', () => {
      fetchLatest();
    });
    return () => {
      unsubAtomCreated();
      unsubFeatured();
    };
  }, [fetchLatest]);

  useEffect(() => {
    let cancelled = false;
    async function fetchAttentionSignals() {
      try {
        const signals = await getTransport().invoke<KnowledgeSignal<AttentionSignalEvidence>[]>(
          'list_knowledge_signals',
          { surface: 'briefing', limit: 4 }
        );
        if (!cancelled) setAttentionSignals(signals);
      } catch (err) {
        console.error('Failed to load briefing suggestions:', err);
      }
    }
    fetchAttentionSignals();
    const handleChanged = () => fetchAttentionSignals();
    window.addEventListener('knowledge-signals:changed', handleChanged);
    return () => {
      cancelled = true;
      window.removeEventListener('knowledge-signals:changed', handleChanged);
    };
  }, []);

  // The mini-canvas now renders only this briefing's referenced atoms (plus
  // their 1-hop neighbors), so deduplicate citation atom IDs into a stable
  // array we hand to SigmaCanvas.
  const briefingAtomIds = useMemo(() => {
    if (!active) return undefined;
    const seen = new Set<string>();
    const out: string[] = [];
    for (const c of active.citations) {
      if (seen.has(c.atom_id)) continue;
      seen.add(c.atom_id);
      out.push(c.atom_id);
    }
    return out;
  }, [active]);

  const handleMiniNodeClick = (atomId: string) => {
    const store = useCanvasStore.getState();
    store.setPendingCamera(null);
    store.setPendingFocusAtomId(atomId);
    setViewMode('canvas');
  };

  const [activeCitation, setActiveCitation] = useState<FindingCitation | null>(null);
  const [anchorRect, setAnchorRect] = useState<{ top: number; left: number; bottom: number; width: number } | null>(null);

  const handleCitationClick = (citation: FindingCitation, element: HTMLElement) => {
    useCanvasStore.getState().previewController?.focusAtom(citation.atom_id);
    const rect = element.getBoundingClientRect();
    setActiveCitation(citation);
    setAnchorRect({ top: rect.top, left: rect.left, bottom: rect.bottom, width: rect.width });
  };

  const closePopover = () => {
    setActiveCitation(null);
    setAnchorRect(null);
  };

  const handleAttentionAction = async (signal: KnowledgeSignal<AttentionSignalEvidence>) => {
    const action = signal.suggested_actions?.[0];
    if (signal.target.kind !== 'tag') return;
    const tagId = signal.evidence?.tag_id ?? signal.target.id;
    const tagName = signal.evidence?.tag_name ?? signal.target.label;
    if (action?.id === 'generate_wiki') {
      if (activeAttentionActionId) return;
      setActiveAttentionActionId(signal.id);
      try {
        await getTransport().invoke('apply_knowledge_signal_action', {
          signalKey: signal.id,
          action: 'generate_wiki',
        });
        await fetchAllArticles();
        openWikiReader(tagId, tagName);
        window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey: signal.id } }));
        const signals = await getTransport().invoke<KnowledgeSignal<AttentionSignalEvidence>[]>(
          'list_knowledge_signals',
          { surface: 'briefing', limit: 4 }
        );
        setAttentionSignals(signals);
      } catch (err) {
        console.error('Failed to generate wiki from briefing suggestion:', err);
      } finally {
        setActiveAttentionActionId(null);
      }
    } else {
      openWikiReader(tagId, tagName);
    }
  };

  const dismissAttentionSignal = async (signalKey: string) => {
    const previous = attentionSignals;
    setAttentionSignals(current => current.filter(signal => signal.id !== signalKey));
    try {
      await getTransport().invoke('dismiss_knowledge_signal', { signalKey });
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { signalKey } }));
    } catch (err) {
      console.error('Failed to dismiss briefing suggestion:', err);
      setAttentionSignals(previous);
    }
  };

  // ===== Fallback stub used when no finding exists yet =====

  const stats = useMemo(() => {
    const newAtoms24h = atoms.filter(a => withinHours(a.created_at, 24)).length;
    const newAtoms7d = atoms.filter(a => withinHours(a.created_at, 24 * 7)).length;
    return { newAtoms24h, newAtoms7d, wikiCount: articles.length };
  }, [atoms, articles]);

  const now = new Date();
  const hello = greeting(now);

  const chips: string[] = [
    `${stats.newAtoms24h} new today`,
    `${stats.newAtoms7d} this week`,
    `${stats.wikiCount} wiki${stats.wikiCount === 1 ? '' : 's'}`,
    `${suggestedArticles.length} suggested`,
  ];

  const hasFinding = active !== null;
  const canGoNewer = activeIndex > 0;
  const canGoOlder = activeIndex < history.length - 1;
  const eyebrowLabel = hasFinding
    ? `BRIEFING · ${formatRelativeDate(active!.finding.created_at).toUpperCase()}`
    : formatToday(now);

  // Run Now is gated on having a featured report; without one we still
  // show the empty-state CTA (capture an atom), not a non-functional button.
  const canRunNow = Boolean(reportId);

  return (
    <div className="pb-2">
      <div className="flex items-center gap-2 mb-3">
        {hasFinding && (
          <>
            <button
              onClick={() => navigate(1)}
              disabled={!canGoOlder || isLoading}
              title="Older briefing"
              className="text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
            >
              <ChevronLeft className="w-4 h-4" strokeWidth={2} />
            </button>
            <button
              onClick={() => navigate(-1)}
              disabled={!canGoNewer || isLoading}
              title="Newer briefing"
              className="text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
            >
              <ChevronRight className="w-4 h-4" strokeWidth={2} />
            </button>
          </>
        )}
        <FeaturedDropdown label={eyebrowLabel} />
        {canRunNow && (
          <button
            onClick={() => runNow()}
            disabled={isRunning}
            title="Regenerate briefing now"
            className="ml-1 text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors disabled:opacity-50 disabled:cursor-wait"
          >
            <RefreshCw className={`w-3 h-3 ${isRunning ? 'animate-spin' : ''}`} strokeWidth={2} />
          </button>
        )}
      </div>

      {!isMobile && hasFinding && (
        <div className="float-right ml-2 mb-2 w-96 aspect-[4/3]">
          <SigmaCanvas
            mode="preview"
            filterAtomIds={briefingAtomIds}
            onPreviewNodeClick={handleMiniNodeClick}
          />
        </div>
      )}

      <h1 className="text-3xl md:text-4xl font-semibold text-[var(--color-text-primary)] tracking-tight mb-4">
        {hello}.
      </h1>

      {isMobile && hasFinding && (
        <div className="my-4 w-full aspect-[16/10]">
          <SigmaCanvas
            mode="preview"
            filterAtomIds={briefingAtomIds}
            onPreviewNodeClick={handleMiniNodeClick}
          />
        </div>
      )}

      {hasFinding ? (
        <>
          <BriefingContent
            content={active!.atom.content}
            citations={active!.citations}
            onCitationClick={handleCitationClick}
          />
          {attentionSignals.length > 0 && (
            <div className="mt-5 border-t border-[var(--color-border)] pt-4">
              <div className="mb-2 flex items-center gap-2 text-[11px] font-medium uppercase tracking-[0.14em] text-[var(--color-text-tertiary)]">
                <Lightbulb className="h-3.5 w-3.5" strokeWidth={2} />
                Worth your attention
              </div>
              <div className="space-y-2">
                {attentionSignals.map(signal => {
                  const primaryAction = signal.suggested_actions?.[0];
                  const isGeneratingThisSignal = activeAttentionActionId === signal.id;
                  const actionDisabled = isGeneratingThisSignal || (primaryAction?.id === 'generate_wiki' && activeAttentionActionId !== null);
                  const actionLabel = primaryAction?.id === 'generate_wiki'
                    ? (isGeneratingThisSignal ? 'Generating...' : 'Generate wiki')
                    : (primaryAction?.label ?? 'Open');
                  return (
                    <div
                      key={signal.id}
                      className="group flex items-start justify-between gap-3 rounded-md border border-[var(--color-border)] bg-[var(--color-bg-secondary)]/45 px-3 py-2"
                    >
                      <div className="min-w-0 flex-1">
                        <span className="block truncate text-sm font-medium text-[var(--color-text-primary)]">
                          {signal.title}
                        </span>
                        {signal.reasons.length > 0 && (
                          <span className="mt-1 block truncate text-[11px] text-[var(--color-text-tertiary)]">
                            {signal.reasons.slice(0, 2).map(reason => reason.label).join(' / ')}
                          </span>
                        )}
                      </div>
                      <div className="flex shrink-0 items-center gap-1">
                        <button
                          onClick={() => handleAttentionAction(signal)}
                          disabled={actionDisabled}
                          title={actionLabel}
                          className="inline-flex h-7 items-center gap-1.5 rounded-md px-2 text-xs font-medium text-[var(--color-text-secondary)] transition-colors hover:bg-[var(--color-bg-tertiary)] hover:text-[var(--color-text-primary)] disabled:cursor-wait disabled:opacity-60"
                        >
                          <BookOpen className={`h-3.5 w-3.5 ${isGeneratingThisSignal ? 'animate-pulse' : ''}`} strokeWidth={2} />
                          <span>{actionLabel}</span>
                        </button>
                        <button
                          onClick={() => dismissAttentionSignal(signal.id)}
                          disabled={isGeneratingThisSignal}
                          title="Dismiss suggestion"
                          className="inline-flex h-7 w-7 items-center justify-center rounded-md text-[var(--color-text-tertiary)] opacity-70 transition-colors hover:bg-[var(--color-bg-tertiary)] hover:text-[var(--color-text-primary)] disabled:cursor-wait disabled:opacity-40"
                        >
                          <X className="h-3.5 w-3.5" strokeWidth={2} />
                        </button>
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
          )}
        </>
      ) : (
        <button
          onClick={handleCreateAtom}
          className="inline-flex items-center gap-2 px-4 py-2 rounded-md bg-[var(--color-accent)] text-white text-sm font-medium hover:bg-[var(--color-accent-hover)] transition-colors"
        >
          <Plus className="w-4 h-4" strokeWidth={2.5} />
          Capture another atom
        </button>
      )}

      {!hasFinding && (
        <div className="mt-5 text-[13px] text-[var(--color-text-tertiary)] tabular-nums">
          {chips.join('  ·  ')}
        </div>
      )}

      <div className="md:clear-right" />

      {!hasFinding && <CaptureOptions />}

      {activeCitation && anchorRect && (
        <CitationPopover
          citation={activeCitation}
          anchorRect={anchorRect}
          onClose={closePopover}
          onViewAtom={(atomId, highlightText) => {
            closePopover();
            openReader(atomId, highlightText);
          }}
        />
      )}
    </div>
  );
}
