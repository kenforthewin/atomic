import { useEffect, useState, useCallback } from 'react';
import { HeartPulse, ArrowRight } from 'lucide-react';
import { getTransport } from '../../../lib/transport';
import { useUIStore } from '../../../stores/ui';

interface HealthReport {
  overall_score: number;
  overall_status: 'healthy' | 'needs_attention' | 'degraded' | 'unhealthy';
  auto_fixable: number;
  requires_review: number;
}

const STATUS_COLORS: Record<string, string> = {
  healthy:         'text-green-400',
  needs_attention: 'text-yellow-400',
  degraded:        'text-orange-400',
  unhealthy:       'text-red-400',
};

const STATUS_LABELS: Record<string, string> = {
  healthy: 'Healthy',
  needs_attention: 'Needs attention',
  degraded: 'Degraded',
  unhealthy: 'Unhealthy',
};

/**
 * Compact dashboard card. Entire surface is a single button that navigates
 * to the full Knowledge Health page — users don't need a secondary CTA.
 */
export function HealthSummaryCard() {
  const [report, setReport] = useState<HealthReport | null>(null);
  const setViewMode = useUIStore(s => s.setViewMode);

  const fetchHealth = useCallback(async () => {
    try {
      const data = await getTransport().invoke<HealthReport>('get_health_knowledge', {});
      setReport(data);
    } catch {
      // silently ignore — card is non-critical
    }
  }, []);

  useEffect(() => { fetchHealth(); }, [fetchHealth]);

  useEffect(() => {
    const unsub = getTransport().subscribe('health-updated', () => fetchHealth());
    return unsub;
  }, [fetchHealth]);

  const score = report?.overall_score ?? 0;
  const status = report?.overall_status ?? '';
  const statusColor = STATUS_COLORS[status] ?? 'text-gray-400';
  const statusLabel = STATUS_LABELS[status] ?? '—';

  const issues = (report?.requires_review ?? 0) + (report?.auto_fixable ?? 0);
  const summary = !report
    ? 'Loading…'
    : issues === 0
      ? 'All checks passing'
      : `${issues} issue${issues === 1 ? '' : 's'}`
        + (report.auto_fixable > 0 ? ` · ${report.auto_fixable} auto-fixable` : '');

  return (
    <button
      onClick={() => setViewMode('health')}
      aria-label={`Open Knowledge Health — score ${score} out of 100, ${statusLabel}`}
      className="group w-full text-left p-3 bg-[#252525] hover:bg-[#2d2d2d] rounded border border-white/5 hover:border-purple-500/40 transition-colors flex items-center gap-3"
    >
      <div className="shrink-0 w-9 h-9 rounded-full bg-purple-600/10 flex items-center justify-center">
        <HeartPulse className="w-4 h-4 text-purple-400" strokeWidth={2} />
      </div>

      <div className="flex-1 min-w-0">
        <div className="flex items-baseline gap-2">
          <span className="text-xs text-gray-400 font-medium">Knowledge Health</span>
          <span className={`text-xs font-medium ${statusColor}`}>{statusLabel}</span>
        </div>
        <div className="flex items-baseline gap-1.5 mt-0.5">
          <span className={`text-2xl font-bold leading-none ${statusColor}`}>{score}</span>
          <span className="text-gray-600 text-xs">/100</span>
          <span className="text-xs text-gray-500 truncate ml-2">{summary}</span>
        </div>
      </div>

      <ArrowRight className="w-4 h-4 text-gray-600 group-hover:text-purple-400 shrink-0 transition-colors" />
    </button>
  );
}
