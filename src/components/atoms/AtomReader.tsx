import { lazy, Suspense, useState, useEffect, useCallback, useMemo, useRef } from 'react';
import { ChevronDown, Trash2 } from 'lucide-react';
import { openExternalUrl } from '../../lib/platform';
import { Modal } from '../ui/Modal';
import { Input } from '../ui/Input';
import { TagChip } from '../tags/TagChip';
import { TagSelector } from '../tags/TagSelector';
import { MiniGraphPreview } from '../canvas/MiniGraphPreview';
import { useAtomsStore, type AtomWithTags, type SemanticSearchResult, type SimilarAtomResult } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { useUIStore } from '../../stores/ui';
import { useInlineEditor } from '../../hooks';
import { formatDate } from '../../lib/date';
import { getTransport } from '../../lib/transport';
import { readerEditorActions } from '../../lib/reader-editor-bridge';
import { atomLinkExtension, type AtomLinkSuggestion, type AtomLinkSuggestionSource } from '../../editor/atom-links';
import type {
  AtomicCodeMirrorEditorHandle,
  AtomicCodeMirrorEditorProps,
} from '@atomic-editor/editor';

// Lazy-load the editor module AND the curated code-languages
// registry together. Pinning both inside the same dynamic boundary
// keeps them in one lazy chunk, and wrapping the base component
// lets us pass the default `codeLanguages` without every call site
// having to know about the sub-path import.
const AtomicCodeMirrorEditor = lazy(async () => {
  const [mod, langs] = await Promise.all([
    import('@atomic-editor/editor'),
    import('@atomic-editor/editor/code-languages'),
  ]);
  const Base = mod.AtomicCodeMirrorEditor;
  const DEFAULT_LANGUAGES = langs.ATOMIC_CODE_LANGUAGES;
  const Wrapped = (props: AtomicCodeMirrorEditorProps) => (
    <Base
      {...props}
      codeLanguages={props.codeLanguages ?? DEFAULT_LANGUAGES}
    />
  );
  return { default: Wrapped };
});

interface AtomReaderProps {
  atomId: string;
  highlightText?: string | null;
}

export function AtomReader({ atomId, highlightText }: AtomReaderProps) {
  const deleteAtom = useAtomsStore(s => s.deleteAtom);
  const fetchTags = useTagsStore(s => s.fetchTags);
  const setSelectedTag = useUIStore(s => s.setSelectedTag);
  const overlayNavigate = useUIStore(s => s.overlayNavigate);
  const overlayDismiss = useUIStore(s => s.overlayDismiss);
  const removeAtomFromTabs = useUIStore(s => s.removeAtomFromTabs);
  const redirectAtomTabToFinding = useUIStore(s => s.redirectAtomTabToFinding);

  const [atom, setAtom] = useState<AtomWithTags | null>(null);
  const [isLoadingAtom, setIsLoadingAtom] = useState(true);
  const [showLoading, setShowLoading] = useState(false);
  const lastFetchedAt = useRef<string | null>(null);

  const refreshAtom = useCallback(async () => {
    const fetchedAtom = await getTransport().invoke<AtomWithTags | null>('get_atom_by_id', { id: atomId });
    setAtom(fetchedAtom);
    lastFetchedAt.current = fetchedAtom?.updated_at ?? null;
  }, [atomId]);


  // Watch the atoms store for updates to the currently viewed atom
  const storeAtom = useAtomsStore((s) =>
    s.atoms.find((a) => a.id === atomId)
  );

  // Fetch atom from database
  useEffect(() => {
    setIsLoadingAtom(true);
    setShowLoading(false);

    // Only show loading indicator if fetch takes longer than 200ms
    const loadingTimer = setTimeout(() => setShowLoading(true), 200);

    refreshAtom()
      .then(() => {
        clearTimeout(loadingTimer);
        setIsLoadingAtom(false);
      })
      .catch((error) => {
        clearTimeout(loadingTimer);
        console.error('Failed to fetch atom:', error);
        setAtom(null);
        setIsLoadingAtom(false);
        // atom loaded
      });

    return () => clearTimeout(loadingTimer);
  }, [atomId, refreshAtom]);

  // Re-fetch when store summary changes (e.g., after tag extraction)
  const storeAtomUpdatedAt = storeAtom?.updated_at;
  useEffect(() => {
    if (storeAtomUpdatedAt && !isLoadingAtom && storeAtomUpdatedAt !== lastFetchedAt.current) {
      lastFetchedAt.current = storeAtomUpdatedAt;
      refreshAtom().catch(console.error);
    }
  }, [storeAtomUpdatedAt, isLoadingAtom, refreshAtom]);

  // Refresh the open reader immediately when tagging completes for this atom.
  // The list store gets its status update from the global event hook, but the
  // reader owns full atom details and needs its own refresh to pick up new tags.
  useEffect(() => {
    const transport = getTransport();
    return transport.subscribe<{ atom_id: string }>('tagging-complete', (payload) => {
      if (payload.atom_id !== atomId) return;
      refreshAtom().catch(console.error);
    });
  }, [atomId, refreshAtom]);

  // If the fetched atom turns out to be a report finding (`kind = 'report'`),
  // redirect to the specialized FindingReader view. The generic atom reader
  // can't render `[N]` citation popovers, and findings are conceptually
  // read-only output not the user's own captures. This covers any path that
  // reached us via `/atoms/:id` for a finding atom — semantic search hits,
  // stale links from before the reports view existed, etc. The redirect
  // morphs the active tab in place + URL-replaces, so neither the tab
  // strip nor the browser back stack accumulates a dead /atoms/:id entry.
  useEffect(() => {
    if (atom?.kind === 'report') {
      redirectAtomTabToFinding(atomId);
    }
  }, [atom, atomId, redirectAtomTabToFinding]);

  return (
    <div className="h-full bg-[var(--color-bg-main)]">
      {isLoadingAtom ? (
        showLoading ? (
          <div className="flex items-center justify-center h-full text-[var(--color-text-secondary)]">
            Loading...
          </div>
        ) : null
      ) : !atom ? (
        <div className="flex items-center justify-center h-full text-[var(--color-text-secondary)]">
          Atom not found
        </div>
      ) : (
        <AtomReaderContent
          atom={atom}
          highlightText={highlightText}
          onDismiss={overlayDismiss}
          onDelete={async () => {
            await deleteAtom(atomId);
            await fetchTags();
            removeAtomFromTabs(atomId);
          }}
          onTagClick={(tagId) => { setSelectedTag(tagId); overlayDismiss(); }}
          onRelatedAtomClick={(id, opts) => overlayNavigate({ type: 'reader', atomId: id }, opts)}
          onViewGraph={(opts) => overlayNavigate({ type: 'graph', atomId }, opts)}
          onAtomUpdated={(updated) => setAtom(updated)}
        />
      )}
    </div>
  );
}

interface AtomReaderContentProps {
  atom: AtomWithTags;
  highlightText?: string | null;
  onDismiss: () => void;
  onDelete: () => Promise<void>;
  onTagClick: (tagId: string) => void;
  onRelatedAtomClick: (atomId: string, opts?: { newTab?: boolean }) => void;
  onViewGraph: (opts?: { newTab?: boolean }) => void;
  onAtomUpdated?: (atom: AtomWithTags) => void;
}

function AtomReaderContent({
  atom, highlightText,
  onDismiss, onDelete, onTagClick, onRelatedAtomClick, onViewGraph, onAtomUpdated,
}: AtomReaderContentProps) {
  const readerTheme = useUIStore(s => s.readerTheme);
  const isEditing = useUIStore(s => s.readerState.editing);
  const setReaderEditing = useUIStore(s => s.setReaderEditing);
  const setReaderSaveStatus = useUIStore(s => s.setReaderSaveStatus);
  const retryTagging = useAtomsStore(s => s.retryTagging);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const editorHandleRef = useRef<AtomicCodeMirrorEditorHandle | null>(null);
  const [showDeleteModal, setShowDeleteModal] = useState(false);
  const [isDeleting, setIsDeleting] = useState(false);
  const [showTagSelector, setShowTagSelector] = useState(false);

  const {
    editContent, editSourceUrl, editTags, saveStatus,
    editorRevision,
    startEditing, setEditContent, setEditSourceUrl, setEditTags, saveNow, flushDraft,
  } = useInlineEditor({ atom, onAtomUpdated });
  const isTaggingInFlight = atom.tagging_status === 'pending' || atom.tagging_status === 'processing';

  const handleAutoTag = useCallback(async () => {
    await retryTagging(atom.id);
    onAtomUpdated?.({ ...atom, tagging_status: 'pending' });
  }, [retryTagging, atom, onAtomUpdated]);

  useEffect(() => {
    setReaderSaveStatus(saveStatus);
  }, [saveStatus, setReaderSaveStatus]);

  useEffect(() => {
    startEditing();
  }, [startEditing]);

  // An empty atom has nothing to read — force edit mode so the surface is
  // usable and the empty-atom delete-on-close flow stays reachable.
  useEffect(() => {
    if (!atom.content.trim() && !useUIStore.getState().readerState.editing) {
      setReaderEditing(true);
    }
    // Mode is captured per document, not re-derived as content changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [atom.id, setReaderEditing]);

  // `flushDraft` changes identity on every draft edit; the mode-transition
  // effect below must not re-run (and steal focus) on keystrokes, so it
  // reads the latest through a ref.
  const flushDraftRef = useRef(flushDraft);
  useEffect(() => { flushDraftRef.current = flushDraft; }, [flushDraft]);

  // Mode transitions: entering edit focuses the editor; leaving edit
  // finalizes the draft (save + embedding/tagging pipeline) and returns
  // focus to the container so Escape and `i` keep working. On mount there
  // is no transition — the effect just sets focus for the opening mode.
  // A mode flip that coincides with navigation (`sameAtom` false) is not
  // an edit→read transition of the new document, so nothing is flushed —
  // the previous document's draft is handled by useInlineEditor.
  const prevEditingRef = useRef(isEditing);
  const prevAtomIdRef = useRef(atom.id);
  useEffect(() => {
    const wasEditing = prevEditingRef.current;
    const sameAtom = prevAtomIdRef.current === atom.id;
    prevEditingRef.current = isEditing;
    prevAtomIdRef.current = atom.id;
    if (isEditing) {
      const id = requestAnimationFrame(() => {
        editorHandleRef.current?.focus();
      });
      return () => cancelAnimationFrame(id);
    }
    if (wasEditing && sameAtom) {
      void flushDraftRef.current();
    }
    containerRef.current?.focus({ preventScroll: true });
  }, [isEditing, atom.id]);

  useEffect(() => {
    readerEditorActions.current = {
      startEditing: () => {
        editorHandleRef.current?.focus();
      },
      stopEditing: async () => {
        await flushDraft();
      },
      undo: () => editorHandleRef.current?.undo(),
      redo: () => editorHandleRef.current?.redo(),
      openSearch: (query?: string) => editorHandleRef.current?.openSearch(query),
      closeSearch: () => editorHandleRef.current?.closeSearch(),
    };
    return () => {
      readerEditorActions.current = null;
    };
  }, [flushDraft]);

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && editorHandleRef.current?.isSearchOpen()) {
        e.preventDefault();
        readerEditorActions.current?.closeSearch();
        return;
      }

      if ((e.ctrlKey || e.metaKey) && e.key === 's') {
        e.preventDefault();
        void saveNow();
        return;
      }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
        e.preventDefault();
        editorHandleRef.current?.openSearch();
        return;
      }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'e') {
        e.preventDefault();
        setReaderEditing(!isEditing);
        return;
      }
      // Vim-style: `i` enters edit mode from read mode (Escape steps back
      // out). Only when the keystroke isn't destined for a real input.
      if (
        e.key === 'i' && !isEditing && !showDeleteModal &&
        !e.metaKey && !e.ctrlKey && !e.altKey &&
        !isTypingTarget(e.target)
      ) {
        e.preventDefault();
        setReaderEditing(true);
        return;
      }
      if (e.key === 'Escape' && !showDeleteModal) {
        e.preventDefault();
        // Step out gradually: edit → read (finalizing the draft), then
        // read → close.
        if (isEditing) {
          setReaderEditing(false);
          return;
        }
        void (async () => {
          await flushDraft();
          onDismiss();
        })();
      }
    };
    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  }, [flushDraft, isEditing, onDismiss, saveNow, setReaderEditing, showDeleteModal]);

  const [revealed, setRevealed] = useState(false);
  useEffect(() => {
    const frame = requestAnimationFrame(() => setRevealed(true));
    return () => cancelAnimationFrame(frame);
  }, []);

  const handleDelete = async () => {
    setIsDeleting(true);
    try {
      await onDelete();
    } catch (error) {
      console.error('Failed to delete atom:', error);
    } finally {
      setIsDeleting(false);
      setShowDeleteModal(false);
    }
  };

  const suggestAtomLinks = useCallback(async (query: string): Promise<AtomLinkSuggestion[]> => {
    const trimmed = query.trim();
    const limit = 12;
    const transport = getTransport();

    const titleMatches = await transport.invoke<AtomLinkSuggestion[]>('get_atom_link_suggestions', {
      q: trimmed,
      limit,
    });
    const titleSuggestions = titleMatches
      .filter((suggestion) => suggestion.id !== atom.id)
      .map((suggestion) => ({
        ...suggestion,
        source: (suggestion.source ?? (trimmed ? 'title' : 'recent')) as AtomLinkSuggestionSource,
      }));

    if (!trimmed || titleSuggestions.length > 0) return titleSuggestions;

    const keywordMatches = await transport.invoke<SemanticSearchResult[]>('search_atoms_keyword', {
      query: trimmed,
      limit,
    });
    const keywordSuggestions = searchResultsToAtomLinkSuggestions(keywordMatches, atom.id, 'content');
    if (keywordSuggestions.length > 0) return keywordSuggestions;

    try {
      const hybridMatches = await transport.invoke<SemanticSearchResult[]>('search_atoms_hybrid', {
        query: trimmed,
        limit,
        threshold: 0.3,
      });
      return searchResultsToAtomLinkSuggestions(hybridMatches, atom.id, 'hybrid');
    } catch (error) {
      console.warn('Atom link hybrid fallback failed:', error);
      return [];
    }
  }, [atom.id]);

  const resolveAtomLink = useCallback(async (id: string) => {
    const linkedAtom = await getTransport().invoke<AtomWithTags | null>('get_atom_by_id', { id });
    if (!linkedAtom) return null;
    return {
      id: linkedAtom.id,
      title: linkedAtom.title,
      snippet: linkedAtom.snippet,
    };
  }, []);

  const atomLinkExtensions = useMemo(
    () => atomLinkExtension({
      currentAtomId: atom.id,
      suggestAtoms: suggestAtomLinks,
      resolveAtom: resolveAtomLink,
      openAtom: (id, opts) => onRelatedAtomClick(id, opts),
    }),
    [atom.id, onRelatedAtomClick, resolveAtomLink, suggestAtomLinks],
  );

  return (
    <div
      ref={containerRef}
      tabIndex={-1}
      data-reader-theme={readerTheme}
      className={`h-full flex flex-col bg-[var(--color-bg-main)] transition-opacity duration-300 ease-out focus:outline-none ${
        revealed ? 'opacity-100' : 'opacity-0'
      }`}
    >
      {/* @container makes the two-column layout react to the actual reader
          pane width rather than the viewport. With the chat sidebar open,
          the viewport may be wide while the reader is narrow — without
          container queries the desktop two-column would render at ~600px
          and squeeze the editor. */}
      <div className="@container flex-1 overflow-y-auto scrollbar-auto-hide">
        <div className="max-w-6xl mx-auto px-3 py-5 sm:px-4 sm:py-6 @4xl:px-6 @4xl:flex @4xl:gap-10">
          <div className="flex-1 min-w-0">
            <Suspense fallback={null}>
              <AtomicCodeMirrorEditor
                key={`${atom.id}:${editorRevision}`}
                documentId={atom.id}
                markdownSource={editContent}
                initialRevealText={highlightText}
                readOnly={!isEditing}
                blurEditorOnMount={!isEditing}
                onMarkdownChange={setEditContent}
                onLinkClick={(url) => {
                  void openExternalUrl(url);
                }}
                editorHandleRef={editorHandleRef}
                extensions={atomLinkExtensions}
              />
            </Suspense>
          </div>

          <div className="w-full @4xl:w-80 @4xl:shrink-0 mt-6 @4xl:mt-0 border border-[var(--color-border)] rounded-lg p-4 self-start">
            <div className="mb-4">
              {/* Source URL + delete share a row — delete sits to the right
                  of the input. */}
              <div className="flex items-center gap-1.5">
                <div className="flex-1 min-w-0">
                  <Input
                    value={editSourceUrl}
                    onChange={(e) => setEditSourceUrl(e.target.value)}
                    placeholder="Source URL (optional)"
                    className="text-xs"
                  />
                </div>
                <button
                  onClick={() => setShowDeleteModal(true)}
                  className="shrink-0 p-1.5 rounded text-[var(--color-text-secondary)] hover:text-red-400 hover:bg-[var(--color-bg-hover)] transition-colors"
                  title="Delete atom"
                  aria-label="Delete atom"
                >
                  <Trash2 className="w-3.5 h-3.5" strokeWidth={2} />
                </button>
              </div>
              {atom.source_url && (
                <button
                  type="button"
                  onClick={() => {
                    void openExternalUrl(atom.source_url!);
                  }}
                  className="mt-2 inline-block text-xs text-[var(--color-text-tertiary)] hover:text-[var(--color-accent)]"
                >
                  Open source
                </button>
              )}
            </div>

            <div className="mb-4">
              <div className="flex flex-wrap gap-1.5 mb-2">
                {editTags.map((tag) => (
                  <TagChip
                    key={tag.id}
                    name={tag.name}
                    size="sm"
                    onRemove={() => setEditTags(editTags.filter((t) => t.id !== tag.id))}
                    onClick={() => onTagClick(tag.id)}
                  />
                ))}
                <button
                  onClick={() => setShowTagSelector(!showTagSelector)}
                  className="text-xs text-[var(--color-accent)] hover:text-[var(--color-accent-light)] transition-colors px-1.5 py-0.5 rounded border border-dashed border-[var(--color-border)]"
                >
                  +
                </button>
              </div>
              {showTagSelector && (
                <TagSelector selectedTags={editTags} onTagsChange={setEditTags} />
              )}
            </div>

            {editTags.length === 0 && (
              <div className="mb-4 rounded-xl border border-dashed border-[var(--color-border)] bg-[var(--color-bg-card)]/60 p-3">
                <div className="flex items-center justify-between gap-3">
                  <div className="min-w-0">
                    <p className="text-sm text-[var(--color-text-primary)]">No tags yet</p>
                    <p className="mt-1 text-xs text-[var(--color-text-tertiary)]">
                      Run tagging for this atom manually.
                    </p>
                  </div>
                  <button
                    onClick={() => {
                      void handleAutoTag();
                    }}
                    disabled={isTaggingInFlight}
                    className="shrink-0 rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-elevated)] px-3 py-1.5 text-xs font-medium text-[var(--color-text-primary)] transition-colors hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-60"
                  >
                    {isTaggingInFlight ? 'Tagging...' : 'Auto-tag'}
                  </button>
                </div>
              </div>
            )}

            {/* Dates */}
            <div className="text-xs text-[var(--color-text-tertiary)] space-y-0.5">
              {atom.published_at && <p>{formatDate(atom.published_at)}</p>}
              <p>{formatDate(atom.updated_at)}</p>
            </div>

            {/* Neighborhood graph — always visible */}
            {atom.embedding_status !== 'failed' && (
              <div className="mt-4">
                <MiniGraphPreview atomId={atom.id} onExpand={onViewGraph} />
              </div>
            )}

            {/* Related atoms — collapsible */}
            {atom.embedding_status !== 'failed' && (
              <SidebarRelatedAtoms atomId={atom.id} onAtomClick={onRelatedAtomClick} />
            )}
          </div>
        </div>
      </div>

      {/* Delete Confirmation Modal */}
      <Modal
        isOpen={showDeleteModal}
        onClose={() => setShowDeleteModal(false)}
        title="Delete Atom"
        confirmLabel={isDeleting ? 'Deleting...' : 'Delete'}
        confirmVariant="danger"
        onConfirm={handleDelete}
      >
        <p>Are you sure you want to delete this atom? This action cannot be undone.</p>
      </Modal>
    </div>
  );
}

/// Whether a keystroke is destined for a real text input, so single-letter
/// shortcuts (`i`) must not hijack it. The read-mode editor body is not
/// contenteditable, so it never matches.
function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable;
}

function searchResultsToAtomLinkSuggestions(
  results: SemanticSearchResult[],
  currentAtomId: string,
  source: AtomLinkSuggestionSource,
): AtomLinkSuggestion[] {
  return results
    .filter((result) => result.id !== currentAtomId)
    .map((result) => ({
      id: result.id,
      title: result.title,
      snippet: result.matching_chunk_content || result.snippet,
      source,
    }));
}

function SidebarRelatedAtoms({ atomId, onAtomClick }: { atomId: string; onAtomClick: (id: string, opts?: { newTab?: boolean }) => void }) {
  const [relatedAtoms, setRelatedAtoms] = useState<SimilarAtomResult[]>([]);
  const [isCollapsed, setIsCollapsed] = useState(true);
  const [hasLoaded, setHasLoaded] = useState(false);
  const [isLoading, setIsLoading] = useState(false);

  // Reset when atomId changes so we re-fetch for the new atom
  useEffect(() => {
    setRelatedAtoms([]);
    setHasLoaded(false);
  }, [atomId]);

  useEffect(() => {
    if (!isCollapsed && !hasLoaded) {
      setIsLoading(true);
      getTransport().invoke<SimilarAtomResult[]>('find_similar_atoms', { atomId, limit: 5, threshold: 0.7 })
        .then((results) => { setRelatedAtoms(results); setHasLoaded(true); })
        .catch(console.error)
        .finally(() => setIsLoading(false));
    }
  }, [atomId, isCollapsed, hasLoaded]);

  return (
    <div className="mt-4">
      <button
        onClick={() => setIsCollapsed(!isCollapsed)}
        className="flex items-center justify-between w-full text-xs font-medium text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
      >
        <span>Related atoms</span>
        <ChevronDown className={`w-3 h-3 transition-transform ${isCollapsed ? '' : 'rotate-180'}`} strokeWidth={2} />
      </button>
      {!isCollapsed && (
        <div className="mt-2 space-y-1.5">
          {isLoading ? (
            <div className="text-xs text-[var(--color-text-tertiary)]">Loading...</div>
          ) : relatedAtoms.length > 0 ? (
            relatedAtoms.map((result) => (
              <button
                key={result.id}
                onClick={(e) => onAtomClick(result.id, { newTab: e.metaKey || e.ctrlKey })}
                onAuxClick={(e) => {
                  if (e.button === 1) {
                    e.preventDefault();
                    onAtomClick(result.id, { newTab: true });
                  }
                }}
                className="w-full text-left p-2 rounded-md hover:bg-[var(--color-bg-hover)] transition-colors"
              >
                <p className="text-xs text-[var(--color-text-primary)] line-clamp-2">
                  {result.title || 'Untitled'}
                </p>
                <span className="text-[10px] text-[var(--color-accent)]">
                  {Math.round(result.similarity_score * 100)}% similar
                </span>
              </button>
            ))
          ) : hasLoaded ? (
            <div className="text-xs text-[var(--color-text-tertiary)]">No similar atoms found</div>
          ) : null}
        </div>
      )}
    </div>
  );
}
