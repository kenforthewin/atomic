import { create } from 'zustand';
import { getTransport } from '../lib/transport';
import {
  isWorkspaceOnly,
  type SettingSource,
  type SettingValue,
} from '../lib/settings';
import { useDatabasesStore } from './databases';

/**
 * The store keeps two parallel maps so existing call sites can keep reading
 * `settings.foo` for the resolved value, while override-aware UI reads
 * `sources.foo` to decide whether to render override affordances.
 *
 * Mutations:
 *   - `setSetting`     — routes per the resolver (workspace-only → registry;
 *                        overridable + N≤1 → registry default; overridable +
 *                        N>1 → per-DB override).
 *   - `clearOverride`  — DELETE the per-DB override; resolver falls back to
 *                        the workspace default. Refetches to pick up the new
 *                        resolved value/source.
 *
 * Note: `set_workspace_default` exists on the backend (`PUT
 * /api/settings/defaults/{key}`) as a primitive for a possible future
 * "change for all DBs" feature, but isn't wired into the store — multi-DB
 * users edit per-DB only. Workspace defaults are the inheritance source for
 * new DBs, frozen at whatever values were live during the N=1 phase.
 */
interface SettingsStore {
  settings: Record<string, string>;
  sources: Record<string, SettingSource>;
  isLoading: boolean;
  error: string | null;

  fetchSettings: () => Promise<void>;
  setSetting: (key: string, value: string) => Promise<void>;
  clearOverride: (key: string) => Promise<void>;
  testOpenRouterConnection: (apiKey: string) => Promise<boolean>;
  testOrcaRouterConnection: (apiKey: string) => Promise<boolean>;
}

function splitResolvedSettings(
  resolved: Record<string, SettingValue>,
): { settings: Record<string, string>; sources: Record<string, SettingSource> } {
  const settings: Record<string, string> = {};
  const sources: Record<string, SettingSource> = {};
  for (const [key, entry] of Object.entries(resolved)) {
    settings[key] = entry.value;
    sources[key] = entry.source;
  }
  return { settings, sources };
}

export const useSettingsStore = create<SettingsStore>((set) => ({
  settings: {},
  sources: {},
  isLoading: false,
  error: null,

  fetchSettings: async () => {
    set({ isLoading: true, error: null });
    try {
      const resolved = await getTransport().invoke<Record<string, SettingValue>>(
        'get_settings',
      );
      const { settings, sources } = splitResolvedSettings(resolved);
      set({ settings, sources, isLoading: false });
    } catch (e) {
      set({ error: String(e), isLoading: false });
    }
  },

  setSetting: async (key: string, value: string) => {
    const current = useSettingsStore.getState().settings[key];
    if (current === value) return;
    try {
      await getTransport().invoke('set_setting', { key, value });
      // Mirror the backend resolver's routing so the override chip flips
      // immediately on edit instead of waiting for a refetch. The rules
      // here must stay in sync with `AtomicCore::set_setting` — workspace-
      // only keys land in registry, overridable keys land in registry while
      // N≤1 (workspace default) and per-DB once N≥2 (override).
      const dbCount = useDatabasesStore.getState().databases.length;
      const newSource: SettingSource = isWorkspaceOnly(key)
        ? 'workspace'
        : dbCount <= 1
          ? 'workspace_default'
          : 'override';
      set((state) => ({
        settings: { ...state.settings, [key]: value },
        sources: { ...state.sources, [key]: newSource },
      }));
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  clearOverride: async (key: string) => {
    try {
      await getTransport().invoke('clear_setting_override', { key });
      // Resolved value flips to the workspace default, which we don't know
      // without re-asking the server.
      await useSettingsStore.getState().fetchSettings();
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },

  testOpenRouterConnection: async (apiKey: string) => {
    const result = await getTransport().invoke<boolean>(
      'test_openrouter_connection',
      { apiKey },
    );
    return result;
  },

  testOrcaRouterConnection: async (apiKey: string) => {
    const result = await getTransport().invoke<boolean>(
      'test_orcarouter_connection',
      { apiKey },
    );
    return result;
  },
}));
