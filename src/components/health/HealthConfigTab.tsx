import { useEffect, useRef, useState } from 'react';
import { Loader2, RotateCcw, Check, AlertCircle } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import { toast } from '../../stores/toasts';
import { WikiExclusionPanel } from './WikiExclusionPanel';

// ---- Shape mirrors crates/atomic-core/src/health/mod.rs::HealthConfig ----

interface HealthCheckOverride {
  enabled: boolean;
  weight?: number | null;
}

interface HealthThresholds {
  boilerplate_similarity: number;
  boilerplate_min_clones: number;
  contradiction_similarity_min: number;
  contradiction_similarity_max: number;
  contradiction_shared_tags_min: number;
  content_overlap_similarity_min: number;
  content_overlap_similarity_max: number;
  content_overlap_shared_tags_min: number;
  content_quality_short_chars: number;
  content_quality_long_chars: number;
  wiki_min_atoms_per_tag: number;
  tag_health_single_atom_threshold: number;
  semantic_graph_freshness_warning: number;
}

const DEFAULT_THRESHOLDS: HealthThresholds = {
  boilerplate_similarity: 0.99,
  boilerplate_min_clones: 2,
  contradiction_similarity_min: 0.80,
  contradiction_similarity_max: 0.92,
  contradiction_shared_tags_min: 1,
  content_overlap_similarity_min: 0.55,
  content_overlap_similarity_max: 0.85,
  content_overlap_shared_tags_min: 2,
  content_quality_short_chars: 100,
  content_quality_long_chars: 15_000,
  wiki_min_atoms_per_tag: 5,
  tag_health_single_atom_threshold: 3,
  semantic_graph_freshness_warning: 20,
};

interface HealthConfig {
  overrides: Record<string, HealthCheckOverride>;
  thresholds?: HealthThresholds;
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

const DEFAULT_CONFIG: HealthConfig = { overrides: {}, thresholds: DEFAULT_THRESHOLDS };

type ChecksDraft = Record<string, { enabled: boolean; weightStr: string }>;
type ThresholdsDraft = Record<keyof HealthThresholds, string>;

interface Draft {
  checks: ChecksDraft;
  thresholds: ThresholdsDraft;
}

type SaveStatus =
  | { kind: 'idle' }
  | { kind: 'saving' }
  | { kind: 'saved'; at: number }
  | { kind: 'error'; message: string };

function toChecksDraft(config: HealthConfig): ChecksDraft {
  const out: ChecksDraft = {};
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

function toThresholdsDraft(t?: HealthThresholds): ThresholdsDraft {
  const src = t ?? DEFAULT_THRESHOLDS;
  const out: Partial<ThresholdsDraft> = {};
  (Object.keys(DEFAULT_THRESHOLDS) as Array<keyof HealthThresholds>).forEach(k => {
    const v = src[k];
    out[k] = v !== undefined && v !== null ? String(v) : String(DEFAULT_THRESHOLDS[k]);
  });
  return out as ThresholdsDraft;
}

function toDraft(config: HealthConfig): Draft {
  return {
    checks: toChecksDraft(config),
    thresholds: toThresholdsDraft(config.thresholds),
  };
}

function fromDraft(draft: Draft): HealthConfig {
  const overrides: Record<string, HealthCheckOverride> = {};
  for (const c of CHECKS) {
    const d = draft.checks[c.name];
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

  // Build thresholds: blank string → fallback to default.
  const thresholds: Partial<HealthThresholds> = {};
  (Object.keys(DEFAULT_THRESHOLDS) as Array<keyof HealthThresholds>).forEach(k => {
    const raw = draft.thresholds[k];
    if (raw === undefined || raw.trim() === '') {
      thresholds[k] = DEFAULT_THRESHOLDS[k];
      return;
    }
    const parsed = Number(raw);
    thresholds[k] = Number.isFinite(parsed) ? parsed : DEFAULT_THRESHOLDS[k];
  });
  return { overrides, thresholds: thresholds as HealthThresholds };
}

export function HealthConfigTab({ onSaved }: { onSaved?: () => void } = {}) {
  const [draft, setDraft] = useState<Draft>(() => toDraft(DEFAULT_CONFIG));
  const [loading, setLoading] = useState(true);
  const [saveStatus, setSaveStatus] = useState<SaveStatus>({ kind: 'idle' });

  // Latest draft to persist. Ref because the debounce closure must read the
  // freshest value, not whatever was captured when the timer was scheduled.
  const latestDraftRef = useRef<Draft | null>(null);
  // Tracks whether the component has finished its initial load — we must not
  // autosave the draft that came straight from the server.
  const initializedRef = useRef(false);
  // Last successfully-saved JSON payload, used to suppress redundant saves.
  const lastSavedRef = useRef<string>('');

  useEffect(() => {
    void (async () => {
      try {
        const cfg = await getTransport().invoke<HealthConfig>('get_health_config', {});
        const resolved = cfg ?? DEFAULT_CONFIG;
        setDraft(toDraft(resolved));
        lastSavedRef.current = JSON.stringify(resolved);
      } catch (err) {
        console.error('load health config', err);
      } finally {
        setLoading(false);
        initializedRef.current = true;
      }
    })();
  }, []);

  const set = (name: string, patch: Partial<{ enabled: boolean; weightStr: string }>) => {
    setDraft(d => ({
      ...d,
      checks: { ...d.checks, [name]: { ...d.checks[name], ...patch } },
    }));
  };

  const setThreshold = (key: keyof HealthThresholds, value: string) => {
    setDraft(d => ({ ...d, thresholds: { ...d.thresholds, [key]: value } }));
  };

  const resetThreshold = (key: keyof HealthThresholds) => {
    setDraft(d => ({
      ...d,
      thresholds: { ...d.thresholds, [key]: String(DEFAULT_THRESHOLDS[key]) },
    }));
  };

  // ---- Autosave ----
  //
  // Debounce 600ms after the last edit, then persist if the serialized config
  // actually changed since the last successful save. Errors surface both
  // inline (status pill) and as a toast with a retry action. A cheap
  // dedupe-on-stringify avoids re-saving when edits cancel out.
  const persist = async (payload: HealthConfig) => {
    const serialized = JSON.stringify(payload);
    if (serialized === lastSavedRef.current) return;
    setSaveStatus({ kind: 'saving' });
    try {
      await getTransport().invoke('set_health_config', payload as unknown as Record<string, unknown>);
      lastSavedRef.current = serialized;
      setSaveStatus({ kind: 'saved', at: Date.now() });
      onSaved?.();
    } catch (err) {
      const detail = err instanceof Error ? err.message : String(err);
      setSaveStatus({ kind: 'error', message: detail });
      toast.error('Autosave failed', {
        detail,
        retry: () => { void persist(payload); },
      });
    }
  };

  useEffect(() => {
    latestDraftRef.current = draft;
    if (!initializedRef.current) return;
    const handle = window.setTimeout(() => {
      const d = latestDraftRef.current;
      if (!d) return;
      const payload = fromDraft(d);
      console.debug('[health-config] autosave tick', { payload, lastSaved: lastSavedRef.current });
      void persist(payload);
    }, 600);
    return () => { window.clearTimeout(handle); };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draft]);

  // Fade the "Saved" pill back to idle after a short delay so it stays out of
  // the way during a normal editing session.
  useEffect(() => {
    if (saveStatus.kind !== 'saved') return;
    const handle = window.setTimeout(() => {
      setSaveStatus(s => (s.kind === 'saved' ? { kind: 'idle' } : s));
    }, 1500);
    return () => { window.clearTimeout(handle); };
  }, [saveStatus]);

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
    const d = draft.checks[c.name];
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
              const d = draft.checks[c.name];
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

      <div className="flex items-center justify-between gap-2 text-xs">
        <p className="text-gray-600" title="Weights are renormalized so this doesn't need to be exactly 1.">
          Total effective weight: <span className="text-gray-300 font-mono">{totalEffectiveWeight.toFixed(2)}</span>
        </p>
        <div className="flex items-center gap-2">
          <SaveStatusPill status={saveStatus} />
          <button
            onClick={resetToDefaults}
            className="flex items-center gap-1.5 px-3 py-1.5 text-xs text-gray-400 hover:text-gray-200 transition-colors rounded hover:bg-white/5"
            title="Revert checks and thresholds to built-in defaults. Autosaves."
          >
            <RotateCcw className="w-3 h-3" />
            Reset to defaults
          </button>
        </div>
      </div>

      <ThresholdsPanel draft={draft.thresholds} set={setThreshold} reset={resetThreshold} />

      <WikiExclusionPanel />
    </div>
  );
}

// ==================== Thresholds panel ====================

interface ThresholdSpec {
  key: keyof HealthThresholds;
  label: string;
  description: string;
  step?: string;
  min?: number;
  max?: number;
  group: string;
}

const THRESHOLD_SPECS: ThresholdSpec[] = [
  // ---- Boilerplate pollution ----
  { key: 'boilerplate_similarity', label: 'Similarity', group: 'Boilerplate pollution',
    description: 'Edges at/above this similarity are treated as template clones.',
    step: '0.01', min: 0, max: 1 },
  { key: 'boilerplate_min_clones', label: 'Min clone edges', group: 'Boilerplate pollution',
    description: 'Minimum clone-edge count before an atom is flagged.',
    step: '1', min: 1 },
  // ---- Contradiction detection ----
  { key: 'contradiction_similarity_min', label: 'Similarity min (≥)', group: 'Contradiction detection',
    description: 'Lower bound (inclusive) of the contradiction similarity window.',
    step: '0.01', min: 0, max: 1 },
  { key: 'contradiction_similarity_max', label: 'Similarity max (<)', group: 'Contradiction detection',
    description: 'Upper bound (exclusive) of the contradiction similarity window.',
    step: '0.01', min: 0, max: 1 },
  { key: 'contradiction_shared_tags_min', label: 'Min shared tags', group: 'Contradiction detection',
    description: 'Minimum shared-tag count for a pair to surface.',
    step: '1', min: 0 },
  // ---- Content overlap ----
  { key: 'content_overlap_similarity_min', label: 'Similarity min', group: 'Content overlap',
    description: 'Lower bound (inclusive) of the cross-source overlap window.',
    step: '0.01', min: 0, max: 1 },
  { key: 'content_overlap_similarity_max', label: 'Similarity max', group: 'Content overlap',
    description: 'Upper bound (inclusive) of the cross-source overlap window.',
    step: '0.01', min: 0, max: 1 },
  { key: 'content_overlap_shared_tags_min', label: 'Min shared tags', group: 'Content overlap',
    description: 'Minimum shared-tag count for a pair to surface.',
    step: '1', min: 0 },
  // ---- Content quality ----
  { key: 'content_quality_short_chars', label: 'Very-short (chars)', group: 'Content quality',
    description: 'Atoms shorter than this are flagged.', step: '10', min: 0 },
  { key: 'content_quality_long_chars', label: 'Very-long (chars)', group: 'Content quality',
    description: 'Atoms longer than this are flagged.', step: '100', min: 0 },
  // ---- Wiki ----
  { key: 'wiki_min_atoms_per_tag', label: 'Min atoms per wiki-eligible tag', group: 'Wiki coverage',
    description: 'Tags below this atom count are not considered wiki-eligible.',
    step: '1', min: 1 },
  // ---- Tag health ----
  { key: 'tag_health_single_atom_threshold', label: 'Single-atom tag allowance', group: 'Tag health',
    description: 'Max autotag single-atom tags before the check penalises.',
    step: '1', min: 0 },
  // ---- Semantic graph freshness ----
  { key: 'semantic_graph_freshness_warning', label: 'Warning window (atoms since rebuild)',
    group: 'Semantic graph freshness',
    description: 'Atoms added since last rebuild before status escalates from warning to error.',
    step: '1', min: 0 },
];

function ThresholdsPanel({
  draft,
  set,
  reset,
}: {
  draft: ThresholdsDraft;
  set: (key: keyof HealthThresholds, value: string) => void;
  reset: (key: keyof HealthThresholds) => void;
}) {
  // Group by `group`, preserving spec order.
  const groups: Array<{ group: string; items: ThresholdSpec[] }> = [];
  for (const spec of THRESHOLD_SPECS) {
    const last = groups[groups.length - 1];
    if (last && last.group === spec.group) {
      last.items.push(spec);
    } else {
      groups.push({ group: spec.group, items: [spec] });
    }
  }

  return (
    <div className="space-y-4 border-t border-white/5 pt-4">
      <div>
        <h3 className="text-xs font-medium text-gray-300 mb-1">Detection thresholds</h3>
        <p className="text-[11px] text-gray-500 max-w-prose">
          Tune when each check fires. Leave a field blank to fall back to the built-in default
          (shown as a placeholder). Saved per-database.
        </p>
      </div>
      {groups.map(({ group, items }) => (
        <div key={group} className="rounded border border-white/5">
          <div className="px-3 py-2 text-[11px] uppercase tracking-wide text-gray-500 bg-white/[0.02] border-b border-white/5">
            {group}
          </div>
          <div className="divide-y divide-white/5">
            {items.map(spec => {
              const defaultValue = DEFAULT_THRESHOLDS[spec.key];
              const current = draft[spec.key] ?? '';
              const isDefault = current === '' || Number(current) === defaultValue;
              return (
                <div key={spec.key} className="grid grid-cols-[1fr_auto] gap-3 px-3 py-2 items-start">
                  <div className="min-w-0">
                    <div className="text-xs text-gray-200">{spec.label}</div>
                    <div className="text-[11px] text-gray-500 mt-0.5">{spec.description}</div>
                  </div>
                  <div className="flex items-center gap-2">
                    <input
                      type="number"
                      inputMode="decimal"
                      step={spec.step ?? 'any'}
                      min={spec.min}
                      max={spec.max}
                      placeholder={String(defaultValue)}
                      value={current}
                      onChange={e => set(spec.key, e.target.value)}
                      className="w-24 bg-[#2a2a2a] border border-white/10 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-purple-500"
                      aria-label={spec.label}
                    />
                    <button
                      type="button"
                      onClick={() => reset(spec.key)}
                      disabled={isDefault}
                      title="Reset to default"
                      className="p-1 text-gray-500 hover:text-gray-200 disabled:opacity-30 disabled:hover:text-gray-500"
                    >
                      <RotateCcw className="w-3 h-3" />
                    </button>
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      ))}
    </div>
  );
}

// ==================== Save status pill ====================

function SaveStatusPill({ status }: { status: SaveStatus }) {
  // Idle: render a blank placeholder of the same size so the row doesn't
  // jitter when autosave kicks in.
  if (status.kind === 'idle') {
    return <span className="text-[11px] text-gray-600 select-none">Autosaves</span>;
  }
  if (status.kind === 'saving') {
    return (
      <span className="flex items-center gap-1.5 text-[11px] text-gray-400">
        <Loader2 className="w-3 h-3 animate-spin" />
        Saving…
      </span>
    );
  }
  if (status.kind === 'saved') {
    return (
      <span className="flex items-center gap-1.5 text-[11px] text-emerald-400">
        <Check className="w-3 h-3" />
        Saved
      </span>
    );
  }
  return (
    <span
      className="flex items-center gap-1.5 text-[11px] text-red-400"
      title={status.message}
    >
      <AlertCircle className="w-3 h-3" />
      Save failed
    </span>
  );
}
