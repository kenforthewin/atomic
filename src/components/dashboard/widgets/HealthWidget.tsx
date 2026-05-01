import { useEffect, useState, useCallback, useRef } from 'react';
import { getTransport } from '../../../lib/transport';
import {
  RefreshCw, CheckCircle, AlertTriangle, XCircle, Play, Download, HelpCircle,
} from 'lucide-react';
import { HealthReviewModal } from './HealthReviewModal';
import { HealthCheckRow, getTrend } from './HealthCheckRow';
import type { HealthCheckResult } from './HealthCheckRow';
import { HealthConfirmModal } from './HealthConfirmModal';
import type { PendingFix } from './HealthConfirmModal';
import { HealthExportModal } from './HealthExportModal';
import { HealthHelpOverlay } from './HealthHelpOverlay';

// ==================== Types ====================

interface HealthReport {
  overall_score: number;
  overall_status: 'healthy' | 'needs_attention' | 'degraded' | 'unhealthy';
  computed_at: string;
  atom_count: number;
  checks: Record<string, HealthCheckResult>;
  auto_fixable: number;
  requires_review: number;
  previous_score?: number;
  previous_check_scores?: Record<string, number>;
}

interface FixAction {
  id: string;
  check: string;
  action: string;
  count: number;
  details: string[];
}

interface FixResponse {
  mode: string;
  actions_taken: FixAction[];
  skipped: Array<{ check: string; reason: string; count: number }>;
  new_score: number;
}

// ==================== Config ====================

const CHECK_LABELS: Record<string, string> = {
  embedding_coverage: 'Embeddings',
  tagging_coverage: 'Tagging',
  source_uniqueness: 'Source dupes',
  orphan_tags: 'Orphan tags',
  semantic_graph_freshness: 'Semantic graph',
  wiki_coverage: 'Wiki coverage',
  content_quality: 'Content quality',
  tag_health: 'Tag health',
  duplicate_detection: 'Duplicates',
  content_overlap: 'Content overlap',
  contradiction_detection: 'Contradictions',
  boilerplate_pollution: 'Boilerplate',
  broken_internal_links: 'Broken links',
};

// One-line explanation shown under each failing check
const CHECK_DESCRIPTIONS: Record<string, (data: Record<string, unknown>) => string> = {
  embedding_coverage: (d) => {
    const failed = d.failed as number ?? 0;
    const pending = d.pending as number ?? 0;
    if (failed > 0) return `${failed} atom${failed !== 1 ? 's' : ''} failed to embed — semantic search can't find them`;
    if (pending > 0) return `${pending} atom${pending !== 1 ? 's' : ''} waiting to be embedded`;
    return 'All atoms are embedded';
  },
  tagging_coverage: (d) => {
    const untagged = (d.untagged_complete as number ?? 0) + (d.skipped_untagged as number ?? 0);
    const failed = d.failed as number ?? 0;
    if (untagged > 0) return `${untagged} atom${untagged !== 1 ? 's' : ''} went through the tagger but got zero tags assigned`;
    if (failed > 0) return `${failed} atom${failed !== 1 ? 's' : ''} failed tagging`;
    return 'All atoms are tagged';
  },
  source_uniqueness: (d) => {
    const count = d.count as number ?? 0;
    return `${count} source URL${count !== 1 ? 's' : ''} appear on more than one atom — likely an import bug`;
  },
  orphan_tags: (d) => {
    const count = (d.tags as unknown[])?.length ?? d.count as number ?? 0;
    return `${count} tag${count !== 1 ? 's' : ''} with no atoms and no children — clutter in the tag tree`;
  },
  semantic_graph_freshness: (d) => {
    const n = d.atoms_since_rebuild as number ?? 0;
    return `${n} atom${n !== 1 ? 's' : ''} added or updated since the similarity graph was last built`;
  },
  wiki_coverage: (d) => {
    const missing = d.without_wiki as number ?? 0;
    const stale = d.stale_wikis as number ?? 0;
    const parts = [];
    if (missing > 0) parts.push(`${missing} eligible tag${missing !== 1 ? 's' : ''} have no wiki`);
    if (stale > 0) parts.push(`${stale} wiki${stale !== 1 ? 's' : ''} are out of date`);
    return parts.join(', ');
  },
  content_quality: (d) => {
    const issues = d.issues as Record<string, { count: number }> | undefined;
    if (!issues) return 'Some atoms may need attention';
    const parts = [];
    if (issues.very_short?.count > 0) parts.push(`${issues.very_short.count} too short`);
    if (issues.very_long?.count > 0) parts.push(`${issues.very_long.count} too long`);
    if (issues.no_headings?.count > 0) parts.push(`${issues.no_headings.count} lack headings`);
    if (issues.no_source?.count > 0) parts.push(`${issues.no_source.count} have no source`);
    return parts.join(', ');
  },
  tag_health: (d) => {
    const parts = [];
    if ((d.single_atom_tags as number) > 3) parts.push(`${d.single_atom_tags} single-atom tags`);
    if ((d.rootless_tags as number) > 0) parts.push(`${d.rootless_tags} root-level tags may need nesting`);
    if ((d.similar_name_pairs as number) > 0) parts.push(`${d.similar_name_pairs} similar-name pairs`);
    return parts.join(', ') || 'Tag structure has issues';
  },
  content_overlap: (d) => {
    const overlaps = (d.cross_source_overlaps as number) ?? 0;
    const exact = (d.exact_duplicates as number) ?? 0;
    const templates = (d.template_clones as number) ?? 0;
    const parts = [];
    if (exact > 0) parts.push(`${exact} exact URL duplicate${exact !== 1 ? 's' : ''}`);
    if (templates > 0) parts.push(`${templates} template clone${templates !== 1 ? 's' : ''}`);
    if (overlaps > 0) parts.push(`${overlaps} cross-source overlap${overlaps !== 1 ? 's' : ''} need review`);
    return parts.join(', ') || 'No cross-source content overlap';
  },
  contradiction_detection: (d) => {
    const count = d.potential_contradictions as number ?? 0;
    return `${count} atom pair${count !== 1 ? 's' : ''} on the same topic with differing content`;
  },
  boilerplate_pollution: (d) => {
    const count = d.count as number ?? 0;
    return `${count} atom${count !== 1 ? 's' : ''} have near-identical semantic edges — their embeddings can’t be distinguished in search. Usually caused by shared template structure in the content.`;
  },
  broken_internal_links: (d) => {
    const n = (d.broken_count as number) ?? 0;
    const atoms = (d.affected_atoms as number) ?? 0;
    return `${n} link${n !== 1 ? 's' : ''} in ${atoms} atom${atoms !== 1 ? 's' : ''} point to other vault documents but resolve to no atom`;
  },
};

// Human-readable description of each fix_action value
const FIX_ACTION_LABELS: Record<string, string> = {
  retry_failed_and_process_pending: 'Retry failed embeddings',
  retry_tagging_pipeline: 'Retry failed tagging',
  reset_skipped_untagged_to_pending: 'Re-tag atoms skipped during import',
  delete_orphan_tags: 'Delete unused tags',
  rebuild_semantic_edges: 'Rebuild semantic graph',
  generate_missing_wikis: 'Generate missing wiki articles',
  merge_exact_source_duplicates: 'Merge exact-URL duplicates',
  resolve_internal_links: 'Resolve internal document links to atom URIs',
};

const STATUS_COLORS = {
  healthy: 'text-green-400',
  needs_attention: 'text-yellow-400',
  degraded: 'text-orange-400',
  unhealthy: 'text-red-400',
};

const CHECK_ORDER = [
  'embedding_coverage',
  'tagging_coverage',
  'source_uniqueness',
  'orphan_tags',
  'semantic_graph_freshness',
  'wiki_coverage',
  'content_quality',
  'tag_health',
  'content_overlap',
  'contradiction_detection',
  'broken_internal_links',
  'boilerplate_pollution',
];

// ==================== Sub-components ====================

function ScoreBar({ score }: { score: number }) {
  const color =
    score >= 90 ? 'bg-green-500' :
    score >= 70 ? 'bg-yellow-500' :
    score >= 50 ? 'bg-orange-500' : 'bg-red-500';
  return (
    <div className="w-full bg-[#3a3a3a] rounded-full h-1.5">
      <div className={`${color} h-1.5 rounded-full transition-all duration-500`} style={{ width: `${score}%` }} />
    </div>
  );
}

// ==================== Pending actions preview ====================

function pendingActions(report: HealthReport, excluded: Set<string>): { label: string; check: string }[] {
  const actions: { label: string; check: string }[] = [];
  for (const key of CHECK_ORDER) {
    const check = report.checks[key];
    if (!check || check.status === 'ok' || !check.auto_fixable) continue;
    if (excluded.has(key)) continue;
    const label = check.fix_action
      ? (FIX_ACTION_LABELS[check.fix_action] ?? check.fix_action.replace(/_/g, ' '))
      : `Fix ${CHECK_LABELS[key] ?? key}`;
    actions.push({ label, check: key });
  }
  return actions;
}

function extractCount(check: HealthCheckResult): number {
  const d = check.data;
  if (typeof d?.count === 'number') return d.count as number;
  if (Array.isArray(d?.pairs)) return (d.pairs as unknown[]).length;
  if (Array.isArray(d?.affected_atoms)) return (d.affected_atoms as unknown[]).length;
  if (d?.issues) {
    const issues = d.issues as Record<string, { count?: number }>;
    return Object.values(issues).reduce((n, v) => n + (v?.count ?? 0), 0);
  }
  if (typeof d?.rootless_tags === 'number') return d.rootless_tags as number;
  return 0;
}

function reviewItems(report: HealthReport): { label: string; count: number }[] {
  const items: { label: string; count: number }[] = [];
  for (const key of CHECK_ORDER) {
    const check = report.checks[key];
    if (!check || !check.requires_review) continue;
    const count = extractCount(check);
    if (count === 0) continue;
    items.push({ label: CHECK_LABELS[key] ?? key, count });
  }
  return items;
}
// ==================== Phase 2: Filters, sorts, severity ====================

type SeverityFilter = 'all' | 'critical' | 'warning' | 'needs-attention' | 'healthy';
type FixableFilter = 'all' | 'fixable' | 'manual-only';
type SortOrder = 'score-asc' | 'score-desc' | 'alphabetical' | 'affected-count';

interface FilterState {
  severity: SeverityFilter;
  fixable: FixableFilter;
  sort: SortOrder;
}

const DEFAULT_FILTER: FilterState = {
  severity: 'all',
  fixable: 'all',
  sort: 'score-asc',
};

function getSeverityBadge(score: number): string {
  if (score <= 40) return '🔴';
  if (score <= 70) return '🟠';
  if (score <= 85) return '🟡';
  return '🟢';
}

function getVisibleChecks(
  report: HealthReport,
  filter: FilterState,
): string[] {
  let visible = CHECK_ORDER.filter(k => {
    const check = report.checks[k];
    if (!check || check.status === 'ok') return false;

    if (filter.severity !== 'all') {
      const score = check.score;
      const sev =
        score <= 40 ? 'critical' :
        score <= 70 ? 'warning' :
        score <= 85 ? 'needs-attention' : 'healthy';
      if (sev !== filter.severity) return false;
    }

    if (filter.fixable === 'fixable' && !check.auto_fixable) return false;
    if (filter.fixable === 'manual-only' && check.auto_fixable) return false;

    return true;
  });

  switch (filter.sort) {
    case 'score-asc':
      visible.sort((a, b) => (report.checks[a]?.score ?? 0) - (report.checks[b]?.score ?? 0));
      break;
    case 'score-desc':
      visible.sort((a, b) => (report.checks[b]?.score ?? 0) - (report.checks[a]?.score ?? 0));
      break;
    case 'alphabetical':
      visible.sort((a, b) => (CHECK_LABELS[a] ?? a).localeCompare(CHECK_LABELS[b] ?? b));
      break;
    case 'affected-count':
      visible.sort((a, b) => {
        const ca = report.checks[a] ? extractCount(report.checks[a]) : 0;
        const cb = report.checks[b] ? extractCount(report.checks[b]) : 0;
        return cb - ca;
      });
      break;
  }

  return visible;
}

// ==================== Main component ====================

export function HealthPanel() {
  const [report, setReport] = useState<HealthReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [fixing, setFixing] = useState(false);
  const [lastFix, setLastFix] = useState<FixResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showPending, setShowPending] = useState(false);
  const [showConfirm, setShowConfirm] = useState(false);
  const [showExport, setShowExport] = useState(false);
  const [showHelp, setShowHelp] = useState(false);
  const [undoToast, setUndoToast] = useState<{ fixIds: string[]; label: string } | null>(null);
  const undoTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Per-row state
  const [expandedChecks, setExpandedChecks] = useState<Set<string>>(new Set());
  const [runningCheck, setRunningCheck] = useState<string | null>(null);
  const [showReviewModal, setShowReviewModal] = useState<string | null>(null);
  // Checks excluded from the batch fix
  const [excludedFromFix, setExcludedFromFix] = useState<Set<string>>(new Set());
  const [filter, setFilter] = useState<FilterState>(DEFAULT_FILTER);
  const fetchHealth = useCallback(async () => {
    try {
      setError(null);
      const data = await getTransport().invoke<HealthReport>('get_health_knowledge', {});
      setReport(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load health data');
    } finally {
      setLoading(false);
    }
  }, []);

  const fetchHealthDebouncedRef = useRef<number | null>(null);
  const scheduleRefetch = useCallback(() => {
    if (fetchHealthDebouncedRef.current) {
      window.clearTimeout(fetchHealthDebouncedRef.current);
    }
    fetchHealthDebouncedRef.current = window.setTimeout(() => {
      fetchHealth();
      fetchHealthDebouncedRef.current = null;
    }, 2000);
  }, [fetchHealth]);

  useEffect(() => () => {
    if (fetchHealthDebouncedRef.current) window.clearTimeout(fetchHealthDebouncedRef.current);
  }, []);

  useEffect(() => { fetchHealth(); }, [fetchHealth]);

  const toggleExpandCheck = useCallback((checkName: string) => {
    setExpandedChecks(prev => {
      const next = new Set(prev);
      if (next.has(checkName)) next.delete(checkName);
      else next.add(checkName);
      return next;
    });
  }, []);

  const toggleIncludeInFix = useCallback((checkName: string) => {
    setExcludedFromFix(prev => {
      const next = new Set(prev);
      if (next.has(checkName)) next.delete(checkName);
      else next.add(checkName);
      return next;
    });
  }, []);

  const runSingleCheck = useCallback(async (checkName: string) => {
    setRunningCheck(checkName);
    try {
      const result = await getTransport().invoke<HealthCheckResult>(
        'health_check_single',
        { check_name: checkName },
      );
      setReport(prev => {
        if (!prev) return prev;
        return { ...prev, checks: { ...prev.checks, [checkName]: result } };
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Check failed');
    } finally {
      setRunningCheck(null);
    }
  }, []);

  const runFix = () => setShowConfirm(true);

  const applyFix = async () => {
    setShowConfirm(false);
    setFixing(true);
    setShowPending(false);
    if (undoTimerRef.current) clearTimeout(undoTimerRef.current);
    setUndoToast(null);
    try {
      const checksToFix = report
        ? CHECK_ORDER.filter(k => {
            const c = report.checks[k];
            return c && c.status !== 'ok' && c.auto_fixable && !excludedFromFix.has(k);
          })
        : undefined;
      const resp = await getTransport().invoke<FixResponse>('run_health_fix', {
        mode: 'auto',
        include_medium: false,
        checks: checksToFix,
      });
      setLastFix(resp);
      if (resp.actions_taken.length > 0) {
        const fixIds = resp.actions_taken.map(a => a.id).filter(Boolean);
        const label = `Fixed ${resp.actions_taken.reduce((n, a) => n + a.count, 0)} items. Score → ${resp.new_score}/100`;
        setUndoToast({ fixIds, label });
        undoTimerRef.current = setTimeout(() => setUndoToast(null), 10_000);
      }
      await fetchHealth();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Fix failed');
    } finally {
      setFixing(false);
    }
  };

  const undoLastFix = async () => {
    if (!undoToast) return;
    if (undoTimerRef.current) clearTimeout(undoTimerRef.current);
    setUndoToast(null);
    try {
      for (const fixId of [...undoToast.fixIds].reverse()) {
        await getTransport().invoke('undo_health_fix', { fixId });
      }
      await fetchHealth();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Undo failed');
    }
  };

  // Compute these before early returns so keyboard handler can reference them
  const issueChecks = report ? getVisibleChecks(report, filter) : [];
  const pending: PendingFix[] = report ? pendingActions(report, excludedFromFix) : [];
  const review = report ? reviewItems(report) : [];

  // Keyboard shortcuts
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement).tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
      if (showConfirm || showExport || showHelp || showReviewModal) return;
      if (e.key === 'r') {
        e.preventDefault();
        fetchHealth();
      } else if (e.key === 'f' && report && pending.length > 0) {
        e.preventDefault();
        setShowConfirm(true);
      } else if (e.key === 'e' && report) {
        e.preventDefault();
        setShowExport(true);
      } else if (e.key === '?') {
        e.preventDefault();
        setShowHelp(v => !v);
      } else if (e.key >= '1' && e.key <= '9' && issueChecks.length > 0) {
        const idx = parseInt(e.key, 10) - 1;
        const checkName = issueChecks[idx];
        if (checkName) toggleExpandCheck(checkName);
      }
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [fetchHealth, report, pending, showConfirm, showExport, showHelp, showReviewModal, issueChecks, toggleExpandCheck]);

  if (loading) {
    return (
      <div className="p-4 bg-[#252525] rounded border border-white/5 flex items-center justify-center h-32">
        <RefreshCw className="w-4 h-4 text-gray-500 animate-spin" />
      </div>
    );
  }

  if (error || !report) {
    return (
      <div className="p-4 bg-[#252525] rounded border border-white/5">
        <div className="flex items-center gap-2 text-red-400 text-sm">
          <XCircle className="w-4 h-4 shrink-0" />
          <span>{error ?? 'No data'}</span>
        </div>
      </div>
    );
  }

  const statusColor = STATUS_COLORS[report.overall_status] ?? 'text-gray-400';


  return (
    <div className="p-4 bg-[#252525] rounded border border-white/5 space-y-3">

      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <h3 className="text-sm font-semibold text-white">Knowledge Health</h3>
          <button
            onClick={fetchHealth}
            className="text-gray-500 hover:text-gray-300 transition-colors"
            title="Refresh all checks"
            aria-label="Refresh health checks"
          >
            <RefreshCw className="w-3.5 h-3.5" />
          </button>
          <button
            onClick={() => setShowExport(true)}
            className="text-gray-500 hover:text-gray-300 transition-colors"
            title="Export health report (e)"
            aria-label="Export health report"
          >
            <Download className="w-3.5 h-3.5" />
          </button>
          <button
            onClick={() => setShowHelp(true)}
            className="text-gray-500 hover:text-gray-300 transition-colors"
            title="Keyboard shortcuts (?)"
            aria-label="Show keyboard shortcuts"
          >
            <HelpCircle className="w-3.5 h-3.5" />
          </button>
        </div>
        <div className="text-right">
          <div className="flex items-center gap-1 justify-end">
            {report.previous_score !== undefined && (
              <span className={`text-sm ${
                getTrend(report.overall_score, report.previous_score) === '↑' ? 'text-green-400' :
                getTrend(report.overall_score, report.previous_score) === '↓' ? 'text-red-400' :
                'text-gray-600'
              }`}>
                {getTrend(report.overall_score, report.previous_score)}
              </span>
            )}
            <span className={`text-2xl font-bold ${statusColor}`}>{report.overall_score}</span>
            <span className="text-gray-500 text-sm">/100</span>
          </div>
        </div>
      </div>

      <ScoreBar score={report.overall_score} />

      {/* Per-check rows */}
      {CHECK_ORDER.some(k => report.checks[k]?.status !== 'ok') && (
        <div className="flex items-center gap-2 flex-wrap">
          <select
            value={filter.severity}
            onChange={e => setFilter(f => ({ ...f, severity: e.target.value as SeverityFilter }))}
            className="text-xs bg-[#2a2a2a] border border-white/10 rounded px-2 py-1 text-gray-400 focus:outline-none focus:border-purple-500"
            aria-label="Filter by severity"
          >
            <option value="all">All severity</option>
            <option value="critical">🔴 Critical</option>
            <option value="warning">🟠 Warning</option>
            <option value="needs-attention">🟡 Needs attention</option>
            <option value="healthy">🟢 Healthy</option>
          </select>
          <select
            value={filter.fixable}
            onChange={e => setFilter(f => ({ ...f, fixable: e.target.value as FixableFilter }))}
            className="text-xs bg-[#2a2a2a] border border-white/10 rounded px-2 py-1 text-gray-400 focus:outline-none focus:border-purple-500"
            aria-label="Filter by auto-fixable"
          >
            <option value="all">All types</option>
            <option value="fixable">Auto-fixable</option>
            <option value="manual-only">Manual only</option>
          </select>
          <select
            value={filter.sort}
            onChange={e => setFilter(f => ({ ...f, sort: e.target.value as SortOrder }))}
            className="text-xs bg-[#2a2a2a] border border-white/10 rounded px-2 py-1 text-gray-400 focus:outline-none focus:border-purple-500"
            aria-label="Sort checks"
          >
            <option value="score-asc">Worst first</option>
            <option value="score-desc">Best first</option>
            <option value="alphabetical">A–Z</option>
            <option value="affected-count">Most affected</option>
          </select>
          {(filter.severity !== 'all' || filter.fixable !== 'all' || filter.sort !== 'score-asc') && (
            <button
              onClick={() => setFilter(DEFAULT_FILTER)}
              className="text-xs text-gray-600 hover:text-gray-400 transition-colors"
            >
              Clear
            </button>
          )}
        </div>
      )}
      {/* Per-check rows */}
      {issueChecks.length > 0 ? (
        <div className="divide-y divide-white/5">
          {issueChecks.map(key => {
            const check = report.checks[key];
            if (!check) return null;
            const desc = CHECK_DESCRIPTIONS[key]?.(check.data) ?? '';
            return (
              <HealthCheckRow
                key={key}
                checkName={key}
                check={check}
                label={CHECK_LABELS[key] ?? key}
                description={desc}
                isExpanded={expandedChecks.has(key)}
                onToggleExpand={toggleExpandCheck}
                onRun={runSingleCheck}
                onReview={(name) => setShowReviewModal(name)}
                isRunning={runningCheck === key}
                includeInFix={!excludedFromFix.has(key)}
                onToggleInclude={toggleIncludeInFix}
                trend={getTrend(check.score, report.previous_check_scores?.[key])}
                severityBadge={getSeverityBadge(check.score)}
              />
            );
          })}
        </div>
      ) : (
        <div className="flex items-center gap-2 text-green-400 text-xs">
          <CheckCircle className="w-3.5 h-3.5" />
          <span>All checks passing</span>
        </div>
      )}

      {/* Actions */}
      {(pending.length > 0 || review.length > 0) && (
        <div className="space-y-2 pt-1 border-t border-white/5">

          {/* Auto-fix */}
          {pending.length > 0 && (
            <div>
              <div className="flex items-center justify-between">
                <button
                  onClick={runFix}
                  disabled={fixing}
                  className="flex items-center gap-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 disabled:opacity-50 disabled:cursor-not-allowed rounded text-xs text-white transition-colors"
                >
                  <Play className="w-3 h-3" />
                  {fixing ? 'Running fixes…' : `Apply ${pending.length} automatic fix${pending.length > 1 ? 'es' : ''}`}
                </button>
                <button
                  onClick={() => setShowPending(v => !v)}
                  className="text-xs text-gray-500 hover:text-gray-300 transition-colors"
                >
                  {showPending ? 'Hide' : 'What will this do?'}
                </button>
              </div>
              {showPending && (
                <ul className="mt-2 space-y-1 pl-1">
                  {pending.map((a, i) => (
                    <li key={i} className="flex items-start gap-1.5 text-xs text-gray-400">
                      <span className="text-purple-400 mt-0.5">•</span>
                      {a.label}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          )}

          {/* Needs review */}
          {review.length > 0 && (
            <button
              onClick={() => setShowReviewModal(issueChecks.find(k => report.checks[k]?.requires_review) ?? null)}
              className="flex items-center gap-1.5 text-xs text-yellow-500 hover:text-yellow-300 transition-colors"
            >
              <AlertTriangle className="w-3 h-3" />
              {review.reduce((n, r) => n + r.count, 0)} item{review.reduce((n, r) => n + r.count, 0) !== 1 ? 's' : ''} need manual review →
            </button>
          )}
        </div>
      )}

      {/* Last fix result — score summary only */}
      {lastFix && lastFix.actions_taken.length > 0 && (
        <div className="border-t border-white/5 pt-2">
          <p className="text-xs text-gray-500">Last run → score {lastFix.new_score}/100</p>
        </div>
      )}

      {/* Undo toast */}
      {undoToast && (
        <div className="fixed bottom-4 left-1/2 -translate-x-1/2 z-50 flex items-center gap-3 px-4 py-3 bg-[#2d2d2d] border border-white/10 rounded-lg shadow-xl text-xs text-gray-300 animate-in slide-in-from-bottom-2 duration-200">
          <span>{undoToast.label}</span>
          <button
            onClick={undoLastFix}
            className="px-2 py-1 bg-[#3a3a3a] hover:bg-[#444] rounded text-white transition-colors font-medium"
          >
            Undo
          </button>
          <button
            onClick={() => {
              if (undoTimerRef.current) clearTimeout(undoTimerRef.current);
              setUndoToast(null);
            }}
            className="text-gray-500 hover:text-gray-300 transition-colors"
            aria-label="Dismiss"
          >
            ×
          </button>
        </div>
      )}

      {/* Modals */}
      {showConfirm && report && (
        <HealthConfirmModal
          pending={pending}
          currentScore={report.overall_score}
          onConfirm={applyFix}
          onCancel={() => setShowConfirm(false)}
        />
      )}
      {showExport && report && (
        <HealthExportModal
          report={report}
          onClose={() => setShowExport(false)}
        />
      )}
      {showHelp && (
        <HealthHelpOverlay onClose={() => setShowHelp(false)} />
      )}

      {/* Review modal */}
      {showReviewModal && report && (
        <HealthReviewModal
          report={report}
          checkName={showReviewModal}
          onClose={() => {
            setShowReviewModal(null);
            if (fetchHealthDebouncedRef.current) {
              window.clearTimeout(fetchHealthDebouncedRef.current);
              fetchHealthDebouncedRef.current = null;
            }
            fetchHealth();
          }}
          onResolved={scheduleRefetch}
        />
      )}

    </div>
  );
}