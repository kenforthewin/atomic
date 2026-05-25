import { useEffect, useMemo, useState } from 'react';
import { Loader2, RefreshCw } from 'lucide-react';
import { toast } from 'sonner';
import { getTransport } from '../../lib/transport';
import type { KnowledgeSignalProviderConfig, KnowledgeSignalProviderSettings } from '../../types/knowledgeSignals';

type Sensitivity = 'more' | 'balanced' | 'strict';

interface ProviderCopy {
  label: string;
  description: string;
  group: string;
  baseScore: number;
  baseConfidence: number;
}

const PROVIDER_COPY: Record<string, ProviderCopy> = {
  wiki_candidate: {
    label: 'Wiki opportunities',
    description: 'Tags that are ready for a new wiki.',
    group: 'Wiki',
    baseScore: 0,
    baseConfidence: 0,
  },
  wiki_update: {
    label: 'Wiki updates',
    description: 'Existing wikis with enough new material to revise.',
    group: 'Wiki',
    baseScore: 10,
    baseConfidence: 0.1,
  },
  tag_redundancy: {
    label: 'Tag cleanup',
    description: 'Tags that strongly overlap or can be merged.',
    group: 'Organization',
    baseScore: 45,
    baseConfidence: 0.55,
  },
  empty_tag: {
    label: 'Empty tags',
    description: 'Unused tags with no atoms or child tags.',
    group: 'Organization',
    baseScore: 10,
    baseConfidence: 0.8,
  },
  missing_tag_overlap: {
    label: 'Ideas to connect',
    description: 'Atoms near a tagged cluster but missing that tag.',
    group: 'Connections',
    baseScore: 50,
    baseConfidence: 0.65,
  },
  near_duplicate_atom: {
    label: 'Similar notes',
    description: 'Atoms with very similar meaning.',
    group: 'Cleanup',
    baseScore: 55,
    baseConfidence: 0.6,
  },
  source_duplicate: {
    label: 'Duplicate sources',
    description: 'Atoms captured from the same source URL.',
    group: 'Cleanup',
    baseScore: 60,
    baseConfidence: 0.8,
  },
  broken_internal_link: {
    label: 'Broken links',
    description: 'Internal links that do not resolve cleanly.',
    group: 'Links',
    baseScore: 35,
    baseConfidence: 0.65,
  },
  underconnected_atom: {
    label: 'Underconnected notes',
    description: 'Atoms with little graph or tag connection.',
    group: 'Connections',
    baseScore: 55,
    baseConfidence: 0.6,
  },
};

function providerCopy(provider: KnowledgeSignalProviderSettings): ProviderCopy {
  return PROVIDER_COPY[provider.provider_id] ?? {
    label: provider.name,
    description: provider.provider_id,
    group: 'Other',
    baseScore: provider.config.min_score,
    baseConfidence: provider.config.min_confidence,
  };
}

function thresholdsFor(providerId: string, sensitivity: Sensitivity) {
  const base = PROVIDER_COPY[providerId] ?? {
    baseScore: 0,
    baseConfidence: 0,
  };

  if (sensitivity === 'more') {
    return {
      min_score: Math.max(0, base.baseScore - 15),
      min_confidence: Math.max(0, base.baseConfidence - 0.15),
    };
  }
  if (sensitivity === 'strict') {
    return {
      min_score: Math.min(95, base.baseScore + 15),
      min_confidence: Math.min(0.95, base.baseConfidence + 0.15),
    };
  }
  return {
    min_score: base.baseScore,
    min_confidence: base.baseConfidence,
  };
}

function inferSensitivity(providerId: string, config: KnowledgeSignalProviderConfig): Sensitivity {
  const balanced = thresholdsFor(providerId, 'balanced');
  const strict = thresholdsFor(providerId, 'strict');
  if (config.min_score >= strict.min_score || config.min_confidence >= strict.min_confidence) {
    return 'strict';
  }
  if (config.min_score < balanced.min_score || config.min_confidence < balanced.min_confidence) {
    return 'more';
  }
  return 'balanced';
}

export function KnowledgeSignalsSettingsTab() {
  const [providers, setProviders] = useState<KnowledgeSignalProviderSettings[]>([]);
  const [pending, setPending] = useState<Record<string, KnowledgeSignalProviderConfig>>({});
  const [savingProvider, setSavingProvider] = useState<string | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const visibleProviders = useMemo(() => {
    return [...providers].sort((a, b) => {
      const copyA = providerCopy(a);
      const copyB = providerCopy(b);
      return copyA.group.localeCompare(copyB.group) || copyA.label.localeCompare(copyB.label);
    });
  }, [providers]);

  const loadProviders = async () => {
    setIsLoading(true);
    setError(null);
    try {
      const result = await getTransport().invoke<KnowledgeSignalProviderSettings[]>('list_knowledge_signal_provider_configs');
      setProviders(result);
      setPending({});
    } catch (err) {
      console.error('Failed to load signal provider settings:', err);
      setError(String(err));
    } finally {
      setIsLoading(false);
    }
  };

  useEffect(() => {
    loadProviders();
  }, []);

  const saveConfig = async (providerId: string, next: KnowledgeSignalProviderConfig) => {
    const previousProviders = providers;
    const previousPending = pending;
    setPending(current => ({ ...current, [providerId]: next }));
    setProviders(current =>
      current.map(provider =>
        provider.provider_id === providerId
          ? { ...provider, config: next }
          : provider
      )
    );
    setSavingProvider(providerId);
    try {
      const saved = await getTransport().invoke<KnowledgeSignalProviderConfig>('set_knowledge_signal_provider_config', {
        providerId,
        config: next,
      });
      setProviders(current =>
        current.map(provider =>
          provider.provider_id === providerId
            ? { ...provider, config: saved }
            : provider
        )
      );
      setPending(current => {
        const { [providerId]: _, ...rest } = current;
        return rest;
      });
      window.dispatchEvent(new CustomEvent('knowledge-signals:changed', { detail: { providerId } }));
    } catch (err) {
      console.error('Failed to save signal provider settings:', err);
      setProviders(previousProviders);
      setPending(previousPending);
      toast.error('Failed to save signal setting', { description: String(err) });
    } finally {
      setSavingProvider(null);
    }
  };

  const updateProvider = (provider: KnowledgeSignalProviderSettings, patch: Partial<KnowledgeSignalProviderConfig>) => {
    const current = pending[provider.provider_id] ?? provider.config;
    saveConfig(provider.provider_id, {
      ...current,
      ...patch,
      provider_id: provider.provider_id,
      config_json: current.config_json ?? {},
    });
  };

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-medium text-[var(--color-text-primary)]">Signal preferences</h3>
          <p className="mt-1 text-xs text-[var(--color-text-tertiary)]">
            Tune what appears on the dashboard and in the briefing for this database.
          </p>
        </div>
        <button
          onClick={loadProviders}
          disabled={isLoading}
          title="Refresh signal settings"
          className="rounded p-1.5 text-[var(--color-text-tertiary)] hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)] disabled:opacity-60"
        >
          {isLoading ? <Loader2 className="h-4 w-4 animate-spin" strokeWidth={2} /> : <RefreshCw className="h-4 w-4" strokeWidth={2} />}
        </button>
      </div>

      {isLoading ? (
        <div className="py-8 text-sm text-[var(--color-text-tertiary)]">Loading signal preferences...</div>
      ) : error ? (
        <div className="py-8 text-sm text-[var(--color-text-tertiary)]">Could not load signal preferences.</div>
      ) : (
        <div className="space-y-2">
          {visibleProviders.map(provider => {
            const copy = providerCopy(provider);
            const config = pending[provider.provider_id] ?? provider.config;
            const sensitivity = inferSensitivity(provider.provider_id, config);
            const isSaving = savingProvider === provider.provider_id;
            return (
              <div key={provider.provider_id} className="rounded-md border border-[var(--color-border)] bg-[var(--color-bg-card)] px-3 py-3">
                <div className="grid gap-3 lg:grid-cols-[minmax(0,1fr)_150px_260px] lg:items-center">
                  <div className="min-w-0">
                    <div className="flex min-w-0 items-center gap-2">
                      <span className="truncate text-sm font-medium text-[var(--color-text-primary)]">{copy.label}</span>
                      <span className="shrink-0 rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] uppercase text-[var(--color-text-tertiary)]">
                        {copy.group}
                      </span>
                      {isSaving && <Loader2 className="h-3.5 w-3.5 animate-spin text-[var(--color-text-tertiary)]" strokeWidth={2} />}
                    </div>
                    <div className="mt-1 truncate text-xs text-[var(--color-text-tertiary)]">{copy.description}</div>
                  </div>

                  <label className="flex items-center gap-2 text-xs text-[var(--color-text-secondary)]">
                    <span className="shrink-0">Sensitivity</span>
                    <select
                      value={sensitivity}
                      onChange={(event) => updateProvider(provider, thresholdsFor(provider.provider_id, event.target.value as Sensitivity))}
                      className="min-w-0 flex-1 rounded border border-[var(--color-border)] bg-[var(--color-bg-secondary)] px-2 py-1 text-xs text-[var(--color-text-primary)] focus:outline-none focus:ring-1 focus:ring-[var(--color-accent)]"
                    >
                      <option value="more">More</option>
                      <option value="balanced">Balanced</option>
                      <option value="strict">Strict</option>
                    </select>
                  </label>

                  <div className="grid grid-cols-3 gap-2">
                    <Toggle
                      label="Enabled"
                      checked={config.enabled}
                      onChange={(checked) => updateProvider(provider, { enabled: checked })}
                    />
                    <Toggle
                      label="Dashboard"
                      checked={config.show_on_dashboard}
                      onChange={(checked) => updateProvider(provider, { show_on_dashboard: checked })}
                    />
                    <Toggle
                      label="Briefing"
                      checked={config.include_in_briefing}
                      onChange={(checked) => updateProvider(provider, { include_in_briefing: checked })}
                    />
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function Toggle({ label, checked, onChange }: { label: string; checked: boolean; onChange: (checked: boolean) => void }) {
  return (
    <label className="flex items-center gap-1.5 text-xs text-[var(--color-text-secondary)]">
      <input
        type="checkbox"
        checked={checked}
        onChange={(event) => onChange(event.target.checked)}
        className="h-3.5 w-3.5 rounded border-[var(--color-border)] bg-[var(--color-bg-secondary)] accent-[var(--color-accent)]"
      />
      <span className="truncate">{label}</span>
    </label>
  );
}
