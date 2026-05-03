import { useEffect, useState, useCallback, useRef } from 'react';
import { getTransport } from '../../lib/transport';
import { runReviewAction } from '../dashboard/widgets/review/reviewActions';
import {
  RefreshCw, CheckCircle, AlertTriangle, XCircle, Play, Download, HelpCircle,
} from 'lucide-react';
import { HealthReviewModal } from '../dashboard/widgets/HealthReviewModal';
import { HealthCheckRow, getTrend } from '../dashboard/widgets/HealthCheckRow';
import type { HealthCheckResult } from '../dashboard/widgets/HealthCheckRow';
import { HealthConfirmModal } from '../dashboard/widgets/HealthConfirmModal';
import type { PendingFix } from '../dashboard/widgets/HealthConfirmModal';
import { HealthExportModal } from '../dashboard/widgets/HealthExportModal';
import { HealthHelpOverlay } from '../dashboard/widgets/HealthHelpOverlay';
import { ScoreBar } from './ScoreBar';
import {
  CHECK_DESCRIPTIONS,
  CHECK_LABELS,
  CHECK_ORDER,
  DEFAULT_FILTER,
  STATUS_COLORS,
  extractExamples,
  getSeverityBadge,
  getVisibleChecks,
  manualOnlyCategories,
  pendingActions,
  reviewItems,
  type FilterState,
  type FixResponse,
  type FixableFilter,
  type HealthReport,
  type SeverityFilter,
  type SortOrder,
} from './healthPanel.helpers';

// ==================== Main component ====================

export function HealthPanel({ hideTitle = false }: { hideTitle?: boolean } = {}) {
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
  // Session-scoped baseline: captures the overall score on first load.
  const [sessionStartScore, setSessionStartScore] = useState<number | null>(null);
  // Per-check lastCheckedAt (wall clock, millis) and transient pulse set.
  const [lastCheckedAt, setLastCheckedAt] = useState<Record<string, number>>({});
  const [recentlyUpdated, setRecentlyUpdated] = useState<Set<string>>(new Set());
  // Global scan in-flight (refresh all)—disables per-row run buttons.
  const [globalScanInFlight, setGlobalScanInFlight] = useState(false);
  const fetchHealth = useCallback(async () => {
    try {
      setError(null);
      setGlobalScanInFlight(true);
      const data = await getTransport().invoke<HealthReport>('get_health_knowledge', {});
      setReport(data);
      setSessionStartScore(prev => (prev === null ? data.overall_score : prev));
      // Stamp every check we just received.
      const now = Date.now();
      setLastCheckedAt(prev => {
        const next = { ...prev };
        for (const k of Object.keys(data.checks)) next[k] = now;
        return next;
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load health data');
    } finally {
      setLoading(false);
      setGlobalScanInFlight(false);
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
      setLastCheckedAt(prev => ({ ...prev, [checkName]: Date.now() }));
      // Flash the row briefly to signal the update.
      setRecentlyUpdated(prev => new Set(prev).add(checkName));
      window.setTimeout(() => {
        setRecentlyUpdated(prev => {
          const next = new Set(prev);
          next.delete(checkName);
          return next;
        });
      }, 1200);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Check failed');
    } finally {
      setRunningCheck(null);
    }
  }, []);

  const runFix = () => setShowConfirm(true);

  const applyFix = async (selectedChecks?: string[]) => {
    setShowConfirm(false);
    setFixing(true);
    setShowPending(false);
    if (undoTimerRef.current) clearTimeout(undoTimerRef.current);
    setUndoToast(null);
    try {
      const checksToFix = selectedChecks && selectedChecks.length > 0
        ? selectedChecks
        : report
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
    const toUndo = [...undoToast.fixIds].reverse();
    setUndoToast(null);
    for (const fixId of toUndo) {
      const ok = await runReviewAction({
        label: 'Undo fix',
        command: 'undo_health_fix',
        args: { fixId },
      });
      if (ok === undefined) return;
    }
    await fetchHealth();
  };

  // Compute these before early returns so keyboard handler can reference them
  const issueChecks = report ? getVisibleChecks(report, filter) : [];
  const pending: PendingFix[] = report ? pendingActions(report, excludedFromFix) : [];
  const manualOnly = report ? manualOnlyCategories(report) : [];
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
          {!hideTitle && <h3 className="text-sm font-semibold text-white">Knowledge Health</h3>}
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
          {sessionStartScore !== null && sessionStartScore !== report.overall_score && (() => {
            const delta = report.overall_score - sessionStartScore;
            const sign = delta > 0 ? '+' : '';
            const color = delta > 0 ? 'text-green-400' : 'text-red-400';
            return (
              <p
                className={`text-[11px] ${color} mt-0.5`}
                title={`Score at session start: ${sessionStartScore}`}
              >
                {sign}{delta} today
              </p>
            );
          })()}
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
          <span
            className="text-xs text-gray-600 ml-auto"
            aria-live="polite"
            title="Visible checks after filtering"
          >
            Showing {issueChecks.length} of {CHECK_ORDER.filter(k => {
              const c = report.checks[k];
              return c && c.status !== 'ok';
            }).length} categories
          </span>
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
                previousScore={report.previous_check_scores?.[key]}
                lastCheckedAt={lastCheckedAt[key]}
                disableRun={globalScanInFlight}
                justUpdated={recentlyUpdated.has(key)}
                examples={extractExamples(check)}
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
          manualOnly={manualOnly}
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