import { useEffect, useRef } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { useUIStore, type Tab, type TabEntry } from '../stores/ui';
import { parseLocation, type ParsedRoute } from './routes';
import { setNavigateFn, type NavigateState } from './navigate-ref';

/// Glue between react-router-dom (URL) and Zustand (UI store).
///
/// Responsibilities:
///   1. Expose the live `navigate` function to non-React code via
///      `setNavigateFn` so store actions can write URLs.
///   2. Reconcile the store to the URL on every location change. URL is the
///      source of truth for what the user is *currently looking at*; the
///      tabs array is the source of truth for what tabs exist and where in
///      their stacks the user has been. Bridge keeps the two consistent:
///      activate/create tabs to match URL, navigate to the current tab's
///      entry on tab switches.
///
/// The seq counter in `history.state` is still used to disambiguate forward
/// vs back popstate. Without it, browser-back to a tab whose stackIndex was
/// already past zero would look indistinguishable from a fresh push.

function entriesEquivalent(a: TabEntry, parsed: ParsedRoute): boolean {
  if (parsed.kind === 'reader') return a.type === 'atom' && a.atomId === parsed.atomId;
  if (parsed.kind === 'graph') return a.type === 'graph' && a.atomId === parsed.atomId;
  if (parsed.kind === 'wiki-reader') return a.type === 'wiki' && a.tagId === parsed.tagId;
  if (parsed.kind === 'reports-detail') return a.type === 'report' && a.reportId === parsed.reportId;
  if (parsed.kind === 'finding-reader') return a.type === 'finding' && a.atomId === parsed.atomId;
  return false;
}

function entryFromRoute(parsed: ParsedRoute, fallbackEntry?: TabEntry): TabEntry | null {
  if (parsed.kind === 'reader') {
    const editing = fallbackEntry?.type === 'atom' && fallbackEntry.atomId === parsed.atomId
      ? fallbackEntry.editing
      : false;
    const highlightText = fallbackEntry?.type === 'atom' && fallbackEntry.atomId === parsed.atomId
      ? fallbackEntry.highlightText
      : null;
    return { type: 'atom', atomId: parsed.atomId, tagId: parsed.tagId, highlightText, editing };
  }
  if (parsed.kind === 'graph') {
    return { type: 'graph', atomId: parsed.atomId, tagId: parsed.tagId };
  }
  if (parsed.kind === 'wiki-reader') {
    const highlightText = fallbackEntry?.type === 'wiki' && fallbackEntry.tagId === parsed.tagId
      ? fallbackEntry.highlightText
      : null;
    return {
      type: 'wiki',
      tagId: parsed.tagId,
      tagName: parsed.tagName ?? '',
      highlightText,
    };
  }
  if (parsed.kind === 'reports-detail') {
    const title = fallbackEntry?.type === 'report' && fallbackEntry.reportId === parsed.reportId
      ? fallbackEntry.title
      : undefined;
    return { type: 'report', reportId: parsed.reportId, title };
  }
  if (parsed.kind === 'finding-reader') {
    const title = fallbackEntry?.type === 'finding' && fallbackEntry.atomId === parsed.atomId
      ? fallbackEntry.title
      : undefined;
    return { type: 'finding', atomId: parsed.atomId, title };
  }
  return null;
}

function projectActiveEntry(entry: TabEntry | null) {
  const emptyReader = { atomId: null, highlightText: null, editing: false, saveStatus: 'idle' as const };
  const emptyWiki = { tagId: null, tagName: null, highlightText: null };
  const emptyReport = { reportId: null };
  const emptyFinding = { atomId: null };

  if (!entry) {
    return {
      readerState: emptyReader,
      wikiReaderState: emptyWiki,
      reportsDetailState: emptyReport,
      findingReaderState: emptyFinding,
      localGraphPatch: { isOpen: false, centerAtomId: null, navigationHistory: [] as string[] },
    };
  }
  if (entry.type === 'atom') {
    return {
      readerState: {
        atomId: entry.atomId,
        highlightText: entry.highlightText,
        editing: entry.editing,
        saveStatus: 'idle' as const,
      },
      wikiReaderState: emptyWiki,
      reportsDetailState: emptyReport,
      findingReaderState: emptyFinding,
      localGraphPatch: { isOpen: false },
    };
  }
  if (entry.type === 'wiki') {
    return {
      readerState: emptyReader,
      wikiReaderState: { tagId: entry.tagId, tagName: entry.tagName, highlightText: entry.highlightText },
      reportsDetailState: emptyReport,
      findingReaderState: emptyFinding,
      localGraphPatch: { isOpen: false },
    };
  }
  if (entry.type === 'report') {
    return {
      readerState: emptyReader,
      wikiReaderState: emptyWiki,
      reportsDetailState: { reportId: entry.reportId },
      findingReaderState: emptyFinding,
      localGraphPatch: { isOpen: false },
    };
  }
  if (entry.type === 'finding') {
    return {
      readerState: emptyReader,
      wikiReaderState: emptyWiki,
      reportsDetailState: emptyReport,
      findingReaderState: { atomId: entry.atomId },
      localGraphPatch: { isOpen: false },
    };
  }
  return {
    readerState: emptyReader,
    wikiReaderState: emptyWiki,
    reportsDetailState: emptyReport,
    findingReaderState: emptyFinding,
    localGraphPatch: {
      isOpen: true,
      centerAtomId: entry.atomId,
      navigationHistory: [entry.atomId],
    },
  };
}

function tagIdFromParsed(parsed: ParsedRoute, fallback: string | null): string | null {
  if (parsed.kind === 'wiki-reader') return fallback;
  if (parsed.kind === 'view' || parsed.kind === 'reader' || parsed.kind === 'graph') {
    return parsed.tagId ?? fallback;
  }
  return fallback;
}

/// For a parsed overlay-kind URL, decide which tab should be active and
/// what its stack should look like. Strategy:
///   1. Active tab's current entry already matches → no-op.
///   2. Active tab has the entry at a different stackIndex (we walked the
///      browser history) → snap stackIndex to it.
///   3. Some other tab has the entry as its current → activate it.
///   4. Otherwise → push onto the active tab if there is one (treating it
///      as in-tab navigation), else create a new tab.
function reconcileTabsForOverlay(
  tabs: Tab[],
  activeTabId: string | null,
  parsed: ParsedRoute,
  direction: 'forward' | 'back' | 'unknown',
): { tabs: Tab[]; activeTabId: string; entry: TabEntry; nextOrdinal?: number; createdTab: boolean } | null {
  const newEntry = entryFromRoute(parsed);
  if (!newEntry) return null;

  const activeTab = activeTabId ? tabs.find((t) => t.id === activeTabId) ?? null : null;
  const activeCurrent = activeTab?.stack[activeTab.stackIndex];

  // Already showing this entry as current. Preserve highlightText/editing
  // bits from the existing entry rather than the route-fresh defaults.
  if (activeTab && activeCurrent && entriesEquivalent(activeCurrent, parsed)) {
    return { tabs, activeTabId: activeTab.id, entry: activeCurrent, createdTab: false };
  }

  // Walking the active tab's stack via browser history.
  if (activeTab && (direction === 'back' || direction === 'forward')) {
    const idx = activeTab.stack.findIndex((e) => entriesEquivalent(e, parsed));
    if (idx >= 0) {
      const updated = { ...activeTab, stackIndex: idx };
      const nextTabs = tabs.map((t) => (t.id === activeTab.id ? updated : t));
      return { tabs: nextTabs, activeTabId: activeTab.id, entry: updated.stack[idx], createdTab: false };
    }
  }

  // Some other tab has this entry as its current — switch to it.
  const matching = tabs.find((t) => {
    const cur = t.stack[t.stackIndex];
    return cur && entriesEquivalent(cur, parsed);
  });
  if (matching) {
    return {
      tabs,
      activeTabId: matching.id,
      entry: matching.stack[matching.stackIndex],
      createdTab: false,
    };
  }

  // Active tab present, fresh URL → push onto it.
  if (activeTab) {
    const truncated = activeTab.stack.slice(0, activeTab.stackIndex + 1);
    const entry = entryFromRoute(parsed, activeCurrent) ?? newEntry;
    const nextStack = [...truncated, entry];
    const updated = { ...activeTab, stack: nextStack, stackIndex: nextStack.length - 1 };
    const nextTabs = tabs.map((t) => (t.id === activeTab.id ? updated : t));
    return { tabs: nextTabs, activeTabId: activeTab.id, entry, createdTab: false };
  }

  // No active tab (cold deep-link, or coming from a base view) — create one.
  return null;
}

export function RouterBridge() {
  const location = useLocation();
  const navigate = useNavigate();
  const prevSeqRef = useRef<number | null>(null);
  const didInjectSeqRef = useRef(false);

  useEffect(() => {
    setNavigateFn(navigate);
  }, [navigate]);

  useEffect(() => {
    const incomingState = (location.state ?? null) as NavigateState | null;
    let seq = incomingState?.seq ?? null;
    if (seq === null && !didInjectSeqRef.current) {
      const existing = window.history.state ?? {};
      window.history.replaceState({ ...existing, seq: 0 }, '', window.location.href);
      seq = 0;
    }
    didInjectSeqRef.current = true;

    const prevSeq = prevSeqRef.current;
    const direction: 'forward' | 'back' | 'unknown' =
      prevSeq === null || seq === null
        ? 'unknown'
        : seq > prevSeq
        ? 'forward'
        : seq < prevSeq
        ? 'back'
        : 'unknown';
    prevSeqRef.current = seq;

    const parsed = parseLocation(location.pathname, location.search);
    const store = useUIStore.getState();

    if (parsed.kind === 'view') {
      // Base view: deactivate any tab. Tabs themselves persist.
      const projected = projectActiveEntry(null);
      const needsUpdate =
        store.activeTabId !== null ||
        store.viewMode !== parsed.viewMode ||
        store.selectedTagId !== parsed.tagId ||
        store.readerState.atomId !== null ||
        store.wikiReaderState.tagId !== null ||
        store.reportsDetailState.reportId !== null ||
        store.findingReaderState.atomId !== null ||
        store.localGraph.isOpen;
      if (!needsUpdate) return;
      useUIStore.setState((s) => ({
        viewMode: parsed.viewMode,
        selectedTagId: parsed.tagId,
        activeTabId: null,
        ...projected,
        localGraph: { ...s.localGraph, ...projected.localGraphPatch },
      }));
      return;
    }

    // Overlay-kind URL: figure out the right tab.
    const reconciled = reconcileTabsForOverlay(store.tabs, store.activeTabId, parsed, direction);

    if (reconciled) {
      const projected = projectActiveEntry(reconciled.entry);
      useUIStore.setState((s) => ({
        tabs: reconciled.tabs,
        activeTabId: reconciled.activeTabId,
        selectedTagId: tagIdFromParsed(parsed, s.selectedTagId),
        ...projected,
        localGraph: { ...s.localGraph, ...projected.localGraphPatch },
      }));
      return;
    }

    // No active tab and no match → create a fresh tab for this URL.
    const newEntry = entryFromRoute(parsed);
    if (!newEntry) return;
    const tabId = (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function')
      ? crypto.randomUUID()
      : `tab-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    const newTab: Tab = {
      id: tabId,
      stack: [newEntry],
      stackIndex: 0,
      ordinal: store.nextTabOrdinal,
    };
    const projected = projectActiveEntry(newEntry);
    useUIStore.setState((s) => ({
      tabs: [...s.tabs, newTab],
      activeTabId: tabId,
      nextTabOrdinal: s.nextTabOrdinal + 1,
      selectedTagId: tagIdFromParsed(parsed, s.selectedTagId),
      ...projected,
      localGraph: { ...s.localGraph, ...projected.localGraphPatch },
    }));
  }, [location.pathname, location.search, location.state]);

  return null;
}
