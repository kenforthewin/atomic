import { useEffect, useState } from 'react';
import { Loader2, RotateCcw, Save } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import { toast } from '../../stores/toasts';
import { WikiExclusionPanel } from './WikiExclusionPanel';

// ---- Shape mirrors crates/atomic-core/src/health/mod.rs::HealthConfig ----

interface HealthCheckOverride {
  enabled: boolean;
  weight?: number | null;
}

interface HealthConfig {
  overrides: Record<string, HealthCheckOverride>;
}

// Canonical list of checks and their labels (mirrors CHECK_ORDER / CHECK_LABELS).
// `informational` marks checks that are opinionated ("completeness-style") —
// they default to zero weight and only contribute to the overall score when
// the user assigns an explicit weight.
const CHECKS: Array<{
  name: string;
  label: string;
  informational: boolean;
  defaultWeight: number;
  description: string;
}> = [
  { name: 'embedding_coverage', label: 'Embedding coverage', informational: false, defaultWeight: 0.20,
    description: 'Atoms missing or failed embeddings — needed for search.' },
  { name: 'tagging_coverage', label: 'Tagging coverage', informational: false, defaultWeight: 0.20,
    description: 'Atoms that never finished the tagging pipeline.' },
  { name: 'orphan_tags', label: 'Orphan tags', informational: false, defaultWeight: 0.15,
    description: 'Tags with zero atoms. Clutters the tree.' },
  { name: 'source_uniqueness', label: 'Source duplicates', informational: false, defaultWeight: 0.10,
    description: 'Accidentally re-imported atoms with identical source URL.' },
  { name: 'semantic_graph_freshness', label: 'Semantic graph freshness', informational: false, defaultWeight: 0.10,
    description: 'Atoms added since the last semantic-edge rebuild.' },
  { name: 'tag_health', label: 'Tag health', informational: false, defaultWeight: 0.10,
    description: 'Rootless tags, single-atom tags, near-duplicate tag names.' },
  { name: 'broken_internal_links', label: 'Broken internal links', informational: false, defaultWeight: 0.10,
    description: 'Wikilinks and [[refs]] that no longer resolve.' },
  { name: 'content_overlap', label: 'Content overlap', informational: false, defaultWeight: 0.05,
    description: 'Cross-source near-duplicates — potential redundancy.' },
  // Informational / opinionated
  { name: 'wiki_coverage', label: 'Wiki coverage', informational: true, defaultWeight: 0,
    description: 'Tags without wikis. Opinionated — you may not want a wiki per tag.' },
  { name: 'content_quality', label: 'Content quality (length / heading / source)', informational: true, defaultWeight: 0,
    description: 'Atoms without source, very long, very short, or missing headings. Opinionated.' },
  { name: 'contradiction_detection', label: 'Contradiction detection', informational: true, defaultWeight: 0,
    description: 'Semantically-similar atoms that disagree. Opinionated — disagreement may be intentional.' },
  { name: 'boilerplate_pollution', label: 'Boilerplate pollution', informational: true, defaultWeight: 0,
    description: 'Atoms with near-identical chunks (shared template text).' },
];

const DEFAULT_CONFIG: HealthConfig = { overrides: {} };

type Draft = Record<string, { enabled: boolean; weightStr: string }>;

function toDraft(config: HealthConfig): Draft {
  const out: Draft = {};
  for (const c of CHECKS) {
    const o = config.overrides[c.name];
    out[c.name] = {
      enabled: o?.enabled ?? true,
      weightStr: o?.weight !== undefined && o.weight !== null
        ? String(o.weight)
        : '',
    };
  }
  return out;
}

function fromDraft(draft: Draft): HealthConfig {
  const overrides: Record<string, HealthCheckOverride> = {};
  for (const c of CHECKS) {
    const d = draft[c.name];
    if (!d) continue;
    const explicitWeight = d.weightStr.trim() === '' ? null : Number(d.weightStr);
    const needsOverride = !d.enabled
      || (explicitWeight !== null && !Number.isNaN(explicitWeight));
    if (!needsOverride) continue;
    overrides[c.name] = {
      enabled: d.enabled,
      weight: explicitWeight !== null && !Number.isNaN(explicitWeight)
        ? explicitWeight
        : undefined,
    };
  }
  return { overrides };
}

export function HealthConfigTab({ onSaved }: { onSaved?: () => void } = {}) {
  const [draft, setDraft] = useState<Draft>(() => toDraft(DEFAULT_CONFIG));
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        const cfg = await getTransport().invoke<HealthConfig>('get_health_config', {});
        setDraft(toDraft(cfg ?? DEFAULT_CONFIG));
      } catch (err) {
        console.error('load health config', err);
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  const set = (name: string, patch: Partial<{ enabled: boolean; weightStr: string }>) => {
    setDraft(d => ({ ...d, [name]: { ...d[name], ...patch } }));
  };

  const save = async () => {
    setSaving(true);
    try {
      await getTransport().invoke('set_health_config', fromDraft(draft) as unknown as Record<string, unknown>);
      toast.success('Health config saved');
      onSaved?.();
    } catch (err) {
      const detail = err instanceof Error ? err.message : String(err);
      toast.error('Save health config failed', {
        detail,
        retry: () => { void save(); },
      });
    } finally {
      setSaving(false);
    }
  };

  const resetToDefaults = () => {
    setDraft(toDraft(DEFAULT_CONFIG));
  };

  if (loading) {
    return (
      <div className="flex items-center gap-2 text-xs text-gray-500 p-4">
        <Loader2 className="w-3.5 h-3.5 animate-spin" /> Loading config…
      </div>
    );
  }

  const totalEffectiveWeight = CHECKS.reduce((sum, c) => {
    const d = draft[c.name];
    if (!d || !d.enabled) return sum;
    const w = d.weightStr.trim() === '' ? (c.informational ? 0 : c.defaultWeight) : Number(d.weightStr);
    return sum + (Number.isFinite(w) ? w : 0);
  }, 0);

  return (
    <div className="space-y-4">
      <div className="text-xs text-gray-400 leading-relaxed max-w-prose">
        <p>
          Pick which checks run and how much each contributes to the overall score.
          Leave weight blank to use the default; blank weight on an <em>informational</em> check
          means it runs but does not affect the score. Weights are renormalized at scoring time,
          so they don't need to sum to 1.
        </p>
      </div>

      <div className="rounded border border-white/5 overflow-hidden">
        <table className="w-full text-xs">
          <thead className="bg-white/[0.02] text-gray-500">
            <tr>
              <th className="text-left px-3 py-2 font-normal">Check</th>
              <th className="text-left px-3 py-2 font-normal w-20">Enabled</th>
              <th className="text-left px-3 py-2 font-normal w-28" title="Leave blank for default">Weight</th>
              <th className="text-left px-3 py-2 font-normal w-16">Default</th>
            </tr>
          </thead>
          <tbody>
            {CHECKS.map(c => {
              const d = draft[c.name];
              return (
                <tr
                  key={c.name}
                  className="border-t border-white/5 hover:bg-white/[0.02]"
                >
                  <td className="px-3 py-2 align-top">
                    <div className="flex items-center gap-1.5">
                      <span className="text-gray-200">{c.label}</span>
                      {c.informational && (
                        <span
                          className="px-1.5 py-0.5 rounded text-[10px] bg-blue-500/15 text-blue-300 border border-blue-500/20"
                          title="Opinionated: default weight is 0. Assign a weight to include in scoring."
                        >
                          informational
                        </span>
                      )}
                    </div>
                    <div className="text-[11px] text-gray-600 mt-0.5">{c.description}</div>
                  </td>
                  <td className="px-3 py-2 align-top">
                    <input
                      type="checkbox"
                      checked={d.enabled}
                      onChange={e => set(c.name, { enabled: e.target.checked })}
                      className="w-3.5 h-3.5 rounded accent-purple-500"
                      aria-label={`Enable ${c.label}`}
                    />
                  </td>
                  <td className="px-3 py-2 align-top">
                    <input
                      type="text"
                      inputMode="decimal"
                      placeholder={c.informational ? '0.00' : c.defaultWeight.toFixed(2)}
                      value={d.weightStr}
                      onChange={e => set(c.name, { weightStr: e.target.value })}
                      disabled={!d.enabled}
                      className="w-20 bg-[#2a2a2a] border border-white/10 rounded px-2 py-1 text-gray-200 focus:outline-none focus:border-purple-500 disabled:opacity-40"
                      aria-label={`Weight for ${c.label}`}
                    />
                  </td>
                  <td className="px-3 py-2 align-top text-gray-600">
                    {c.defaultWeight.toFixed(2)}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <div className="flex items-center justify-between text-xs">
        <p className="text-gray-600" title="Weights are renormalized so this doesn't need to be exactly 1.">
          Total effective weight: <span className="text-gray-300 font-mono">{totalEffectiveWeight.toFixed(2)}</span>
        </p>
        <div className="flex items-center gap-2">
          <button
            onClick={resetToDefaults}
            className="flex items-center gap-1.5 px-3 py-1.5 text-xs text-gray-400 hover:text-gray-200 transition-colors rounded hover:bg-white/5"
          >
            <RotateCcw className="w-3 h-3" />
            Reset to defaults
          </button>
          <button
            onClick={save}
            disabled={saving}
            className="flex items-center gap-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 disabled:bg-[#3a3a3a] rounded text-xs text-white transition-colors"
          >
            {saving ? <Loader2 className="w-3 h-3 animate-spin" /> : <Save className="w-3 h-3" />}
            Save
          </button>
        </div>
      </div>

      <WikiExclusionPanel />
    </div>
  );
}
