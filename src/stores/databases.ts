import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import { toast } from 'sonner';
import { getTransport } from '../lib/transport';
import { syncSharedConfig } from '../lib/mobile/shared-config';
import { useAtomsStore } from './atoms';
import { useTagsStore } from './tags';
import { useWikiStore } from './wiki';
import { useChatStore } from './chat';
import { useFeaturedReportStore } from './featuredReport';

export interface DatabaseInfo {
  id: string;
  name: string;
  is_default: boolean;
  created_at: string;
  last_opened_at: string | null;
}

export interface DatabaseStats {
  atom_count: number;
}

interface DatabasesStore {
  databases: DatabaseInfo[];
  activeId: string | null;
  isLoading: boolean;
  error: string | null;

  fetchDatabases: () => Promise<void>;
  createDatabase: (name: string) => Promise<DatabaseInfo>;
  renameDatabase: (id: string, name: string) => Promise<void>;
  deleteDatabase: (id: string) => Promise<void>;
  switchDatabase: (id: string) => Promise<void>;
  setDefaultDatabase: (id: string) => Promise<void>;
  getDatabaseStats: (id: string) => Promise<DatabaseStats>;
}

export const useDatabasesStore = create<DatabasesStore>()(
  persist(
    (set, get) => ({
  databases: [],
  activeId: null,
  isLoading: false,
  error: null,

  fetchDatabases: async () => {
    set({ isLoading: true, error: null });
    try {
      const transport = getTransport();
      const result = await transport.invoke('list_databases', {}) as {
        databases: DatabaseInfo[];
        active_id: string;
      };
      set({
        databases: result.databases,
        activeId: result.active_id,
        isLoading: false,
      });
      void syncSharedConfig({ databaseId: result.active_id });
    } catch (e) {
      set({ error: String(e), isLoading: false });
    }
  },

  createDatabase: async (name: string) => {
    try {
      const transport = getTransport();
      const info = await transport.invoke('create_database', { name }) as DatabaseInfo;
      await get().fetchDatabases();
      return info;
    } catch (e) {
      toast.error('Failed to create database', { description: String(e) });
      throw e;
    }
  },

  renameDatabase: async (id: string, name: string) => {
    try {
      const transport = getTransport();
      await transport.invoke('rename_database', { id, name });
      await get().fetchDatabases();
    } catch (e) {
      toast.error('Failed to rename database', { description: String(e) });
      throw e;
    }
  },

  deleteDatabase: async (id: string) => {
    try {
      const transport = getTransport();
      const wasActive = get().activeId === id;
      await transport.invoke('delete_database', { id });
      await get().fetchDatabases();

      // If the deleted DB was active, the backend switched to default —
      // reset data stores to load the new active DB's data
      if (wasActive) {
        useAtomsStore.getState().reset();
        useTagsStore.getState().reset();
        useWikiStore.getState().reset();
        useChatStore.getState().reset();
        useFeaturedReportStore.getState().reset();
        useTagsStore.getState().fetchTags();
        useAtomsStore.getState().fetchAtoms();
        useFeaturedReportStore.getState().fetchLatest();
      }
    } catch (e) {
      toast.error('Failed to delete database', { description: String(e) });
      throw e;
    }
  },

  setDefaultDatabase: async (id: string) => {
    try {
      const transport = getTransport();
      await transport.invoke('set_default_database', { id });
      await get().fetchDatabases();
    } catch (e) {
      toast.error('Failed to set default database', { description: String(e) });
      throw e;
    }
  },

  getDatabaseStats: async (id: string) => {
    try {
      const transport = getTransport();
      return await transport.invoke('get_database_stats', { id }) as DatabaseStats;
    } catch (e) {
      toast.error('Failed to load database stats', { description: String(e) });
      throw e;
    }
  },

  switchDatabase: async (id: string) => {
    try {
      const transport = getTransport();
      await transport.invoke('activate_database', { id });
      set({ activeId: id });
      void syncSharedConfig({ databaseId: id });

      // Reset all data stores to force refetch
      useAtomsStore.getState().reset();
      useTagsStore.getState().reset();
      useWikiStore.getState().reset();
      useChatStore.getState().reset();
      useFeaturedReportStore.getState().reset();

      // Hydrate the new DB's cached data before firing fetches — user sees
      // the sidebar/list for the new DB instantly instead of an empty flash.
      await Promise.all([
        useTagsStore.getState().hydrateFromCache(id),
        useAtomsStore.getState().hydrateFromCache(id),
      ]);

      // Refetch data for the new database
      useTagsStore.getState().fetchTags();
      useAtomsStore.getState().fetchAtoms();
      useFeaturedReportStore.getState().fetchLatest();
    } catch (e) {
      toast.error('Failed to switch database', { description: String(e) });
      throw e;
    }
  },
    }),
    {
      name: 'atomic-databases-storage',
      version: 1,
      // Only persist the activeId — the `databases` list, loading state, and
      // errors should come from the server fresh. activeId is what we need
      // *before* the network responds so cold-start can hydrate the right
      // DB's cache.
      partialize: (state) => ({ activeId: state.activeId }),
    },
  ),
);
