import { useState, useEffect, useRef, useCallback } from 'react';
import { createPortal } from 'react-dom';
import {
  getPipelineStatus,
  retryEmbedding,
  resetStuckProcessing,
  processPendingEmbeddings,
  type PipelineStatus as PipelineStatusData,
  type FailedAtom,
} from '../../lib/api';

interface PipelineStatusProps {
  isOpen: boolean;
  onClose: () => void;
}

function relativeTime(dateStr: string): string {
  const now = Date.now();
  const then = new Date(dateStr).getTime();
  const diff = now - then;
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

function truncate(str: string, max: number): string {
  if (!str) return '';
  return str.length > max ? str.slice(0, max) + '...' : str;
}

export function PipelineStatus({ isOpen, onClose }: PipelineStatusProps) {
  const [status, setStatus] = useState<PipelineStatusData | null>(null);
  const [retrying, setRetrying] = useState<Set<string>>(new Set());
  const [retryingAll, setRetryingAll] = useState(false);
  const [actionInProgress, setActionInProgress] = useState<string | null>(null);
  const overlayRef = useRef<HTMLDivElement>(null);

  const fetchStatus = useCallback(async () => {
    try {
      const data = await getPipelineStatus();
      setStatus(data);
    } catch (err) {
      console.error('Failed to fetch pipeline status:', err);
    }
  }, []);

  // Poll every 5 seconds while open
  useEffect(() => {
    if (!isOpen) return;
    fetchStatus();
    const interval = setInterval(fetchStatus, 5000);
    return () => clearInterval(interval);
  }, [isOpen, fetchStatus]);

  // ESC to close
  useEffect(() => {
    if (!isOpen) return;
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', handleKey);
    return () => document.removeEventListener('keydown', handleKey);
  }, [isOpen, onClose]);

  const handleRetry = async (atomId: string) => {
    setRetrying(prev => new Set(prev).add(atomId));
    try {
      await retryEmbedding(atomId);
      await fetchStatus();
    } catch (err) {
      console.error('Retry failed:', err);
    } finally {
      setRetrying(prev => {
        const next = new Set(prev);
        next.delete(atomId);
        return next;
      });
    }
  };

  const handleRetryAll = async () => {
    if (!status?.failed.length) return;
    setRetryingAll(true);
    try {
      await Promise.all(status.failed.map(f => retryEmbedding(f.atom_id)));
      await fetchStatus();
    } catch (err) {
      console.error('Retry all failed:', err);
    } finally {
      setRetryingAll(false);
    }
  };

  const handleResetStuck = async () => {
    setActionInProgress('reset');
    try {
      const count = await resetStuckProcessing();
      if (count > 0) {
        console.log(`Reset ${count} stuck atoms`);
      }
      await fetchStatus();
    } catch (err) {
      console.error('Reset stuck failed:', err);
    } finally {
      setActionInProgress(null);
    }
  };

  const handleProcessPending = async () => {
    setActionInProgress('process');
    try {
      const count = await processPendingEmbeddings();
      if (count > 0) {
        console.log(`Processing ${count} pending atoms`);
      }
      await fetchStatus();
    } catch (err) {
      console.error('Process pending failed:', err);
    } finally {
      setActionInProgress(null);
    }
  };

  if (!isOpen) return null;

  return createPortal(
    <div
      ref={overlayRef}
      onClick={(e) => e.target === overlayRef.current && onClose()}
      data-modal="true"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
    >
      <div className="relative bg-[var(--color-bg-panel)] rounded-lg shadow-xl border border-[var(--color-border)] w-full max-w-3xl mx-4 max-h-[85vh] flex flex-col animate-in fade-in zoom-in-95 duration-200">
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-3 border-b border-[var(--color-border)]">
          <h2 className="text-base font-semibold text-[var(--color-text-primary)]">Pipeline Status</h2>
          <button
            onClick={onClose}
            className="text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
          >
            <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
            </svg>
          </button>
        </div>

        {/* Summary stats */}
        {status ? (
          <>
            <div className="grid grid-cols-4 gap-3 px-5 py-4 border-b border-[var(--color-border)]">
              <StatBox label="Pending" value={status.pending} color="amber" />
              <StatBox label="Processing" value={status.processing} color="blue" pulse={status.processing > 0} />
              <StatBox label="Complete" value={status.complete} color="emerald" />
              <StatBox label="Failed" value={status.failed_count} color="red" />
            </div>

            {/* Failed atoms list */}
            {status.failed_count > 0 && (
              <div className="flex-1 overflow-y-auto">
                <div className="flex items-center justify-between px-5 py-3 border-b border-[var(--color-border)]">
                  <h3 className="text-sm font-medium text-[var(--color-text-primary)]">
                    Failed Atoms ({status.failed_count})
                  </h3>
                  <button
                    onClick={handleRetryAll}
                    disabled={retryingAll}
                    className="px-3 py-1 text-xs font-medium rounded-md bg-red-500/20 text-red-400 hover:bg-red-500/30 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
                  >
                    {retryingAll ? 'Retrying...' : 'Retry All Failed'}
                  </button>
                </div>
                <div className="px-5 py-2 space-y-2">
                  {status.failed.map((atom) => (
                    <FailedAtomRow
                      key={atom.atom_id}
                      atom={atom}
                      isRetrying={retrying.has(atom.atom_id)}
                      onRetry={() => handleRetry(atom.atom_id)}
                    />
                  ))}
                </div>
              </div>
            )}

            {status.failed_count === 0 && (
              <div className="flex-1 flex items-center justify-center py-12 text-[var(--color-text-secondary)] text-sm">
                No failed atoms
              </div>
            )}

            {/* Action buttons */}
            <div className="flex items-center gap-2 px-5 py-3 border-t border-[var(--color-border)]">
              <button
                onClick={handleResetStuck}
                disabled={actionInProgress !== null}
                className="px-3 py-1.5 text-xs font-medium rounded-md bg-[var(--color-bg-card)] text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] disabled:opacity-50 disabled:cursor-not-allowed transition-colors border border-[var(--color-border)]"
              >
                {actionInProgress === 'reset' ? 'Resetting...' : 'Reset Stuck'}
              </button>
              <button
                onClick={handleProcessPending}
                disabled={actionInProgress !== null}
                className="px-3 py-1.5 text-xs font-medium rounded-md bg-[var(--color-accent)] text-white hover:brightness-110 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
              >
                {actionInProgress === 'process' ? 'Processing...' : 'Process Pending'}
              </button>
            </div>
          </>
        ) : (
          <div className="flex items-center justify-center py-16 text-[var(--color-text-secondary)] text-sm">
            Loading...
          </div>
        )}
      </div>
    </div>,
    document.body
  );
}

// ── Sub-components ──────────────────────────────────────

function StatBox({ label, value, color, pulse }: { label: string; value: number; color: string; pulse?: boolean }) {
  const colorMap: Record<string, string> = {
    amber: 'bg-amber-500/15 text-amber-400 border-amber-500/30',
    blue: 'bg-blue-500/15 text-blue-400 border-blue-500/30',
    emerald: 'bg-emerald-500/15 text-emerald-400 border-emerald-500/30',
    red: 'bg-red-500/15 text-red-400 border-red-500/30',
  };

  return (
    <div className={`rounded-lg border px-3 py-2.5 text-center ${colorMap[color] || ''}`}>
      <div className={`text-2xl font-bold tabular-nums ${pulse ? 'animate-pulse' : ''}`}>
        {value}
      </div>
      <div className="text-[11px] font-medium uppercase tracking-wider opacity-80 mt-0.5">
        {label}
      </div>
    </div>
  );
}

function FailedAtomRow({ atom, isRetrying, onRetry }: { atom: FailedAtom; isRetrying: boolean; onRetry: () => void }) {
  return (
    <div className="flex items-start gap-3 p-3 rounded-lg bg-[var(--color-bg-card)] border border-[var(--color-border)]">
      <div className="flex-1 min-w-0">
        <div className="text-sm font-medium text-[var(--color-text-primary)] truncate">
          {truncate(atom.title || 'Untitled', 60)}
        </div>
        {atom.error && (
          <div className="mt-1 text-xs font-mono text-red-400 leading-relaxed break-all">
            {atom.error}
          </div>
        )}
        <div className="mt-1 text-[11px] text-[var(--color-text-secondary)]">
          {relativeTime(atom.updated_at)}
        </div>
      </div>
      <button
        onClick={onRetry}
        disabled={isRetrying}
        className="shrink-0 px-2.5 py-1 text-xs font-medium rounded-md bg-[var(--color-bg-main)] text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] disabled:opacity-50 disabled:cursor-not-allowed transition-colors border border-[var(--color-border)]"
      >
        {isRetrying ? 'Retrying...' : 'Retry'}
      </button>
    </div>
  );
}
