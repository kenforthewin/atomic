import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import { navigateTo } from '../router/navigate-ref';
import { viewPath, atomReaderPath, wikiReaderPath, atomGraphPath, reportDetailPath } from '../router/routes';

export type ViewMode = 'dashboard' | 'atoms' | 'canvas' | 'wiki' | 'reports';
export type AtomsLayout = 'grid' | 'list';

interface LocalGraphState {
  isOpen: boolean;
  centerAtomId: string | null;
  depth: 1 | 2;
  navigationHistory: string[];  // For breadcrumb navigation
}

export interface LoadingOperation {
  id: string;
  message: string;
  timestamp: number;
}

interface ReaderState {
  atomId: string | null;
  highlightText: string | null;
  editing: boolean;
  saveStatus: 'idle' | 'saving' | 'saved' | 'error';
}

interface WikiReaderState {
  tagId: string | null;
  tagName: string | null;
  highlightText: string | null;
}

interface ReportsDetailState {
  /// The report whose detail view is currently active. `null` when no
  /// report tab is the active tab. Projected from the active 'report'
  /// TabEntry so existing component code can read a single field
  /// rather than walking the tabs array.
  reportId: string | null;
}

/// A single navigation entry within a tab's stack. Tabs hold the user's
/// per-context history through atoms, wiki articles, and graphs.
///
/// `title` is an optional label hint set at open time — the tab strip
/// shows it directly when available, falls back to looking up the atom in
/// the loaded atoms store, and finally to "Tab N". Storing it here keeps
/// pill labels stable across reloads even when the atoms list hasn't
/// hydrated yet.
export type TabEntry =
  | { type: 'atom'; atomId: string; tagId: string | null; highlightText: string | null; editing: boolean; title?: string }
  | { type: 'wiki'; tagId: string; tagName: string; highlightText: string | null }
  | { type: 'graph'; atomId: string; tagId: string | null; title?: string }
  | { type: 'report'; reportId: string; title?: string };

export interface Tab {
  id: string;
  stack: TabEntry[];
  stackIndex: number;
  /// Monotonically increasing ordinal assigned at creation. Used for the
  /// "Tab N" fallback label so untitled tabs stay distinguishable even after
  /// reordering or closures.
  ordinal: number;
}

/// Legacy shape preserved for any callers / tests that still import it.
/// Tabs are the new home; this is a one-field projection of an active tab.
export type OverlayNavEntry =
  | { type: 'reader'; atomId: string; highlightText?: string | null }
  | { type: 'graph'; atomId: string }
  | { type: 'wiki'; tagId: string; tagName: string; highlightText?: string | null };

export interface OverlayNav {
  stack: OverlayNavEntry[];
  index: number;
}

interface UIStore {
  selectedTagId: string | null;
  expandedTagIds: Record<string, boolean>;
  readerState: ReaderState;
  wikiReaderState: WikiReaderState;
  reportsDetailState: ReportsDetailState;
  // Tabs model — the source of truth for "what's open and where in its
  // stack the user is". `readerState`/`wikiReaderState`/`localGraph` are
  // kept as synced projections of the active tab's current entry so that
  // existing components don't need to know tabs exist.
  tabs: Tab[];
  activeTabId: string | null;
  nextTabOrdinal: number;
  viewMode: ViewMode;
  atomsLayout: AtomsLayout;
  searchQuery: string;
  loadingOperations: LoadingOperation[];
  // Panel state
  leftPanelOpen: boolean;
  wikiSidebarOpen: boolean;
  // Chat sidebar state
  chatSidebarOpen: boolean;
  chatSidebarWidth: number;
  chatSidebarConversationId: string | null;
  chatSidebarInitialTagId: string | null;
  chatSidebarInitialConversationId: string | null;
  // Server connection state
  serverConnected: boolean;
  // Local graph state (synced from active tab when entry.type === 'graph')
  localGraph: LocalGraphState;
  highlightedAtomId: string | null;
  // Command palette state
  commandPaletteOpen: boolean;
  commandPaletteInitialQuery: string;
  searchPaletteOpen: boolean;
  searchPaletteInitialQuery: string;
  // Reader theme
  readerTheme: 'light' | 'dark';
  // Actions
  setServerConnected: (connected: boolean) => void;
  setLeftPanelOpen: (open: boolean) => void;
  toggleLeftPanel: () => void;
  setWikiSidebarOpen: (open: boolean) => void;
  toggleWikiSidebar: () => void;
  setSelectedTag: (tagId: string | null) => void;
  expandTagPath: (tagIds: string[]) => void;
  toggleTagExpanded: (tagId: string) => void;
  // Tab actions
  openEntry: (entry: TabEntry, opts?: { newTab?: boolean }) => void;
  switchToTab: (tabId: string) => void;
  closeTab: (tabId: string) => void;
  reorderTabs: (fromIndex: number, toIndex: number) => void;
  tabBack: () => void;
  tabForward: () => void;
  deactivateTabs: () => void;
  removeAtomFromTabs: (atomId: string) => void;
  // Legacy public openers (delegate to openEntry)
  openReader: (atomId: string, highlightText?: string, opts?: { newTab?: boolean }) => void;
  openReaderEditing: (atomId: string, opts?: { newTab?: boolean }) => void;
  setReaderEditState: (editing: boolean, saveStatus?: 'idle' | 'saving' | 'saved' | 'error') => void;
  closeReader: () => void;
  openWikiReader: (tagId: string, tagName: string, highlightText?: string, opts?: { newTab?: boolean }) => void;
  openReportDetail: (reportId: string, opts?: { newTab?: boolean; title?: string }) => void;
  closeReportDetail: () => void;
  overlayNavigate: (entry: OverlayNavEntry, opts?: { newTab?: boolean }) => void;
  overlayBack: () => void;
  overlayForward: () => void;
  overlayDismiss: () => void;
  // Chat sidebar actions
  toggleChatSidebar: () => void;
  setChatSidebarOpen: (open: boolean) => void;
  setChatSidebarWidth: (width: number) => void;
  setChatSidebarConversationId: (id: string | null) => void;
  openChatSidebar: (tagId?: string, conversationId?: string) => void;
  clearChatSidebarInitial: () => void;
  setViewMode: (mode: ViewMode) => void;
  setAtomsLayout: (layout: AtomsLayout) => void;
  setSearchQuery: (query: string) => void;
  addLoadingOperation: (id: string, message: string) => void;
  removeLoadingOperation: (id: string) => void;
  // Local graph actions
  openLocalGraph: (atomId: string, depth?: 1 | 2, opts?: { newTab?: boolean }) => void;
  navigateLocalGraph: (atomId: string) => void;
  goBackLocalGraph: () => void;
  closeLocalGraph: () => void;
  setLocalGraphDepth: (depth: 1 | 2) => void;
  setHighlightedAtom: (atomId: string | null) => void;
  // Command palette actions
  openCommandPalette: (initialQuery?: string) => void;
  closeCommandPalette: () => void;
  toggleCommandPalette: () => void;
  openSearchPalette: (initialQuery?: string) => void;
  closeSearchPalette: () => void;
  toggleSearchPalette: () => void;
  setReaderTheme: (theme: 'light' | 'dark') => void;
  toggleReaderTheme: () => void;
}

/// Generate a tab id. Falls back to a monotonic counter for environments
/// without crypto.randomUUID (older mobile webviews).
let tabIdCounter = 0;
function generateTabId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  tabIdCounter += 1;
  return `tab-${Date.now()}-${tabIdCounter}`;
}

function entriesEquivalent(a: TabEntry, b: TabEntry): boolean {
  if (a.type !== b.type) return false;
  if (a.type === 'atom' && b.type === 'atom') return a.atomId === b.atomId;
  if (a.type === 'wiki' && b.type === 'wiki') return a.tagId === b.tagId;
  if (a.type === 'graph' && b.type === 'graph') return a.atomId === b.atomId;
  if (a.type === 'report' && b.type === 'report') return a.reportId === b.reportId;
  return false;
}

function entryUrl(entry: TabEntry): string {
  if (entry.type === 'atom') return atomReaderPath(entry.atomId, entry.tagId);
  if (entry.type === 'wiki') return wikiReaderPath(entry.tagId, entry.tagName);
  if (entry.type === 'report') return reportDetailPath(entry.reportId);
  return atomGraphPath(entry.atomId, entry.tagId);
}

/// Project an active tab entry into the legacy reader/wiki/graph slices so
/// existing components keep rendering against the same shapes.
function projectActiveEntry(entry: TabEntry | null): {
  readerState: ReaderState;
  wikiReaderState: WikiReaderState;
  reportsDetailState: ReportsDetailState;
  localGraphPatch: Partial<LocalGraphState>;
} {
  const emptyReader: ReaderState = { atomId: null, highlightText: null, editing: false, saveStatus: 'idle' };
  const emptyWiki: WikiReaderState = { tagId: null, tagName: null, highlightText: null };
  const emptyReport: ReportsDetailState = { reportId: null };

  if (!entry) {
    return {
      readerState: emptyReader,
      wikiReaderState: emptyWiki,
      reportsDetailState: emptyReport,
      localGraphPatch: { isOpen: false, centerAtomId: null, navigationHistory: [] },
    };
  }
  if (entry.type === 'atom') {
    return {
      readerState: {
        atomId: entry.atomId,
        highlightText: entry.highlightText,
        editing: entry.editing,
        saveStatus: 'idle',
      },
      wikiReaderState: emptyWiki,
      reportsDetailState: emptyReport,
      localGraphPatch: { isOpen: false },
    };
  }
  if (entry.type === 'wiki') {
    return {
      readerState: emptyReader,
      wikiReaderState: { tagId: entry.tagId, tagName: entry.tagName, highlightText: entry.highlightText },
      reportsDetailState: emptyReport,
      localGraphPatch: { isOpen: false },
    };
  }
  if (entry.type === 'report') {
    return {
      readerState: emptyReader,
      wikiReaderState: emptyWiki,
      reportsDetailState: { reportId: entry.reportId },
      localGraphPatch: { isOpen: false },
    };
  }
  return {
    readerState: emptyReader,
    wikiReaderState: emptyWiki,
    reportsDetailState: emptyReport,
    localGraphPatch: {
      isOpen: true,
      centerAtomId: entry.atomId,
      navigationHistory: [entry.atomId],
    },
  };
}

export const useUIStore = create<UIStore>()(
  persist(
    (set, get) => ({
      selectedTagId: null,
      expandedTagIds: {} as Record<string, boolean>,
      readerState: {
        atomId: null,
        highlightText: null,
        editing: false,
        saveStatus: 'idle' as const,
      },
      wikiReaderState: {
        tagId: null,
        tagName: null,
        highlightText: null,
      },
      reportsDetailState: {
        reportId: null,
      },
      tabs: [],
      activeTabId: null,
      nextTabOrdinal: 1,
      viewMode: 'atoms',
      atomsLayout: 'grid',
      searchQuery: '',
      loadingOperations: [],
      localGraph: {
        isOpen: false,
        centerAtomId: null,
        depth: 1,
        navigationHistory: [],
      },
      highlightedAtomId: null,
      leftPanelOpen: true,
      wikiSidebarOpen: true,
      chatSidebarOpen: false,
      chatSidebarWidth: 480,
      chatSidebarConversationId: null,
      chatSidebarInitialTagId: null,
      chatSidebarInitialConversationId: null,
      serverConnected: false,
      commandPaletteOpen: false,
      commandPaletteInitialQuery: '',
      searchPaletteOpen: false,
      searchPaletteInitialQuery: '',
      readerTheme: 'dark' as 'light' | 'dark',

      setLeftPanelOpen: (open: boolean) => set({ leftPanelOpen: open }),
      toggleLeftPanel: () => set((state) => ({ leftPanelOpen: !state.leftPanelOpen })),
      setWikiSidebarOpen: (open: boolean) => set({ wikiSidebarOpen: open }),
      toggleWikiSidebar: () => set((state) => ({ wikiSidebarOpen: !state.wikiSidebarOpen })),
      setServerConnected: (connected: boolean) => set({ serverConnected: connected }),

      setSelectedTag: (tagId: string | null) => {
        set({ selectedTagId: tagId });
        const state = get();
        const activeTab = state.activeTabId ? state.tabs.find((t) => t.id === state.activeTabId) : null;
        const activeEntry = activeTab?.stack[activeTab.stackIndex] ?? null;
        if (activeEntry?.type === 'atom') {
          navigateTo(atomReaderPath(activeEntry.atomId, tagId), { replace: true });
        } else if (activeEntry?.type === 'wiki') {
          // wiki reader URL is keyed on its own tagId; selectedTagId scope
          // doesn't apply here.
        } else if (activeEntry?.type === 'graph') {
          navigateTo(atomGraphPath(activeEntry.atomId, tagId), { replace: true });
        } else {
          navigateTo(viewPath(state.viewMode, tagId), { replace: true });
        }
      },

      expandTagPath: (tagIds: string[]) =>
        set((state) => {
          const updated = { ...state.expandedTagIds };
          for (const id of tagIds) {
            updated[id] = true;
          }
          return { expandedTagIds: updated };
        }),

      toggleTagExpanded: (tagId: string) =>
        set((state) => ({
          expandedTagIds: {
            ...state.expandedTagIds,
            [tagId]: !state.expandedTagIds[tagId],
          },
        })),

      // -- Tab actions ---------------------------------------------------

      /// Single entry point for opening any kind of routed entry. Three rules:
      ///   1. opts.newTab=true (cmd/ctrl+click) → always create a new tab.
      ///   2. there is an active tab → push onto its stack (truncating any
      ///      forward history past stackIndex).
      ///   3. no active tab → if some tab's *current* entry equivalent
      ///      already shows this entry, switch to it; otherwise new tab.
      openEntry: (entry, opts) => {
        const newTab = !!opts?.newTab;
        const state = get();

        // Cmd/ctrl+click: always new tab.
        if (newTab) {
          const id = generateTabId();
          const tab: Tab = { id, stack: [entry], stackIndex: 0, ordinal: state.nextTabOrdinal };
          set((s) => {
            const projected = projectActiveEntry(entry);
            return {
              tabs: [...s.tabs, tab],
              activeTabId: id,
              nextTabOrdinal: s.nextTabOrdinal + 1,
              ...projected,
              localGraph: { ...s.localGraph, ...projected.localGraphPatch },
            };
          });
          navigateTo(entryUrl(entry));
          return;
        }

        // Plain click while a tab is active → push onto its stack.
        if (state.activeTabId) {
          set((s) => {
            const tabs = s.tabs.map((t) => {
              if (t.id !== s.activeTabId) return t;
              const truncated = t.stack.slice(0, t.stackIndex + 1);
              const nextStack = [...truncated, entry];
              return { ...t, stack: nextStack, stackIndex: nextStack.length - 1 };
            });
            const projected = projectActiveEntry(entry);
            return {
              tabs,
              ...projected,
              localGraph: { ...s.localGraph, ...projected.localGraphPatch },
            };
          });
          navigateTo(entryUrl(entry));
          return;
        }

        // Plain click from a base view: switch if a tab already shows this entry.
        const existing = state.tabs.find((t) => {
          const cur = t.stack[t.stackIndex];
          return cur && entriesEquivalent(cur, entry);
        });
        if (existing) {
          set((s) => {
            const projected = projectActiveEntry(existing.stack[existing.stackIndex]);
            return {
              activeTabId: existing.id,
              ...projected,
              localGraph: { ...s.localGraph, ...projected.localGraphPatch },
            };
          });
          navigateTo(entryUrl(existing.stack[existing.stackIndex]));
          return;
        }

        // Otherwise: create a fresh tab.
        const id = generateTabId();
        const tab: Tab = { id, stack: [entry], stackIndex: 0, ordinal: state.nextTabOrdinal };
        set((s) => {
          const projected = projectActiveEntry(entry);
          return {
            tabs: [...s.tabs, tab],
            activeTabId: id,
            nextTabOrdinal: s.nextTabOrdinal + 1,
            ...projected,
            localGraph: { ...s.localGraph, ...projected.localGraphPatch },
          };
        });
        navigateTo(entryUrl(entry));
      },

      switchToTab: (tabId) => {
        const state = get();
        if (state.activeTabId === tabId) return;
        const tab = state.tabs.find((t) => t.id === tabId);
        if (!tab) return;
        const entry = tab.stack[tab.stackIndex];
        if (!entry) return;
        const projected = projectActiveEntry(entry);
        set((s) => ({
          activeTabId: tabId,
          ...projected,
          localGraph: { ...s.localGraph, ...projected.localGraphPatch },
        }));
        navigateTo(entryUrl(entry));
      },

      closeTab: (tabId) => {
        const state = get();
        const idx = state.tabs.findIndex((t) => t.id === tabId);
        if (idx === -1) return;
        const remaining = state.tabs.filter((t) => t.id !== tabId);
        const wasActive = state.activeTabId === tabId;

        if (!wasActive) {
          set({ tabs: remaining });
          return;
        }

        // Closing the active tab: focus a neighbor if available; otherwise
        // fall back to the base view for the current viewMode.
        const neighbor = remaining[idx] ?? remaining[idx - 1] ?? null;
        if (neighbor) {
          const entry = neighbor.stack[neighbor.stackIndex];
          const projected = projectActiveEntry(entry);
          set((s) => ({
            tabs: remaining,
            activeTabId: neighbor.id,
            ...projected,
            localGraph: { ...s.localGraph, ...projected.localGraphPatch },
          }));
          navigateTo(entryUrl(entry));
        } else {
          const projected = projectActiveEntry(null);
          set((s) => ({
            tabs: remaining,
            activeTabId: null,
            ...projected,
            localGraph: { ...s.localGraph, ...projected.localGraphPatch },
          }));
          navigateTo(viewPath(state.viewMode, state.selectedTagId));
        }
      },

      reorderTabs: (fromIndex, toIndex) => {
        if (fromIndex === toIndex) return;
        set((s) => {
          if (fromIndex < 0 || fromIndex >= s.tabs.length || toIndex < 0 || toIndex >= s.tabs.length) {
            return s;
          }
          const next = s.tabs.slice();
          const [moved] = next.splice(fromIndex, 1);
          next.splice(toIndex, 0, moved);
          return { tabs: next };
        });
      },

      tabBack: () => {
        const state = get();
        const tab = state.activeTabId ? state.tabs.find((t) => t.id === state.activeTabId) : null;
        if (!tab) return;
        const newIndex = tab.stackIndex - 1;
        if (newIndex < 0) return;
        const entry = tab.stack[newIndex];
        const projected = projectActiveEntry(entry);
        set((s) => ({
          tabs: s.tabs.map((t) => (t.id === tab.id ? { ...t, stackIndex: newIndex } : t)),
          ...projected,
          localGraph: { ...s.localGraph, ...projected.localGraphPatch },
        }));
        navigateTo(entryUrl(entry), { replace: true });
      },

      tabForward: () => {
        const state = get();
        const tab = state.activeTabId ? state.tabs.find((t) => t.id === state.activeTabId) : null;
        if (!tab) return;
        const newIndex = tab.stackIndex + 1;
        if (newIndex >= tab.stack.length) return;
        const entry = tab.stack[newIndex];
        const projected = projectActiveEntry(entry);
        set((s) => ({
          tabs: s.tabs.map((t) => (t.id === tab.id ? { ...t, stackIndex: newIndex } : t)),
          ...projected,
          localGraph: { ...s.localGraph, ...projected.localGraphPatch },
        }));
        navigateTo(entryUrl(entry), { replace: true });
      },

      deactivateTabs: () => {
        const projected = projectActiveEntry(null);
        set((s) => ({
          activeTabId: null,
          ...projected,
          localGraph: { ...s.localGraph, ...projected.localGraphPatch },
        }));
      },

      removeAtomFromTabs: (atomId) => {
        const state = get();
        // Drop any tab whose *current* entry is this atom (atom or graph).
        // Tabs whose stack merely *contains* this atom further back are kept;
        // user can still chevron-back through other entries even if the
        // forward history references a now-deleted atom (visiting that entry
        // would just show "Atom not found"). Could prune those too, but
        // surgical removal is friendlier.
        const isCurrentlyShowing = (tab: Tab) => {
          const e = tab.stack[tab.stackIndex];
          if (!e) return false;
          if (e.type === 'atom' && e.atomId === atomId) return true;
          if (e.type === 'graph' && e.atomId === atomId) return true;
          return false;
        };
        const toClose = state.tabs.filter(isCurrentlyShowing).map((t) => t.id);
        if (toClose.length === 0) return;

        // Closing one tab at a time so closeTab handles activation fallback.
        const wasActive = state.activeTabId && toClose.includes(state.activeTabId);
        const remaining = state.tabs.filter((t) => !toClose.includes(t.id));

        if (wasActive) {
          // Drop tab + fall back to atoms list (delete most likely happened
          // from an atom view; landing on the list is the natural place).
          const projected = projectActiveEntry(null);
          set((s) => ({
            tabs: remaining,
            activeTabId: null,
            viewMode: 'atoms',
            ...projected,
            localGraph: { ...s.localGraph, ...projected.localGraphPatch },
          }));
          navigateTo(viewPath('atoms', state.selectedTagId));
        } else {
          set({ tabs: remaining });
        }
      },

      // -- Legacy openers (delegate to openEntry) ------------------------

      openReader: (atomId, highlightText, opts) => {
        get().openEntry(
          {
            type: 'atom',
            atomId,
            tagId: get().selectedTagId,
            highlightText: highlightText ?? null,
            editing: false,
          },
          opts,
        );
      },

      openReaderEditing: (atomId, opts) => {
        get().openEntry(
          {
            type: 'atom',
            atomId,
            tagId: get().selectedTagId,
            highlightText: null,
            editing: true,
          },
          opts,
        );
      },

      setReaderEditState: (editing, saveStatus) =>
        set((state) => ({
          readerState: { ...state.readerState, editing, ...(saveStatus !== undefined ? { saveStatus } : {}) },
        })),

      closeReader: () => {
        const state = get();
        if (state.activeTabId) {
          state.closeTab(state.activeTabId);
        }
      },

      openWikiReader: (tagId, tagName, highlightText, opts) => {
        get().openEntry(
          {
            type: 'wiki',
            tagId,
            tagName,
            highlightText: highlightText ?? null,
          },
          opts,
        );
      },

      openReportDetail: (reportId, opts) => {
        get().openEntry(
          {
            type: 'report',
            reportId,
            title: opts?.title,
          },
          { newTab: opts?.newTab },
        );
      },

      closeReportDetail: () => {
        // Deactivate-without-close, mirroring `overlayDismiss`. Reports
        // are a small, persistent set per database — users routinely
        // bounce between the list and a specific report's detail view,
        // and `closeTab` semantics (which actually destroy the tab)
        // make every round-trip create a fresh tab with an incremented
        // ordinal. Keep the tab in the strip; the X button on the pill
        // is the explicit close affordance.
        const state = get();
        state.deactivateTabs();
        navigateTo(viewPath('reports', state.selectedTagId));
      },

      overlayNavigate: (entry, opts) => {
        if (entry.type === 'reader') {
          get().openEntry(
            {
              type: 'atom',
              atomId: entry.atomId,
              tagId: get().selectedTagId,
              highlightText: entry.highlightText ?? null,
              editing: false,
            },
            opts,
          );
        } else if (entry.type === 'wiki') {
          get().openEntry(
            {
              type: 'wiki',
              tagId: entry.tagId,
              tagName: entry.tagName,
              highlightText: entry.highlightText ?? null,
            },
            opts,
          );
        } else {
          get().openEntry(
            {
              type: 'graph',
              atomId: entry.atomId,
              tagId: get().selectedTagId,
            },
            opts,
          );
        }
      },

      overlayBack: () => get().tabBack(),
      overlayForward: () => get().tabForward(),
      // Legacy "dismiss the open overlay" — used by Escape in the reader,
      // tag clicks, etc. In the tabs world this means: leave the active
      // tab (it stays in the strip) and navigate the URL to the current
      // base view. Without the navigate, the URL gets out of sync with
      // the rendered chrome and a reload re-opens the dismissed tab.
      overlayDismiss: () => {
        const state = get();
        state.deactivateTabs();
        navigateTo(viewPath(state.viewMode, state.selectedTagId));
      },

      // -- Chat sidebar --------------------------------------------------

      toggleChatSidebar: () => set((state) => ({ chatSidebarOpen: !state.chatSidebarOpen })),
      setChatSidebarOpen: (open: boolean) => set({ chatSidebarOpen: open }),
      setChatSidebarWidth: (width: number) => set({ chatSidebarWidth: Math.min(Math.max(width, 320), 800) }),
      setChatSidebarConversationId: (id: string | null) => set({ chatSidebarConversationId: id }),
      openChatSidebar: (tagId?: string, conversationId?: string) =>
        set({
          chatSidebarOpen: true,
          chatSidebarInitialTagId: tagId || null,
          chatSidebarInitialConversationId: conversationId || null,
        }),
      clearChatSidebarInitial: () =>
        set({ chatSidebarInitialTagId: null, chatSidebarInitialConversationId: null }),

      setViewMode: (mode: ViewMode) => {
        // Clicking a main-nav button drops out of any active tab and shows
        // the base view. Tabs persist; the user can return by clicking a
        // pill.
        const state = get();
        const wasInTab = state.activeTabId !== null;
        const projected = wasInTab ? projectActiveEntry(null) : null;
        set((s) => ({
          viewMode: mode,
          ...(wasInTab
            ? {
                activeTabId: null,
                ...projected!,
                localGraph: { ...s.localGraph, ...projected!.localGraphPatch },
              }
            : {}),
        }));
        navigateTo(viewPath(mode, get().selectedTagId));
      },

      setAtomsLayout: (layout: AtomsLayout) => set({ atomsLayout: layout }),

      setSearchQuery: (query: string) => set({ searchQuery: query }),

      addLoadingOperation: (id: string, message: string) =>
        set((state) => ({
          loadingOperations: [
            ...state.loadingOperations,
            { id, message, timestamp: Date.now() },
          ],
        })),

      removeLoadingOperation: (id: string) =>
        set((state) => ({
          loadingOperations: state.loadingOperations.filter((op) => op.id !== id),
        })),

      // -- Local graph --------------------------------------------------

      openLocalGraph: (atomId, depth = 1, opts) => {
        // Update depth state-side; the graph entry itself doesn't carry depth
        // (the local-graph view reads it from store, same as before).
        set((s) => ({ localGraph: { ...s.localGraph, depth } }));
        get().openEntry(
          {
            type: 'graph',
            atomId,
            tagId: get().selectedTagId,
          },
          opts,
        );
      },

      navigateLocalGraph: (atomId: string) =>
        set((state) => ({
          localGraph: {
            ...state.localGraph,
            centerAtomId: atomId,
            navigationHistory: [...state.localGraph.navigationHistory, atomId],
          },
        })),

      goBackLocalGraph: () =>
        set((state) => {
          const history = [...state.localGraph.navigationHistory];
          history.pop();
          const previousAtomId = history[history.length - 1] || null;
          return {
            localGraph: {
              ...state.localGraph,
              centerAtomId: previousAtomId,
              navigationHistory: history,
              isOpen: history.length > 0,
            },
          };
        }),

      closeLocalGraph: () => {
        // closing the graph dismisses the active tab if it's a graph tab.
        const state = get();
        const tab = state.activeTabId ? state.tabs.find((t) => t.id === state.activeTabId) : null;
        const entry = tab?.stack[tab.stackIndex];
        if (entry?.type === 'graph') {
          state.closeTab(tab!.id);
        } else {
          set({
            localGraph: {
              isOpen: false,
              centerAtomId: null,
              depth: 1,
              navigationHistory: [],
            },
          });
        }
      },

      setLocalGraphDepth: (depth: 1 | 2) =>
        set((state) => ({
          localGraph: {
            ...state.localGraph,
            depth,
          },
        })),

      setHighlightedAtom: (atomId: string | null) =>
        set({ highlightedAtomId: atomId }),

      // -- Command palette ----------------------------------------------

      openCommandPalette: (initialQuery?: string) => set({
        commandPaletteOpen: true,
        commandPaletteInitialQuery: initialQuery || '',
        searchPaletteOpen: false,
        searchPaletteInitialQuery: '',
      }),
      closeCommandPalette: () => set({
        commandPaletteOpen: false,
        commandPaletteInitialQuery: '',
      }),
      toggleCommandPalette: () =>
        set((state) => ({
          commandPaletteOpen: !state.commandPaletteOpen,
          commandPaletteInitialQuery: state.commandPaletteOpen ? '' : state.commandPaletteInitialQuery,
          searchPaletteOpen: state.commandPaletteOpen ? state.searchPaletteOpen : false,
          searchPaletteInitialQuery: state.commandPaletteOpen ? state.searchPaletteInitialQuery : '',
        })),
      openSearchPalette: (initialQuery?: string) => set({
        searchPaletteOpen: true,
        searchPaletteInitialQuery: initialQuery || '',
        commandPaletteOpen: false,
        commandPaletteInitialQuery: '',
      }),
      closeSearchPalette: () => set({
        searchPaletteOpen: false,
        searchPaletteInitialQuery: '',
      }),
      toggleSearchPalette: () =>
        set((state) => ({
          searchPaletteOpen: !state.searchPaletteOpen,
          searchPaletteInitialQuery: state.searchPaletteOpen ? '' : state.searchPaletteInitialQuery,
          commandPaletteOpen: state.searchPaletteOpen ? state.commandPaletteOpen : false,
          commandPaletteInitialQuery: state.searchPaletteOpen ? state.commandPaletteInitialQuery : '',
        })),

      setReaderTheme: (theme: 'light' | 'dark') => set({ readerTheme: theme }),
      toggleReaderTheme: () => set((state) => ({ readerTheme: state.readerTheme === 'dark' ? 'light' : 'dark' })),
    }),
    {
      name: 'atomic-ui-storage',
      version: 2,
      partialize: (state) => ({
        viewMode: state.viewMode,
        atomsLayout: state.atomsLayout,
        readerTheme: state.readerTheme,
        chatSidebarOpen: state.chatSidebarOpen,
        chatSidebarWidth: state.chatSidebarWidth,
        chatSidebarConversationId: state.chatSidebarConversationId,
        leftPanelOpen: state.leftPanelOpen,
        tabs: state.tabs,
        activeTabId: state.activeTabId,
        nextTabOrdinal: state.nextTabOrdinal,
      }),
      // v0 → v1: 'grid' and 'list' were top-level ViewMode values. They're now
      // collapsed into a single 'atoms' view with a separate atomsLayout field.
      // v1 → v2: tabs introduced. No data migration needed — older sessions
      // without persisted tabs simply start with [].
      migrate: (persistedState: unknown, version: number) => {
        const state = (persistedState ?? {}) as Record<string, unknown>;
        if (version < 1) {
          if (state.viewMode === 'grid' || state.viewMode === 'list') {
            state.atomsLayout = state.viewMode;
            state.viewMode = 'atoms';
          }
        }
        if (version < 2) {
          state.tabs = [];
          state.activeTabId = null;
          state.nextTabOrdinal = 1;
        }
        return state;
      },
    }
  )
);
