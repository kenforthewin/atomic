import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import type { CanvasLevel } from '../lib/api';
import { getCanvasLevel } from '../lib/api';

export type DrawerMode = 'editor' | 'viewer' | 'wiki' | 'chat';
export type ViewMode = 'grid' | 'list' | 'canvas' | 'settings';

interface DrawerState {
  isOpen: boolean;
  mode: DrawerMode;
  atomId: string | null;      // For editor/viewer modes
  tagId: string | null;       // For wiki and chat modes
  tagName: string | null;     // For wiki mode (display purposes)
  conversationId: string | null;  // For chat mode
  highlightText: string | null;   // For viewer mode (text to highlight and scroll to)
}

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

interface CanvasNavState {
  currentLevel: CanvasLevel | null;
  isLoading: boolean;
}

interface UIStore {
  selectedTagId: string | null;
  expandedTagIds: Record<string, boolean>;  // Tags that should be expanded in sidebar
  drawerState: DrawerState;
  viewMode: ViewMode;
  previousViewMode: ViewMode;
  searchQuery: string;
  loadingOperations: LoadingOperation[];
  // Left panel state
  leftPanelOpen: boolean;
  // Server connection state
  serverConnected: boolean;
  // Local graph state
  localGraph: LocalGraphState;
  highlightedAtomId: string | null;
  // Command palette state
  commandPaletteOpen: boolean;
  commandPaletteInitialQuery: string;
  // Canvas navigation state
  canvasNav: CanvasNavState;
  // Actions
  setServerConnected: (connected: boolean) => void;
  setLeftPanelOpen: (open: boolean) => void;
  toggleLeftPanel: () => void;
  setSelectedTag: (tagId: string | null) => void;
  expandTagPath: (tagIds: string[]) => void;  // Expand all tags in path
  toggleTagExpanded: (tagId: string) => void;
  openDrawer: (mode: DrawerMode, atomId?: string, highlightText?: string) => void;
  openWikiDrawer: (tagId: string, tagName: string) => void;
  openWikiListDrawer: () => void;
  openChatDrawer: (tagId?: string, conversationId?: string) => void;
  closeDrawer: () => void;
  setViewMode: (mode: ViewMode) => void;
  goBack: () => void;
  setSearchQuery: (query: string) => void;
  addLoadingOperation: (id: string, message: string) => void;
  removeLoadingOperation: (id: string) => void;
  // Local graph actions
  openLocalGraph: (atomId: string, depth?: 1 | 2) => void;
  navigateLocalGraph: (atomId: string) => void;
  goBackLocalGraph: () => void;
  closeLocalGraph: () => void;
  setLocalGraphDepth: (depth: 1 | 2) => void;
  setHighlightedAtom: (atomId: string | null) => void;
  // Command palette actions
  openCommandPalette: (initialQuery?: string) => void;
  closeCommandPalette: () => void;
  toggleCommandPalette: () => void;
  // Canvas navigation actions
  navigateCanvas: (parentId: string | null, childrenHint?: string[]) => Promise<void>;
}

export const useUIStore = create<UIStore>()(
  persist(
    (set) => ({
      selectedTagId: null,
      expandedTagIds: {} as Record<string, boolean>,
      drawerState: {
        isOpen: false,
        mode: 'viewer',
        atomId: null,
        tagId: null,
        tagName: null,
        conversationId: null,
        highlightText: null,
      },
      viewMode: 'grid',
      previousViewMode: 'grid',
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
      serverConnected: false,
      commandPaletteOpen: false,
      commandPaletteInitialQuery: '',
      canvasNav: {
        currentLevel: null,
        isLoading: false,
      },

      setLeftPanelOpen: (open: boolean) => set({ leftPanelOpen: open }),
      toggleLeftPanel: () => set((state) => ({ leftPanelOpen: !state.leftPanelOpen })),
      setServerConnected: (connected: boolean) => set({ serverConnected: connected }),

      setSelectedTag: (tagId: string | null) => set((state) => ({
        selectedTagId: tagId,
        // Close settings page when a tag is clicked
        ...(state.viewMode === 'settings' ? {
          viewMode: state.previousViewMode,
          ...((state as any)._leftPanelWasOpen ? { leftPanelOpen: true, _leftPanelWasOpen: false } : {}),
        } : {}),
      })),

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

      openDrawer: (mode: DrawerMode, atomId?: string, highlightText?: string) =>
        set({
          drawerState: {
            isOpen: true,
            mode,
            atomId: atomId || null,
            tagId: null,
            tagName: null,
            conversationId: null,
            highlightText: highlightText || null,
          },
        }),

      openWikiDrawer: (tagId: string, tagName: string) =>
        set({
          drawerState: {
            isOpen: true,
            mode: 'wiki',
            atomId: null,
            tagId,
            tagName,
            conversationId: null,
            highlightText: null,
          },
        }),

      openWikiListDrawer: () =>
        set({
          drawerState: {
            isOpen: true,
            mode: 'wiki',
            atomId: null,
            tagId: null,
            tagName: null,
            conversationId: null,
            highlightText: null,
          },
        }),

      openChatDrawer: (tagId?: string, conversationId?: string) =>
        set({
          drawerState: {
            isOpen: true,
            mode: 'chat',
            atomId: null,
            tagId: tagId || null,
            tagName: null,
            conversationId: conversationId || null,
            highlightText: null,
          },
        }),

      closeDrawer: () =>
        set((state) => ({
          drawerState: {
            ...state.drawerState,
            isOpen: false,
            highlightText: null,
          },
        })),

      setViewMode: (mode: ViewMode) => set((state) => {
        const wasSettings = state.viewMode === 'settings';
        const goingToSettings = mode === 'settings';
        return {
          previousViewMode: wasSettings ? state.previousViewMode : state.viewMode,
          viewMode: mode,
          // Auto-collapse left panel when entering settings
          ...(goingToSettings && state.leftPanelOpen ? { leftPanelOpen: false, _leftPanelWasOpen: true } : {}),
          // Restore left panel when leaving settings
          ...(!goingToSettings && wasSettings && (state as any)._leftPanelWasOpen ? { leftPanelOpen: true, _leftPanelWasOpen: false } : {}),
        };
      }),
      goBack: () => set((state) => ({
        viewMode: state.previousViewMode,
        // Restore left panel if it was open before settings
        ...((state as any)._leftPanelWasOpen ? { leftPanelOpen: true, _leftPanelWasOpen: false } : {}),
      })),

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

      // Local graph actions
      openLocalGraph: (atomId: string, depth: 1 | 2 = 1) =>
        set({
          localGraph: {
            isOpen: true,
            centerAtomId: atomId,
            depth,
            navigationHistory: [atomId],
          },
        }),

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
          history.pop(); // Remove current
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

      closeLocalGraph: () =>
        set({
          localGraph: {
            isOpen: false,
            centerAtomId: null,
            depth: 1,
            navigationHistory: [],
          },
        }),

      setLocalGraphDepth: (depth: 1 | 2) =>
        set((state) => ({
          localGraph: {
            ...state.localGraph,
            depth,
          },
        })),

      setHighlightedAtom: (atomId: string | null) =>
        set({ highlightedAtomId: atomId }),

      // Command palette actions
      openCommandPalette: (initialQuery?: string) => set({
        commandPaletteOpen: true,
        commandPaletteInitialQuery: initialQuery || ''
      }),
      closeCommandPalette: () => set({
        commandPaletteOpen: false,
        commandPaletteInitialQuery: ''
      }),
      toggleCommandPalette: () =>
        set((state) => ({
          commandPaletteOpen: !state.commandPaletteOpen,
          commandPaletteInitialQuery: state.commandPaletteOpen ? '' : state.commandPaletteInitialQuery
        })),

      navigateCanvas: async (parentId: string | null, childrenHint?: string[]) => {
        set({ canvasNav: { currentLevel: null, isLoading: true } });
        try {
          const level = await getCanvasLevel(parentId, childrenHint);
          set({ canvasNav: { currentLevel: level, isLoading: false } });
        } catch (err) {
          console.error('Failed to load canvas level:', err);
          set({ canvasNav: { currentLevel: null, isLoading: false } });
        }
      },
    }),
    {
      name: 'atomic-ui-storage',
      partialize: (state) => ({
        viewMode: state.viewMode === 'settings' ? state.previousViewMode : state.viewMode,
      }),
    }
  )
);

