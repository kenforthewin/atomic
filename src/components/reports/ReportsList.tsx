import { memo, useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { Telescope } from 'lucide-react';
import { Report, ReportFindingWithAtom } from '../../stores/reports';
import { ReportRow } from './ReportRow';

interface ReportsListProps {
  reports: Report[];
  lastFindingByReport: Record<string, ReportFindingWithAtom | null>;
  isLoading: boolean;
  onRowClick?: (reportId: string) => void;
  onEdit?: (reportId: string) => void;
  onToggleEnabled?: (reportId: string, next: boolean) => void;
  onDelete?: (reportId: string) => void;
}

/// Row height target. Two lines of identity (name + excerpt) + the
/// vertical padding of the row container. Used as the virtualizer's
/// estimate; rows in this list are uniformly tall, so the estimate is
/// the actual height.
const ROW_HEIGHT = 76;

export const ReportsList = memo(function ReportsList({
  reports,
  lastFindingByReport,
  isLoading,
  onRowClick,
  onEdit,
  onToggleEnabled,
  onDelete,
}: ReportsListProps) {
  const parentRef = useRef<HTMLDivElement>(null);

  const virtualizer = useVirtualizer({
    count: reports.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 6,
  });

  if (reports.length === 0 && isLoading) {
    return (
      <div className="h-full overflow-y-auto scrollbar-auto-hide">
        {Array.from({ length: 5 }, (_, i) => (
          <div key={i} className="grid grid-cols-[1fr_auto_auto] items-center gap-6 px-5 py-4 border-b border-[var(--color-border)]">
            <div className="flex flex-col gap-2">
              <div className="h-4 w-48 bg-[var(--color-border)] rounded animate-pulse" />
              <div className="h-3 w-80 max-w-full bg-[var(--color-border)]/60 rounded animate-pulse" />
            </div>
            <div className="hidden md:block h-3 w-32 bg-[var(--color-border)]/50 rounded animate-pulse" />
            <div className="h-3 w-20 bg-[var(--color-border)]/50 rounded animate-pulse" />
          </div>
        ))}
      </div>
    );
  }

  if (reports.length === 0) {
    return (
      <div className="flex flex-col items-center justify-center h-full text-center px-8">
        <Telescope className="w-16 h-16 text-[var(--color-border)] mb-4" strokeWidth={1.5} />
        <h3 className="text-lg font-medium text-[var(--color-text-primary)] mb-2">No reports yet</h3>
        <p className="text-sm text-[var(--color-text-secondary)] max-w-sm">
          Reports are scheduled research tasks that turn your atoms into recurring
          findings. The next phase of authoring lands the create flow — for now,
          a seeded "Daily Briefing" should appear here on first run.
        </p>
      </div>
    );
  }

  return (
    <div ref={parentRef} className="h-full overflow-y-auto scrollbar-auto-hide">
      <div
        className="relative w-full"
        style={{ height: `${virtualizer.getTotalSize()}px` }}
      >
        {virtualizer.getVirtualItems().map((virtualRow) => {
          const report = reports[virtualRow.index];
          return (
            <div
              key={report.id}
              className="absolute left-0 right-0"
              style={{
                top: `${virtualRow.start}px`,
                height: `${virtualRow.size}px`,
              }}
            >
              <ReportRow
                report={report}
                lastFinding={lastFindingByReport[report.id]}
                onClick={onRowClick}
                onEdit={onEdit}
                onToggleEnabled={onToggleEnabled}
                onDelete={onDelete}
              />
            </div>
          );
        })}
      </div>
    </div>
  );
});
