import { create } from 'zustand';
import { getTransport } from '../lib/transport';
import { cacheKey, readCache, writeCache } from '../lib/cache/idb';
import { useDatabasesStore } from './databases';

export interface Tag {
  id: string;
  name: string;
  parent_id: string | null;
  created_at: string;
  is_autotag_target: boolean;
  autotag_description: string;
}

/// Per-tag overrides for the prompts that write and update that tag's wiki
/// article. `null` on either field means "no override" — the global prompt
/// from settings applies, and failing that Atomic's built-in prompt. Kept off
/// the `Tag` model on purpose: prompts can be multi-KB and the tree fetches
/// hundreds of tags at a time, so they are loaded on demand.
export interface TagWikiPrompts {
  generation_prompt: string | null;
  update_prompt: string | null;
}

export interface TagWithCount extends Tag {
  atom_count: number;
  children_total: number;
  children: TagWithCount[];
}

interface PaginatedTagChildren {
  children: TagWithCount[];
  total: number;
}

export interface CompactionResult {
  tags_moved: number;
  tags_merged: number;
  atoms_retagged: number;
}

const TAG_CHILDREN_PAGE_SIZE = 100;

interface TagsStore {
  tags: TagWithCount[];
  isLoading: boolean;
  isCompacting: boolean;
  error: string | null;
  hydrateFromCache: (dbId?: string | null) => Promise<void>;
  fetchTags: () => Promise<void>;
  fetchTagChildren: (parentId: string) => Promise<void>;
  fetchMoreTagChildren: (parentId: string) => Promise<void>;
  createTag: (name: string, parentId?: string) => Promise<Tag>;
  updateTag: (id: string, name: string, parentId?: string) => Promise<Tag>;
  deleteTag: (id: string, recursive?: boolean) => Promise<void>;
  setTagAutotagTarget: (id: string, value: boolean) => Promise<void>;
  setTagAutotagDescription: (id: string, description: string) => Promise<void>;
  fetchTagWikiPrompts: (id: string) => Promise<TagWikiPrompts>;
  saveTagWikiPrompts: (id: string, prompts: TagWikiPrompts) => Promise<TagWikiPrompts>;
  configureAutotagTargets: (keepDefaults: string[], addCustom: string[]) => Promise<Tag[]>;
  compactTags: () => Promise<CompactionResult>;
  clearError: () => void;
  reset: () => void;
}

function replaceChildrenInTree(
  nodes: TagWithCount[],
  parentId: string,
  newChildren: TagWithCount[],
  total: number,
): TagWithCount[] {
  return nodes.map((node) => {
    if (node.id === parentId) {
      return { ...node, children: newChildren, children_total: total };
    }
    if (node.children.length > 0) {
      return { ...node, children: replaceChildrenInTree(node.children, parentId, newChildren, total) };
    }
    return node;
  });
}

function appendChildrenInTree(
  nodes: TagWithCount[],
  parentId: string,
  moreChildren: TagWithCount[],
): TagWithCount[] {
  return nodes.map((node) => {
    if (node.id === parentId) {
      return { ...node, children: [...node.children, ...moreChildren] };
    }
    if (node.children.length > 0) {
      return { ...node, children: appendChildrenInTree(node.children, parentId, moreChildren) };
    }
    return node;
  });
}

function findTagInTree(nodes: TagWithCount[], tagId: string): TagWithCount | null {
  for (const node of nodes) {
    if (node.id === tagId) return node;
    if (node.children.length > 0) {
      const found = findTagInTree(node.children, tagId);
      if (found) return found;
    }
  }
  return null;
}

/// Fetch the full tag tree from the server and persist it to the IDB cache.
/// Several mutation endpoints refetch-then-set for the updated tree; this
/// helper keeps the cache in sync with each of those writes in one place.
async function fetchAllTagsFresh(): Promise<TagWithCount[]> {
  const tags = await getTransport().invoke<TagWithCount[]>('get_all_tags');
  const dbId = useDatabasesStore.getState().activeId;
  if (dbId) void writeCache(cacheKey('tags', dbId), tags);
  return tags;
}

export const useTagsStore = create<TagsStore>((set, get) => ({
  tags: [],
  isLoading: false,
  isCompacting: false,
  error: null,

  fetchTags: async () => {
    set({ isLoading: true, error: null });
    try {
      const tags = await fetchAllTagsFresh();
      set({ tags, isLoading: false });
    } catch (error) {
      set({ error: String(error), isLoading: false });
    }
  },

  hydrateFromCache: async (dbId?: string | null) => {
    const resolvedDbId = dbId ?? useDatabasesStore.getState().activeId;
    if (!resolvedDbId) return;
    // Don't clobber a freshly-fetched tree.
    if (get().tags.length > 0) return;
    const cached = await readCache<TagWithCount[]>(cacheKey('tags', resolvedDbId));
    if (!cached) return;
    if (get().tags.length > 0) return;
    set({ tags: cached.data });
  },

  fetchTagChildren: async (parentId: string) => {
    try {
      const result = await getTransport().invoke<PaginatedTagChildren>('get_tag_children', {
        parentId,
        minCount: 0,
        limit: TAG_CHILDREN_PAGE_SIZE,
        offset: 0,
      });
      set((state) => ({
        tags: replaceChildrenInTree(state.tags, parentId, result.children, result.total),
      }));
    } catch (error) {
      set({ error: String(error) });
    }
  },

  fetchMoreTagChildren: async (parentId: string) => {
    try {
      const parent = findTagInTree(get().tags, parentId);
      if (!parent) return;
      const offset = parent.children.length;
      if (offset >= parent.children_total) return;

      const result = await getTransport().invoke<PaginatedTagChildren>('get_tag_children', {
        parentId,
        minCount: 0,
        limit: TAG_CHILDREN_PAGE_SIZE,
        offset,
      });
      set((state) => ({
        tags: appendChildrenInTree(state.tags, parentId, result.children),
      }));
    } catch (error) {
      set({ error: String(error) });
    }
  },

  createTag: async (name: string, parentId?: string) => {
    set({ error: null });
    try {
      const tag = await getTransport().invoke<Tag>('create_tag', {
        name,
        parentId: parentId || null,
      });
      // Refetch tags to get updated tree structure
      const tags = await fetchAllTagsFresh();
      set({ tags });
      return tag;
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  updateTag: async (id: string, name: string, parentId?: string) => {
    set({ error: null });
    try {
      const tag = await getTransport().invoke<Tag>('update_tag', {
        id,
        name,
        parentId: parentId || null,
      });
      // Refetch tags to get updated tree structure
      const tags = await fetchAllTagsFresh();
      set({ tags });
      return tag;
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  deleteTag: async (id: string, recursive?: boolean) => {
    set({ error: null });
    try {
      await getTransport().invoke('delete_tag', { id, recursive: recursive ?? false });
      // Refetch tags to get updated tree structure
      const tags = await fetchAllTagsFresh();
      set({ tags });
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  setTagAutotagTarget: async (id: string, value: boolean) => {
    set({ error: null });
    try {
      await getTransport().invoke('set_tag_autotag_target', { id, value });
      const tags = await fetchAllTagsFresh();
      set({ tags });
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  setTagAutotagDescription: async (id: string, description: string) => {
    set({ error: null });
    try {
      await getTransport().invoke('set_tag_autotag_description', { id, description });
      const tags = await fetchAllTagsFresh();
      set({ tags });
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  // Wiki prompts live outside the cached tree, so neither action refetches
  // it. Callers hold the values for as long as their editor is open.
  fetchTagWikiPrompts: async (id: string) => {
    set({ error: null });
    try {
      return await getTransport().invoke<TagWikiPrompts>('get_tag_wiki_prompts', { id });
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  saveTagWikiPrompts: async (id: string, prompts: TagWikiPrompts) => {
    set({ error: null });
    try {
      // The server normalizes blank prompts to null and echoes the stored
      // record back, so the response is what the tag now has. `id` is spread
      // last so the tag being written is always the one the caller named,
      // whatever the prompts object happens to carry.
      return await getTransport().invoke<TagWikiPrompts>('set_tag_wiki_prompts', { ...prompts, id });
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  configureAutotagTargets: async (keepDefaults: string[], addCustom: string[]) => {
    set({ error: null });
    try {
      const created = await getTransport().invoke<Tag[]>('configure_autotag_targets', {
        keepDefaults,
        addCustom,
      });
      const tags = await fetchAllTagsFresh();
      set({ tags });
      return created;
    } catch (error) {
      set({ error: String(error) });
      throw error;
    }
  },

  compactTags: async () => {
    set({ isCompacting: true, error: null });
    try {
      const result = await getTransport().invoke<CompactionResult>('compact_tags');
      // Refetch tags to get updated tree structure
      const tags = await fetchAllTagsFresh();
      set({ tags, isCompacting: false });
      return result;
    } catch (error) {
      set({ error: String(error), isCompacting: false });
      throw error;
    }
  },

  clearError: () => set({ error: null }),

  reset: () => set({
    tags: [],
    isLoading: false,
    isCompacting: false,
    error: null,
  }),
}));
