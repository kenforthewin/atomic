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

  // Subscribe to health-updated events
  useEffect(() => {
    const unsub = getTransport().subscribe('health-updated', () => fetchHealth());
    return unsub;
  }, [fetchHealth]);

  const score = report?.overall_score ?? 0;
  const statusColor = STATUS_COLORS[report?.overall_status ?? ''] ?? 'text-gray-400';

  return (
    <div className="p-4 bg-[#252525] rounded border border-white/5 flex flex-col gap-3">
      <div className="flex items-center gap-2">
        <HeartPulse className="w-4 h-4 text-purple-400" strokeWidth={2} />
        <h3 className="text-sm font-semibold text-white">Knowledge Health</h3>
      </div>

      {report ? (
        <>
          <div className="flex items-end gap-2">
            <span className={`text-4xl font-bold leading-none ${statusColor}`}>{score}</span>
            <span className="text-gray-500 text-sm mb-0.5">/100</span>
          </div>

          <div className="space-y-1">
            {report.requires_review > 0 && (
              <p className="text-xs text-yellow-400">
                {report.requires_review} issue{report.requires_review !== 1 ? 's' : ''} to review
              </p>
            )}
            {report.auto_fixable > 0 && (
              <p className="text-xs text-blue-400">
                {report.auto_fixable} auto-fixable
              </p>
            )}
            {report.requires_review === 0 && report.auto_fixable === 0 && (
              <p className="text-xs text-green-400">All checks passing</p>
            )}
          </div>
        </>
      ) : (
        <div className="h-10 flex items-center">
          <p className="text-xs text-gray-600">Loading…</p>
        </div>
      )}

      <button
        onClick={() => setViewMode('health')}
        className="flex items-center justify-between w-full mt-1 px-3 py-2 bg-purple-600 hover:bg-purple-500 rounded text-xs text-white font-medium transition-colors"
        aria-label="Open Knowledge Health page"
      >
        Open Knowledge Health
        <ArrowRight className="w-3.5 h-3.5" />
      </button>
    </div>
  );
}
