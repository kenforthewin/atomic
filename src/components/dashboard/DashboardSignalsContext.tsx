import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { getTransport } from '../../lib/transport';
import type {
  DashboardKnowledgeSignals,
  KnowledgeSignal,
  KnowledgeSignalProviderSettings,
} from '../../types/knowledgeSignals';

const DASHBOARD_PROVIDER_LIMIT = 20;

interface DashboardSignalsContextValue {
  response: DashboardKnowledgeSignals | null;
  providerSettings: KnowledgeSignalProviderSettings[] | null;
  isLoading: boolean;
  error: string | null;
  getProviderSignals: <Evidence = Record<string, unknown>>(providerId: string) => KnowledgeSignal<Evidence>[];
  removeSignal: (signalKey: string) => void;
  refreshAll: () => Promise<void>;
  refreshProvider: <Evidence = Record<string, unknown>>(providerId: string, limit?: number) => Promise<KnowledgeSignal<Evidence>[]>;
}

const DashboardSignalsContext = createContext<DashboardSignalsContextValue | null>(null);

export function DashboardSignalsProvider({ children }: { children: ReactNode }) {
  const [response, setResponse] = useState<DashboardKnowledgeSignals | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const refreshAll = useCallback(async () => {
    setIsLoading(true);
    setError(null);
    try {
      const next = await getTransport().invoke<DashboardKnowledgeSignals>('list_dashboard_knowledge_signals', {
        limit: DASHBOARD_PROVIDER_LIMIT,
      });
      setResponse(next);
    } catch (err) {
      console.error('Failed to load dashboard signal groups:', err);
      setError(String(err));
    } finally {
      setIsLoading(false);
    }
  }, []);

  useEffect(() => {
    refreshAll();
  }, [refreshAll]);

  useEffect(() => {
    const handleChanged = (event: Event) => {
      const detail = event instanceof CustomEvent ? event.detail : null;
      if (detail?.providerId) {
        void refreshAll();
      }
    };
    window.addEventListener('knowledge-signals:changed', handleChanged);
    return () => window.removeEventListener('knowledge-signals:changed', handleChanged);
  }, [refreshAll]);

  const removeSignal = useCallback((signalKey: string) => {
    setResponse(current => {
      if (!current) return current;
      return {
        ...current,
        groups: current.groups.map(group => ({
          ...group,
          signals: group.signals.filter(signal => signal.id !== signalKey),
        })),
      };
    });
  }, []);

  useEffect(() => {
    const handleChanged = (event: Event) => {
      const detail = event instanceof CustomEvent ? event.detail : null;
      if (typeof detail?.signalKey === 'string') {
        removeSignal(detail.signalKey);
      }
    };
    window.addEventListener('knowledge-signals:changed', handleChanged);
    return () => window.removeEventListener('knowledge-signals:changed', handleChanged);
  }, [removeSignal]);

  const refreshProvider = useCallback(async <Evidence = Record<string, unknown>,>(providerId: string, limit = DASHBOARD_PROVIDER_LIMIT) => {
    const signals = await getTransport().invoke<KnowledgeSignal<Evidence>[]>('list_knowledge_signals', {
      providerId,
      limit,
    });
    setResponse(current => {
      if (!current) return current;
      const setting = current.provider_settings.find(provider => provider.provider_id === providerId);
      const existing = current.groups.find(group => group.provider_id === providerId);
      const nextGroup = {
        provider_id: providerId,
        name: existing?.name ?? setting?.name ?? providerId,
        evaluation_ms: existing?.evaluation_ms ?? 0,
        signals: signals as KnowledgeSignal[],
      };
      const seen = new Set<string>();
      const groups = current.groups.map(group => {
        if (group.provider_id !== providerId) return group;
        seen.add(providerId);
        return nextGroup;
      });
      if (!seen.has(providerId)) groups.push(nextGroup);
      return { ...current, groups };
    });
    return signals;
  }, []);

  const groupsByProvider = useMemo(() => {
    const map = new Map<string, KnowledgeSignal[]>();
    for (const group of response?.groups ?? []) {
      map.set(group.provider_id, group.signals);
    }
    return map;
  }, [response]);

  const getProviderSignals = useCallback(<Evidence = Record<string, unknown>,>(providerId: string) => {
    return (groupsByProvider.get(providerId) ?? []) as KnowledgeSignal<Evidence>[];
  }, [groupsByProvider]);

  const value: DashboardSignalsContextValue = {
    response,
    providerSettings: response?.provider_settings ?? null,
    isLoading,
    error,
    getProviderSignals,
    removeSignal,
    refreshAll,
    refreshProvider,
  };

  return (
    <DashboardSignalsContext.Provider value={value}>
      {children}
    </DashboardSignalsContext.Provider>
  );
}

export function useDashboardSignals() {
  return useContext(DashboardSignalsContext);
}
