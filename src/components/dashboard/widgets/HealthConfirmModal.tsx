import { createPortal } from 'react-dom';
import { X, Play } from 'lucide-react';
import { useEffect, useState } from 'react';

export interface PendingFix {
  label: string;
  check: string;
}

export interface ManualOnlyCategory {
  label: string;
  check: string;
  reason?: string;
}

interface Props {
  pending: PendingFix[];
  manualOnly?: ManualOnlyCategory[];
  currentScore: number;
  atomsAffected?: number;
  /** Called with the final subset of check names to run. */
  onConfirm: (selectedChecks: string[]) => void;
  onCancel: () => void;
}

export function HealthConfirmModal({
  pending,
  manualOnly = [],
  currentScore,
  atomsAffected,
  onConfirm,
  onCancel,
}: Props) {
  const [selected, setSelected] = useState<Set<string>>(
    () => new Set(pending.map(p => p.check)),
  );

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onCancel();
      if (e.key === 'Enter' && selected.size > 0) {
        onConfirm(Array.from(selected));
      }
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [onCancel, onConfirm, selected]);

  const toggle = (check: string) => {
    setSelected(prev => {
      const next = new Set(prev);
      if (next.has(check)) next.delete(check);
      else next.add(check);
      return next;
    });
  };

  const selectedCount = selected.size;
  const estSec = Math.max(2, selectedCount * 3);
  const estLabel = estSec < 60 ? `~${estSec}s` : `~${Math.ceil(estSec / 60)}m`;

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={e => { if (e.target === e.currentTarget) onCancel(); }}
      role="dialog"
      aria-modal="true"
      aria-label="Apply automatic fixes"
    >
      <div className="bg-[#1e1e1e] border border-white/10 rounded-lg shadow-2xl w-full max-w-lg mx-4">
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-white/5">
          <div>
            <h2 className="text-sm font-semibold text-white">Apply automatic fixes?</h2>
            <p className="text-xs text-gray-500 mt-0.5">
              Current score: {currentScore}/100
              {atomsAffected !== undefined && ` · ~${atomsAffected} atoms affected`}
              {` · est. ${estLabel}`}
            </p>
          </div>
          <button
            onClick={onCancel}
            className="text-gray-500 hover:text-gray-300 transition-colors p-1"
            aria-label="Close"
          >
            <X className="w-4 h-4" />
          </button>
        </div>

        {/* Fix list */}
        <div className="px-5 py-4 space-y-2">
          <p className="text-xs text-gray-400 mb-2">Select the fixes to run:</p>
          <ul className="space-y-1.5">
            {pending.map((fix) => {
              const on = selected.has(fix.check);
              return (
                <li key={fix.check}>
                  <label className="flex items-start gap-2 text-xs text-gray-300 cursor-pointer select-none hover:bg-white/5 px-1.5 py-1 rounded -mx-1.5">
                    <input
                      type="checkbox"
                      checked={on}
                      onChange={() => toggle(fix.check)}
                      className="w-3.5 h-3.5 rounded accent-purple-500 mt-0.5 shrink-0"
                      aria-label={`Include ${fix.label}`}
                    />
                    <span className={on ? '' : 'text-gray-500 line-through'}>
                      {fix.label}
                    </span>
                  </label>
                </li>
              );
            })}
          </ul>

          {manualOnly.length > 0 && (
            <div className="mt-3 pt-3 border-t border-white/5">
              <p className="text-[11px] uppercase tracking-wide text-gray-600 mb-1.5">
                Manual review only (no auto-fix)
              </p>
              <ul className="space-y-1">
                {manualOnly.map(cat => (
                  <li
                    key={cat.check}
                    className="flex items-start gap-2 text-xs text-gray-600 px-1.5 py-0.5"
                  >
                    <input
                      type="checkbox"
                      checked={false}
                      disabled
                      className="w-3.5 h-3.5 rounded mt-0.5 shrink-0 opacity-30"
                      aria-hidden="true"
                    />
                    <span>
                      {cat.label}
                      {cat.reason && (
                        <span className="text-gray-700"> — {cat.reason}</span>
                      )}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-between gap-2 px-5 py-4 border-t border-white/5">
          <p className="text-xs text-gray-600" aria-live="polite">
            {selectedCount} of {pending.length} selected
          </p>
          <div className="flex items-center gap-2">
            <button
              onClick={onCancel}
              className="px-3 py-1.5 text-xs text-gray-400 hover:text-gray-200 transition-colors rounded hover:bg-white/5"
            >
              Cancel
            </button>
            <button
              onClick={() => onConfirm(Array.from(selected))}
              disabled={selectedCount === 0}
              className="flex items-center gap-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 disabled:bg-[#3a3a3a] disabled:text-gray-500 disabled:cursor-not-allowed rounded text-xs text-white transition-colors"
            >
              <Play className="w-3 h-3" />
              Apply {selectedCount} fix{selectedCount !== 1 ? 'es' : ''}
            </button>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  );
}
