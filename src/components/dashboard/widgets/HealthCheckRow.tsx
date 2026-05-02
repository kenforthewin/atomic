import { Play, Search, ChevronDown, ChevronUp, Loader2 } from 'lucide-react';

export function getTrend(current: number, previous?: number): '↑' | '↓' | '→' {
  if (previous === undefined) return '→';
  if (current > previous) return '↑';
  if (current < previous) return '↓';
  return '→';
}
// These constants are re-exported here for use by HealthCheckRow
// The canonical source remains health/HealthPanel.tsx
export interface HealthCheckResult {
  status: 'ok' | 'warning' | 'error' | 'info';
  score: number;
  auto_fixable: boolean;
  requires_review: boolean;
  fix_action?: string;
  data: Record<string, unknown>;
}

export interface HealthCheckRowProps {
  checkName: string;
  check: HealthCheckResult;
  label: string;
  description: string;
  isExpanded: boolean;
  onToggleExpand: (name: string) => void;
  onRun: (name: string) => void;
  onReview: (name: string) => void;
  isRunning: boolean;
  includeInFix: boolean;
  onToggleInclude: (name: string) => void;
  trend?: '↑' | '↓' | '→';
  severityBadge?: string;
  previousScore?: number;
  lastCheckedAt?: number;
  disableRun?: boolean;
  justUpdated?: boolean;
  examples?: string[];
}

function ScoreBarMini({ score }: { score: number }) {
  const color =
    score >= 90 ? 'bg-green-500' :
    score >= 70 ? 'bg-yellow-500' :
    score >= 50 ? 'bg-orange-500' : 'bg-red-500';
  return (
    <div className="w-20 bg-[#3a3a3a] rounded-full h-1.5 shrink-0">
      <div
        className={`${color} h-1.5 rounded-full transition-all duration-500`}
        style={{ width: `${score}%` }}
      />
    </div>
  );
}

export function HealthCheckRow({
  checkName,
  check,
  label,
  description,
  isExpanded,
  onToggleExpand,
  onRun,
  onReview,
  isRunning,
  includeInFix,
  onToggleInclude,
  trend,
  severityBadge,
  previousScore,
  lastCheckedAt,
  disableRun,
  justUpdated,
  examples,
}: HealthCheckRowProps) {
  const scoreColor =
    check.score >= 90 ? 'text-green-400' :
    check.score >= 70 ? 'text-yellow-400' :
    check.score >= 50 ? 'text-orange-400' : 'text-red-400';

  const severityTitle =
    check.score >= 86 ? 'Healthy — score 86+' :
    check.score >= 71 ? 'Needs attention — score 71–85' :
    check.score >= 41 ? 'Warning — score 41–70' :
                          'Critical — score below 41';

  const critical = check.score < 41;

  const trendTitle = (() => {
    if (trend === undefined) return undefined;
    if (previousScore === undefined) return 'No previous score recorded';
    const delta = check.score - previousScore;
    const sign = delta > 0 ? '+' : '';
    return `Was ${previousScore}, now ${check.score} (${sign}${delta} since last scan)`;
  })();

  const lastCheckedLabel = lastCheckedAt ? formatRelative(lastCheckedAt) : null;

  return (
    <div
      className={`border-b border-white/5 py-2 last:border-b-0 transition-colors ${
        justUpdated ? 'bg-green-500/5' : ''
      }`}
    >
      {/* Header row */}
      <div className="flex items-center gap-2">
        {/* Expand toggle */}
        <button
          onClick={() => onToggleExpand(checkName)}
          className="text-gray-600 hover:text-gray-400 transition-colors shrink-0"
          aria-label={isExpanded ? `Collapse ${label}` : `Expand ${label}`}
          aria-expanded={isExpanded}
        >
          {isExpanded
            ? <ChevronUp className="w-3.5 h-3.5" />
            : <ChevronDown className="w-3.5 h-3.5" />}
        </button>

        {/* Label + score bar */}
        <div className="flex-1 min-w-0 flex items-center gap-2">
          <span className="text-xs text-gray-300 truncate">{label}</span>
          <ScoreBarMini score={check.score} />
          <span className={`text-xs font-mono shrink-0 ${scoreColor}`}>{check.score}</span>
        </div>

        {/* Trend indicator */}
        {trend !== undefined && (
          <span
            className={`text-xs shrink-0 ${
              trend === '↑' ? 'text-green-400' :
              trend === '↓' ? 'text-red-400' :
              'text-gray-600'
            }`}
            aria-label={`Trend: ${trend}`}
            title={trendTitle}
          >
            {trend}
          </span>
        )}
        {severityBadge && (
          <span
            className={`text-sm shrink-0 ${critical ? 'animate-pulse' : ''}`}
            aria-label={`Severity: ${severityTitle}`}
            title={severityTitle}
          >
            {severityBadge}
          </span>
        )}
        {/* Action buttons */}
        <div className="flex items-center gap-0.5 shrink-0">
          <button
            onClick={() => onRun(checkName)}
            disabled={isRunning || disableRun}
            className="p-1.5 text-gray-500 hover:text-gray-300 disabled:opacity-40 disabled:cursor-not-allowed transition-colors rounded hover:bg-white/5"
            title={
              disableRun && !isRunning
                ? 'Global scan in progress…'
                : `Re-run ${label} check`
            }
            aria-label={`Re-run ${label} check`}
          >
            {isRunning
              ? <Loader2 className="w-3.5 h-3.5 animate-spin" />
              : <Play className="w-3.5 h-3.5" />}
          </button>

          {check.requires_review && (
            <button
              onClick={() => onReview(checkName)}
              className="p-1.5 text-gray-500 hover:text-yellow-400 transition-colors rounded hover:bg-white/5"
              title={`Review ${label} samples`}
              aria-label={`Review ${label} samples`}
            >
              <Search className="w-3.5 h-3.5" />
            </button>
          )}
        </div>
      </div>

      {/* Description (always shown) */}
      {description && (
        <p className="text-xs text-gray-500 pl-5 mt-0.5 leading-relaxed">{description}</p>
      )}

      {/* Expanded detail */}
      {isExpanded && (
        <div className="mt-2 pl-5 space-y-2">
          {lastCheckedLabel && (
            <p className="text-[11px] text-gray-600">Last checked: {lastCheckedLabel}</p>
          )}

          {examples && examples.length > 0 && (
            <div className="text-xs text-gray-400 space-y-0.5">
              {examples.slice(0, 2).map((ex, i) => (
                <div key={i} className="flex items-start gap-1.5">
                  <span className="text-gray-600 mt-0.5">•</span>
                  <span className="truncate" title={ex}>{ex}</span>
                </div>
              ))}
            </div>
          )}

          {check.auto_fixable && (
            <label className="flex items-center gap-2 text-xs text-gray-400 cursor-pointer select-none">
              <input
                type="checkbox"
                checked={includeInFix}
                onChange={() => onToggleInclude(checkName)}
                className="w-3 h-3 rounded accent-purple-500"
              />
              <span>Include in auto-fix batch</span>
            </label>
          )}

          {check.requires_review && (
            <button
              onClick={() => onReview(checkName)}
              className="text-xs text-blue-400 hover:text-blue-300 transition-colors"
            >
              View samples →
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function formatRelative(ts: number): string {
  const diff = Date.now() - ts;
  if (diff < 0) return 'just now';
  const mins = Math.floor(diff / 60_000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins} min ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs} hr ago`;
  const days = Math.floor(hrs / 24);
  return `${days}d ago`;
}
