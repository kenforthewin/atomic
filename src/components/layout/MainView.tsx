import { useMemo, useCallback, useEffect, useRef, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { AtomGrid } from '../atoms/AtomGrid';
import { AtomList } from '../atoms/AtomList';
import { FilterBar } from '../atoms/FilterBar';
import { SigmaCanvas } from '../canvas/SigmaCanvas';
import { SettingsPage } from '../settings/SettingsPage';
import { FAB } from '../ui/FAB';
import { useAtomsStore } from '../../stores/atoms';
import { useUIStore } from '../../stores/ui';

export function MainView() {
  const atoms = useAtomsStore(s => s.atoms);
  const totalCount = useAtomsStore(s => s.totalCount);
  const hasMore = useAtomsStore(s => s.hasMore);
  const isLoadingInitial = useAtomsStore(s => s.isLoadingInitial);
  const isLoadingMore = useAtomsStore(s => s.isLoadingMore);
  const fetchNextPage = useAtomsStore(s => s.fetchNextPage);
  const semanticSearchResults = useAtomsStore(s => s.semanticSearchResults);
  const semanticSearchQuery = useAtomsStore(s => s.semanticSearchQuery);
  const retryEmbedding = useAtomsStore(s => s.retryEmbedding);
  const retryTagging = useAtomsStore(s => s.retryTagging);
  const sourceFilter = useAtomsStore(s => s.sourceFilter);
  const sourceValue = useAtomsStore(s => s.sourceValue);
  const sortBy = useAtomsStore(s => s.sortBy);
  const sortOrder = useAtomsStore(s => s.sortOrder);
  const search = useAtomsStore(s => s.search);
  const clearSemanticSearch = useAtomsStore(s => s.clearSemanticSearch);

  const { viewMode, searchQuery } = useUIStore(
    useShallow(s => ({
      viewMode: s.viewMode,
      searchQuery: s.searchQuery,
    }))
  );
  const leftPanelOpen = useUIStore(s => s.leftPanelOpen);
  const toggleLeftPanel = useUIStore(s => s.toggleLeftPanel);
  const setViewMode = useUIStore(s => s.setViewMode);
  const openDrawer = useUIStore(s => s.openDrawer);
  const openChatDrawer = useUIStore(s => s.openChatDrawer);
  const openWikiListDrawer = useUIStore(s => s.openWikiListDrawer);
  const openCommandPalette = useUIStore(s => s.openCommandPalette);

  const [filterBarOpen, setFilterBarOpen] = useState(false);
  const hasActiveFilter = sourceFilter !== 'all' || !!sourceValue || sortBy !== 'updated' || sortOrder !== 'desc';

  // Debounced server-side search when searchQuery changes
  const searchTimerRef = useRef<ReturnType<typeof setTimeout>>();
  useEffect(() => {
    if (searchTimerRef.current) clearTimeout(searchTimerRef.current);

    const query = searchQuery.trim();
    if (!query) {
      // Clear search results when query is empty
      if (semanticSearchResults !== null) {
        clearSemanticSearch();
      }
      return;
    }

    // Debounce 300ms before triggering API search
    searchTimerRef.current = setTimeout(() => {
      search(query);
    }, 300);

    return () => {
      if (searchTimerRef.current) clearTimeout(searchTimerRef.current);
    };
  }, [searchQuery]);

  // Determine what to display
  const displayAtoms = useMemo(() => {
    // If semantic search is active, use those results
    if (semanticSearchResults !== null) {
      return semanticSearchResults;
    }
    return atoms;
  }, [atoms, semanticSearchResults]);

  // Check if we're showing semantic search results
  const isSemanticSearch = semanticSearchResults !== null;

  // Build lookup map for matching chunk content (avoids .find() per atom)
  const matchingChunkMap = useMemo(() => {
    if (!isSemanticSearch) return null;
    const map = new Map<string, string>();
    for (const r of semanticSearchResults) {
      if (r.matching_chunk_content) {
        map.set(r.id, r.matching_chunk_content);
      }
    }
    return map;
  }, [isSemanticSearch, semanticSearchResults]);

  const getMatchingChunkContent = useCallback((atomId: string): string | undefined => {
    return matchingChunkMap?.get(atomId);
  }, [matchingChunkMap]);

  const handleAtomClick = useCallback((atomId: string) => {
    // Pass highlight text based on search mode:
    // - Keyword: highlight the search query terms
    // - Semantic: highlight the matching chunk content
    // - Hybrid: highlight the search query (prioritize keywords over chunk)
    const isSearch = useAtomsStore.getState().semanticSearchResults !== null;
    if (!isSearch) {
      openDrawer('editor', atomId);
      return;
    }
    const mode = useAtomsStore.getState().searchMode;
    const query = useAtomsStore.getState().semanticSearchQuery;
    let highlightText: string | undefined;
    if (mode === 'keyword' || mode === 'hybrid') {
      highlightText = query;
    } else {
      highlightText = matchingChunkMap?.get(atomId);
    }
    openDrawer('viewer', atomId, highlightText);
  }, [openDrawer, matchingChunkMap]);

  const handleNewAtom = useCallback(() => {
    openDrawer('editor');
  }, [openDrawer]);

  const handleRetryEmbedding = useCallback(async (atomId: string) => {
    try {
      await retryEmbedding(atomId);
    } catch (error) {
      console.error('Failed to retry embedding:', error);
    }
  }, [retryEmbedding]);

  const handleRetryTagging = useCallback(async (atomId: string) => {
    try {
      await retryTagging(atomId);
    } catch (error) {
      console.error('Failed to retry tagging:', error);
    }
  }, [retryTagging]);

  const handleOpenChat = useCallback(() => {
    openChatDrawer();
  }, [openChatDrawer]);

  const handleOpenWiki = useCallback(() => {
    openWikiListDrawer();
  }, [openWikiListDrawer]);

  const handleOpenSearch = useCallback(() => {
    openCommandPalette('/');
  }, [openCommandPalette]);

  const handleLoadMore = useCallback(() => {
    if (!isSemanticSearch && hasMore) {
      fetchNextPage();
    }
  }, [isSemanticSearch, hasMore, fetchNextPage]);

  // Display count: totalCount from server when not searching, results length when searching
  const displayCount = isSemanticSearch ? displayAtoms.length : totalCount;

  // Settings is a full-page view that replaces the main content
  if (viewMode === 'settings') {
    return (
      <main className="flex-1 flex flex-col h-full bg-[var(--color-bg-main)] overflow-hidden">
        <SettingsPage />
      </main>
    );
  }

  return (
    <main className="flex-1 flex flex-col h-full bg-[var(--color-bg-main)] overflow-hidden">
      {/* Titlebar row - aligned with traffic lights */}
      <div className="h-[52px] flex items-center gap-3 px-4 flex-shrink-0">
        {/* Sidebar toggle — visible on small screens when panel is collapsed */}
        {!leftPanelOpen && (
          <button
            onClick={toggleLeftPanel}
            className="md:hidden p-1.5 rounded-md text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors"
            title="Show sidebar"
          >
            <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <rect x="3" y="3" width="18" height="18" rx="2" />
              <line x1="9" y1="3" x2="9" y2="21" />
            </svg>
          </button>
        )}

        {/* View Mode Toggle */}
        <div className="flex items-center bg-[var(--color-bg-card)] rounded-md border border-[var(--color-border)] shrink-0">
          <button
            onClick={() => setViewMode('grid')}
            className={`p-1.5 rounded-l-md transition-colors ${
              viewMode === 'grid'
                ? 'bg-[var(--color-accent)] text-white'
                : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]'
            }`}
            title="Grid view"
          >
            <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth={2}
                d="M4 6a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2H6a2 2 0 01-2-2V6zM14 6a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2h-2a2 2 0 01-2-2V6zM4 16a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2H6a2 2 0 01-2-2v-2zM14 16a2 2 0 012-2h2a2 2 0 012 2v2a2 2 0 01-2 2h-2a2 2 0 01-2-2v-2z"
              />
            </svg>
          </button>
          <button
            onClick={() => setViewMode('list')}
            className={`p-1.5 transition-colors ${
              viewMode === 'list'
                ? 'bg-[var(--color-accent)] text-white'
                : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]'
            }`}
            title="List view"
          >
            <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth={2}
                d="M4 6h16M4 12h16M4 18h16"
              />
            </svg>
          </button>
          <button
            onClick={() => setViewMode('canvas')}
            className={`p-1.5 rounded-r-md transition-colors ${
              viewMode === 'canvas'
                ? 'bg-[var(--color-accent)] text-white'
                : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]'
            }`}
            title="Canvas view"
          >
            <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24" strokeWidth={2}>
              <circle cx="6" cy="6" r="2" />
              <circle cx="18" cy="6" r="2" />
              <circle cx="6" cy="18" r="2" />
              <circle cx="18" cy="18" r="2" />
              <circle cx="12" cy="12" r="2" />
              <path strokeLinecap="round" d="M8 7l2.5 3.5M16 7l-2.5 3.5M8 17l2.5-3.5M16 17l-2.5-3.5" />
            </svg>
          </button>
        </div>

        {/* Search button */}
        <button
          onClick={handleOpenSearch}
          className="p-1.5 rounded-md text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors"
          title="Search atoms"
        >
          <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z" />
          </svg>
        </button>

        {/* Wiki button */}
        <button
          onClick={handleOpenWiki}
          className="p-1.5 rounded-md text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors"
          title="Open wiki articles"
        >
          <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 6.253v13m0-13C10.832 5.477 9.246 5 7.5 5S4.168 5.477 3 6.253v13C4.168 18.477 5.754 18 7.5 18s3.332.477 4.5 1.253m0-13C13.168 5.477 14.754 5 16.5 5c1.747 0 3.332.477 4.5 1.253v13C19.832 18.477 18.247 18 16.5 18c-1.746 0-3.332.477-4.5 1.253" />
          </svg>
        </button>

        {/* Chat button */}
        <button
          onClick={handleOpenChat}
          className="p-1.5 rounded-md text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors"
          title="Open conversations"
        >
          <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z" />
          </svg>
        </button>

        {/* Drag region - fills available space */}
        <div data-tauri-drag-region className="flex-1 h-full drag-region" />

        {/* Filter toggle + atom count — right-aligned, hide for canvas */}
        {viewMode !== 'canvas' && (
          <div className="flex items-center gap-2 shrink-0">
            <button
              onClick={() => setFilterBarOpen(!filterBarOpen)}
              className={`relative p-1.5 rounded-md transition-colors ${
                filterBarOpen || hasActiveFilter
                  ? 'text-[var(--color-accent-light)] hover:text-[var(--color-accent)]'
                  : 'text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)]'
              }`}
              title="Filter & sort"
            >
              <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M3 4a1 1 0 011-1h16a1 1 0 011 1v2.586a1 1 0 01-.293.707l-6.414 6.414a1 1 0 00-.293.707V17l-4 4v-6.586a1 1 0 00-.293-.707L3.293 7.293A1 1 0 013 6.586V4z" />
              </svg>
              {hasActiveFilter && !filterBarOpen && (
                <span className="absolute top-0.5 right-0.5 w-1.5 h-1.5 bg-[var(--color-accent)] rounded-full" />
              )}
            </button>
            <span className="text-sm text-[var(--color-text-secondary)]">
              {displayCount} atom{displayCount !== 1 ? 's' : ''}
            </span>
          </div>
        )}
      </div>

      {/* Search results header - only show for grid/list views */}
      {isSemanticSearch && viewMode !== 'canvas' && (
        <div className="px-4 py-2 text-sm text-[var(--color-text-secondary)] border-b border-[var(--color-border)]">
          {semanticSearchResults.length > 0 ? (
            <span>
              {semanticSearchResults.length} results for "{semanticSearchQuery}"
            </span>
          ) : (
            <span>No atoms match your search</span>
          )}
        </div>
      )}

      {/* Filter bar - visible for grid/list views when toggled open */}
      {!isSemanticSearch && viewMode !== 'canvas' && filterBarOpen && <FilterBar />}

      {/* Content */}
      <div className="flex-1 overflow-hidden">
        {viewMode === 'canvas' ? (
          <SigmaCanvas />
        ) : viewMode === 'grid' ? (
          <AtomGrid
            atoms={displayAtoms}
            onAtomClick={handleAtomClick}
            getMatchingChunkContent={isSemanticSearch ? getMatchingChunkContent : undefined}
            onRetryEmbedding={handleRetryEmbedding}
            onRetryTagging={handleRetryTagging}
            onLoadMore={handleLoadMore}
            isLoading={isLoadingInitial}
            isLoadingMore={isLoadingMore}
          />
        ) : (
          <AtomList
            atoms={displayAtoms}
            onAtomClick={handleAtomClick}
            getMatchingChunkContent={isSemanticSearch ? getMatchingChunkContent : undefined}
            onRetryEmbedding={handleRetryEmbedding}
            onRetryTagging={handleRetryTagging}
            onLoadMore={handleLoadMore}
            isLoading={isLoadingInitial}
            isLoadingMore={isLoadingMore}
          />
        )}
      </div>

      {/* FAB */}
      <FAB onClick={handleNewAtom} title="Create new atom" />
    </main>
  );
}
