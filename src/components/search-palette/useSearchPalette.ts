import { useState, useCallback, useEffect, useMemo, useRef } from 'react';
import { getTransport } from '../../lib/transport';
import { useTagsStore, TagWithCount } from '../../stores/tags';
import { useUIStore } from '../../stores/ui';
import { useAtomsStore } from '../../stores/atoms';
import { useChatStore } from '../../stores/chat';
import {
  GlobalChatSearchResult,
  GlobalSearchResponse,
  GlobalTagSearchResult,
  GlobalWikiSearchResult,
  MatchOffset,
  SemanticSearchResult,
} from '../command-palette/types';
import { markdownToPlainText } from './markdownToPlainText';

const SEARCH_DEBOUNCE_MS = 250;
const SECTION_LIMIT = 5;
const HYBRID_ATOM_LIMIT = 12;
const HYBRID_ATOM_THRESHOLD = 0.3;
/** Padding (in bytes/chars) to pull into a per-match snippet on each side. */
export const MATCH_SNIPPET_PAD = 40;
/** Padding around a match used as `initialRevealText` when opening the reader. */
const MATCH_REVEAL_PAD = 30;

type SearchPaletteMode = 'global' | 'tags' | 'atoms-hybrid';

type SearchPalettePrefix =
  | { token: '#'; label: 'tags' }
  | { token: '>'; label: 'atoms' }
  | null;

export type SearchPaletteItem =
  | { kind: 'atom'; result: SemanticSearchResult; expandable: boolean; expanded: boolean }
  | {
      kind: 'atom-match';
      atom: SemanticSearchResult;
      matchIndex: number;
      offset: MatchOffset;
    }
  /** Tail row shown under an expanded atom when the backend capped its
   *  offset list — selecting it opens the atom at its first match. */
  | { kind: 'atom-match-more'; atom: SemanticSearchResult; hiddenCount: number }
  | { kind: 'wiki'; result: GlobalWikiSearchResult; expandable: boolean; expanded: boolean }
  | {
      kind: 'wiki-match';
      wiki: GlobalWikiSearchResult;
      matchIndex: number;
      offset: MatchOffset;
    }
  | { kind: 'wiki-match-more'; wiki: GlobalWikiSearchResult; hiddenCount: number }
  | { kind: 'chat'; result: GlobalChatSearchResult }
  | { kind: 'tag'; result: GlobalTagSearchResult };

interface UseSearchPaletteOptions {
  isOpen: boolean;
  onClose: () => void;
  initialQuery?: string;
}

function flattenTags(tagList: TagWithCount[], result: TagWithCount[] = []): TagWithCount[] {
  for (const tag of tagList) {
    result.push(tag);
    if (tag.children?.length) {
      flattenTags(tag.children, result);
    }
  }
  return result;
}

function strongSubstringMatch(haystack: string, needle: string): boolean {
  if (needle.length < 2) {
    return haystack === needle;
  }
  return haystack
    .split(/[^a-z0-9]+/i)
    .filter(Boolean)
    .some((segment) => segment.includes(needle));
}

/** UTF-8 byte length of a single Unicode code point. */
function utf8ByteLength(codePoint: number): number {
  if (codePoint < 0x80) return 1;
  if (codePoint < 0x800) return 2;
  if (codePoint < 0x10000) return 3;
  return 4;
}

/**
 * Backend `match_offsets` are UTF-8 byte offsets into `atom.content`. JS
 * string slicing uses UTF-16 code units, so any multi-byte character (smart
 * quote, em dash, accented letter, emoji) before the match causes the bold
 * region to drift right. This helper walks the content once, converting each
 * requested byte offset to its corresponding UTF-16 index. Offsets past
 * end-of-content (shouldn't happen, but guarding) clamp to `content.length`.
 */
export function byteOffsetsToUtf16(content: string, offsets: MatchOffset[]): MatchOffset[] {
  if (offsets.length === 0) return offsets;
  const maxByte = offsets.reduce((m, o) => Math.max(m, o.end), 0);

  const targets = new Set<number>();
  for (const o of offsets) {
    targets.add(o.start);
    targets.add(o.end);
  }

  const byteToUtf16 = new Map<number, number>();
  byteToUtf16.set(0, 0);

  let byteCursor = 0;
  let utf16Cursor = 0;
  while (utf16Cursor < content.length && byteCursor < maxByte) {
    const codePoint = content.codePointAt(utf16Cursor)!;
    byteCursor += utf8ByteLength(codePoint);
    utf16Cursor += codePoint > 0xffff ? 2 : 1;
    if (targets.has(byteCursor)) {
      byteToUtf16.set(byteCursor, utf16Cursor);
    }
  }

  return offsets.map((o) => ({
    start: byteToUtf16.get(o.start) ?? Math.min(o.start, content.length),
    end: byteToUtf16.get(o.end) ?? Math.min(o.end, content.length),
  }));
}

export function useSearchPalette({ isOpen, onClose, initialQuery = '' }: UseSearchPaletteOptions) {
  const [query, setQuery] = useState('');
  const [selectedIndex, setSelectedIndex] = useState(0);
  const [expandedAtomIds, setExpandedAtomIds] = useState<Set<string>>(() => new Set());
  const [expandedWikiIds, setExpandedWikiIds] = useState<Set<string>>(() => new Set());
  const [globalResults, setGlobalResults] = useState<GlobalSearchResponse>({
    atoms: [],
    wiki: [],
    chats: [],
    tags: [],
  });
  const [hybridAtomResults, setHybridAtomResults] = useState<SemanticSearchResult[]>([]);
  const [isSearching, setIsSearching] = useState(false);

  const searchTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const searchAbortRef = useRef<AbortController | null>(null);
  const tags = useTagsStore((state) => state.tags);

  useEffect(() => {
    searchAbortRef.current?.abort();

    if (isOpen) {
      setQuery(initialQuery);
      setSelectedIndex(0);
      setExpandedAtomIds(new Set());
      setExpandedWikiIds(new Set());
      setGlobalResults({ atoms: [], wiki: [], chats: [], tags: [] });
      setHybridAtomResults([]);
      setIsSearching(false);
    }
  }, [isOpen, initialQuery]);

  const prefix: SearchPalettePrefix = query.startsWith('#')
    ? { token: '#', label: 'tags' }
    : query.startsWith('>')
      ? { token: '>', label: 'atoms' }
      : null;
  const mode: SearchPaletteMode = prefix?.token === '#'
    ? 'tags'
    : prefix?.token === '>'
      ? 'atoms-hybrid'
      : 'global';
  const searchQuery = prefix ? query.slice(prefix.token.length) : query;

  useEffect(() => {
    // Every query/mode change resets expansion — stale expanded state from a
    // previous query is never useful and would confuse the selection index.
    setExpandedAtomIds(new Set());
    setExpandedWikiIds(new Set());

    if (mode !== 'global' && mode !== 'atoms-hybrid') {
      setGlobalResults({ atoms: [], wiki: [], chats: [], tags: [] });
      setHybridAtomResults([]);
      setIsSearching(false);
      return;
    }

    if (searchTimeoutRef.current) {
      clearTimeout(searchTimeoutRef.current);
    }

    const trimmed = searchQuery.trim();
    if (trimmed.length < 2) {
      setGlobalResults({ atoms: [], wiki: [], chats: [], tags: [] });
      setHybridAtomResults([]);
      setIsSearching(false);
      return;
    }

    setIsSearching(true);
    const controller = new AbortController();
    searchAbortRef.current = controller;
    searchTimeoutRef.current = setTimeout(async () => {
      try {
        if (mode === 'atoms-hybrid') {
          const results = await getTransport().invoke<SemanticSearchResult[]>('search_atoms_hybrid', {
            query: trimmed,
            limit: HYBRID_ATOM_LIMIT,
            threshold: HYBRID_ATOM_THRESHOLD,
          }, { signal: controller.signal });
          setHybridAtomResults(results);
          setGlobalResults({ atoms: [], wiki: [], chats: [], tags: [] });
        } else {
          const results = await getTransport().invoke<GlobalSearchResponse>('search_global_keyword', {
            query: trimmed,
            sectionLimit: SECTION_LIMIT,
          }, { signal: controller.signal });
          setGlobalResults(results);
          setHybridAtomResults([]);
        }
      } catch (error) {
        if (controller.signal.aborted) return;
        console.error('Global search failed:', error);
        setGlobalResults({ atoms: [], wiki: [], chats: [], tags: [] });
        setHybridAtomResults([]);
      } finally {
        if (searchAbortRef.current === controller) {
          searchAbortRef.current = null;
          setIsSearching(false);
        }
      }
    }, SEARCH_DEBOUNCE_MS);

    return () => {
      if (searchTimeoutRef.current) {
        clearTimeout(searchTimeoutRef.current);
      }
      controller.abort();
    };
  }, [mode, searchQuery]);

  const tagResults = useMemo(() => {
    if (mode !== 'tags') return [];
    const trimmed = searchQuery.trim().toLowerCase();
    if (!trimmed) return [];

    return flattenTags(tags)
      .map((tag) => {
        const lower = tag.name.toLowerCase();
        let score = 0;
        if (lower === trimmed) {
          score = 1;
        } else if (lower.startsWith(trimmed)) {
          score = 0.95;
        } else if (strongSubstringMatch(lower, trimmed)) {
          score = 0.8;
        }

        return score > 0
          ? {
              id: tag.id,
              name: tag.name,
              parent_id: tag.parent_id,
              created_at: tag.created_at,
              atom_count: tag.atom_count,
              score,
            }
          : null;
      })
      .filter((tag): tag is GlobalTagSearchResult => tag !== null)
      .sort((a, b) => b.score - a.score || b.atom_count - a.atom_count || a.name.localeCompare(b.name))
      .slice(0, SECTION_LIMIT * 2);
  }, [mode, searchQuery, tags]);

  const flatItems = useMemo<SearchPaletteItem[]>(() => {
    if (mode === 'tags') {
      return tagResults.map((result) => ({ kind: 'tag', result }));
    }

    const expandAtomRow = (result: SemanticSearchResult): SearchPaletteItem[] => {
      const offsets = result.match_offsets ?? [];
      const totalMatches = result.match_count ?? offsets.length;
      const expandable = totalMatches > 1;
      const expanded = expandable && expandedAtomIds.has(result.id);
      const header: SearchPaletteItem = {
        kind: 'atom',
        result,
        expandable,
        expanded,
      };
      if (!expanded) return [header];
      // Convert the backend's UTF-8 byte offsets into UTF-16 indices usable
      // with `content.slice`. Only done on expansion so the cost is paid at
      // most once per atom, only when the user drills in.
      const utf16Offsets = byteOffsetsToUtf16(result.content, offsets);
      const subRows: SearchPaletteItem[] = utf16Offsets.map((offset, matchIndex) => ({
        kind: 'atom-match',
        atom: result,
        matchIndex,
        offset,
      }));
      const hiddenCount = totalMatches - offsets.length;
      if (hiddenCount > 0) {
        subRows.push({ kind: 'atom-match-more', atom: result, hiddenCount });
      }
      return [header, ...subRows];
    };

    const expandWikiRow = (result: GlobalWikiSearchResult): SearchPaletteItem[] => {
      const offsets = result.match_offsets ?? [];
      const totalMatches = result.match_count ?? offsets.length;
      const expandable = totalMatches > 1;
      const expanded = expandable && expandedWikiIds.has(result.id);
      const header: SearchPaletteItem = {
        kind: 'wiki',
        result,
        expandable,
        expanded,
      };
      if (!expanded) return [header];
      const utf16Offsets = byteOffsetsToUtf16(result.content, offsets);
      const subRows: SearchPaletteItem[] = utf16Offsets.map((offset, matchIndex) => ({
        kind: 'wiki-match',
        wiki: result,
        matchIndex,
        offset,
      }));
      const hiddenCount = totalMatches - offsets.length;
      if (hiddenCount > 0) {
        subRows.push({ kind: 'wiki-match-more', wiki: result, hiddenCount });
      }
      return [header, ...subRows];
    };

    if (mode === 'atoms-hybrid') {
      return hybridAtomResults.flatMap(expandAtomRow);
    }

    return [
      ...globalResults.atoms.flatMap(expandAtomRow),
      ...globalResults.wiki.flatMap(expandWikiRow),
      ...globalResults.chats.map((result) => ({ kind: 'chat' as const, result })),
      ...globalResults.tags.map((result) => ({ kind: 'tag' as const, result })),
    ];
  }, [mode, globalResults, hybridAtomResults, tagResults, expandedAtomIds, expandedWikiIds]);

  const totalItems = flatItems.length;

  // Keep selectedIndex in range when the flat list shrinks (e.g., on collapse
  // or when a new search returns fewer results than the prior cursor offset).
  useEffect(() => {
    if (totalItems === 0) {
      if (selectedIndex !== 0) setSelectedIndex(0);
      return;
    }
    if (selectedIndex >= totalItems) {
      setSelectedIndex(totalItems - 1);
    }
  }, [totalItems, selectedIndex]);

  const handleSelect = useCallback(
    (index: number) => {
      const item = flatItems[index];
      if (!item) return;

      onClose();

      switch (item.kind) {
        case 'atom': {
          // Keyword-mode hits have a precise match, so highlight just the
          // query itself. Semantic/hybrid hits have no literal anchor, so
          // fall back to the matching chunk for approximate reveal.
          const trimmedQuery = searchQuery.trim();
          const highlight =
            mode === 'atoms-hybrid'
              ? item.result.matching_chunk_content || trimmedQuery
              : trimmedQuery || item.result.matching_chunk_content;
          useUIStore.getState().openReader(item.result.id, highlight);
          break;
        }
        case 'atom-match': {
          // Pass a unique surrounding window as the reveal text so the editor's
          // initialRevealText substring search lands on *this* specific match
          // rather than the first occurrence of the bare query.
          const { content } = item.atom;
          const start = Math.max(0, item.offset.start - MATCH_REVEAL_PAD);
          const end = Math.min(content.length, item.offset.end + MATCH_REVEAL_PAD);
          const window = content.slice(start, end);
          useUIStore.getState().openReader(item.atom.id, window);
          break;
        }
        case 'atom-match-more': {
          // "+N more matches" tail row — no specific offset, so behave like
          // the atom header and let the reader reveal the first occurrence.
          const trimmedQuery = searchQuery.trim();
          useUIStore.getState().openReader(item.atom.id, trimmedQuery);
          break;
        }
        case 'wiki': {
          const trimmedQuery = searchQuery.trim();
          const highlight = trimmedQuery || undefined;
          useUIStore
            .getState()
            .openWikiReader(item.result.tag_id, item.result.tag_name, highlight);
          break;
        }
        case 'wiki-match': {
          // The wiki reader renders markdown to plaintext, so pass a
          // markdown-stripped window (not the raw source slice) — otherwise
          // the reader's substring search won't find syntax like `[link](url)`
          // that doesn't survive rendering.
          const { content } = item.wiki;
          const start = Math.max(0, item.offset.start - MATCH_REVEAL_PAD);
          const end = Math.min(content.length, item.offset.end + MATCH_REVEAL_PAD);
          const rawWindow = content.slice(start, end);
          const highlight = markdownToPlainText(rawWindow) || rawWindow;
          useUIStore
            .getState()
            .openWikiReader(item.wiki.tag_id, item.wiki.tag_name, highlight);
          break;
        }
        case 'wiki-match-more': {
          const trimmedQuery = searchQuery.trim();
          const highlight = trimmedQuery || undefined;
          useUIStore
            .getState()
            .openWikiReader(item.wiki.tag_id, item.wiki.tag_name, highlight);
          break;
        }
        case 'chat':
          useUIStore.getState().openChatSidebar(undefined, item.result.id);
          void useChatStore.getState().openConversation(item.result.id);
          break;
        case 'tag': {
          const ancestorIds: string[] = [];
          const allTags = flattenTags(tags);
          const tagMap = new Map(allTags.map((tag) => [tag.id, tag]));
          let currentParentId = item.result.parent_id;
          while (currentParentId) {
            ancestorIds.push(currentParentId);
            currentParentId = tagMap.get(currentParentId)?.parent_id ?? null;
          }
          if (ancestorIds.length > 0) {
            useUIStore.getState().expandTagPath(ancestorIds);
          }
          useUIStore.getState().setSelectedTag(item.result.id);
          void useAtomsStore.getState().fetchAtomsByTag(item.result.id);
          break;
        }
      }
    },
    [flatItems, onClose, searchQuery, tags]
  );

  const toggleAtomExpanded = useCallback((id: string) => {
    setExpandedAtomIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const toggleWikiExpanded = useCallback((id: string) => {
    setExpandedWikiIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case 'ArrowDown':
          e.preventDefault();
          setSelectedIndex((prev) => Math.min(prev + 1, totalItems - 1));
          break;
        case 'ArrowUp':
          e.preventDefault();
          setSelectedIndex((prev) => Math.max(prev - 1, 0));
          break;
        case 'ArrowRight': {
          const item = flatItems[selectedIndex];
          if (!item) break;
          if (item.kind === 'atom' && item.expandable && !item.expanded) {
            e.preventDefault();
            setExpandedAtomIds((prev) => {
              const next = new Set(prev);
              next.add(item.result.id);
              return next;
            });
          } else if (item.kind === 'wiki' && item.expandable && !item.expanded) {
            e.preventDefault();
            setExpandedWikiIds((prev) => {
              const next = new Set(prev);
              next.add(item.result.id);
              return next;
            });
          }
          break;
        }
        case 'ArrowLeft': {
          const item = flatItems[selectedIndex];
          if (!item) break;
          if (item.kind === 'atom-match' || item.kind === 'atom-match-more') {
            e.preventDefault();
            const parentId = item.atom.id;
            const parentIdx = flatItems.findIndex(
              (it) => it.kind === 'atom' && it.result.id === parentId,
            );
            setExpandedAtomIds((prev) => {
              const next = new Set(prev);
              next.delete(parentId);
              return next;
            });
            if (parentIdx >= 0) setSelectedIndex(parentIdx);
          } else if (item.kind === 'wiki-match' || item.kind === 'wiki-match-more') {
            e.preventDefault();
            const parentId = item.wiki.id;
            const parentIdx = flatItems.findIndex(
              (it) => it.kind === 'wiki' && it.result.id === parentId,
            );
            setExpandedWikiIds((prev) => {
              const next = new Set(prev);
              next.delete(parentId);
              return next;
            });
            if (parentIdx >= 0) setSelectedIndex(parentIdx);
          } else if (item.kind === 'atom' && item.expanded) {
            e.preventDefault();
            setExpandedAtomIds((prev) => {
              const next = new Set(prev);
              next.delete(item.result.id);
              return next;
            });
          } else if (item.kind === 'wiki' && item.expanded) {
            e.preventDefault();
            setExpandedWikiIds((prev) => {
              const next = new Set(prev);
              next.delete(item.result.id);
              return next;
            });
          }
          break;
        }
        case 'Enter':
          e.preventDefault();
          handleSelect(selectedIndex);
          break;
        case 'Escape':
          e.preventDefault();
          onClose();
          break;
      }
    },
    [selectedIndex, totalItems, flatItems, handleSelect, onClose]
  );

  return {
    query,
    setQuery,
    prefix,
    mode,
    searchQuery,
    selectedIndex,
    isSearching,
    globalResults,
    hybridAtomResults,
    tagResults,
    expandedAtomIds,
    expandedWikiIds,
    toggleAtomExpanded,
    toggleWikiExpanded,
    handleKeyDown,
    handleSelect,
  };
}
