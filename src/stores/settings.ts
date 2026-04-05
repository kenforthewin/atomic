import { create } from 'zustand';
import { getTransport } from '../lib/transport';

interface SettingsStore {
  settings: Record<string, string>;
  isLoading: boolean;
  error: string | null;
  
  fetchSettings: () => Promise<void>;
  setSetting: (key: string, value: string) => Promise<any>;
  testOpenRouterConnection: (apiKey: string) => Promise<boolean>;
}

export const useSettingsStore = create<SettingsStore>((set) => ({
  settings: {},
  isLoading: false,
  error: null,
  
  fetchSettings: async () => {
    set({ isLoading: true, error: null });
    try {
      const settings = await getTransport().invoke<Record<string, string>>('get_settings');
      set({ settings, isLoading: false });
    } catch (e) {
      set({ error: String(e), isLoading: false });
    }
  },
  
  setSetting: async (key: string, value: string) => {
    try {
      const result = await getTransport().invoke('set_setting', { key, value });
      set((state) => ({
        settings: { ...state.settings, [key]: value }
      }));
      return result;
    } catch (e) {
      set({ error: String(e) });
      throw e;
    }
  },
  
  testOpenRouterConnection: async (apiKey: string) => {
    const result = await getTransport().invoke<boolean>('test_openrouter_connection', { apiKey });
    return result;
  },
}));

