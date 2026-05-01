import { createPortal } from 'react-dom';
import { X, Play } from 'lucide-react';
import { useEffect } from 'react';

export interface PendingFix {
  label: string;
  check: string;
}

interface Props {
  pending: PendingFix[];
  currentScore: number;
  onConfirm: () => void;
  onCancel: () => void;
}

export function HealthConfirmModal({ pending, currentScore, onConfirm, onCancel }: Props) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onCancel();
      if (e.key === 'Enter') onConfirm();
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [onCancel, onConfirm]);

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={e => { if (e.target === e.currentTarget) onCancel(); }}
    >
      <div className="bg-[#1e1e1e] border border-white/10 rounded-lg shadow-2xl w-full max-w-md mx-4">
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-white/5">
          <div>
            <h2 className="text-sm font-semibold text-white">Apply automatic fixes?</h2>
            <p className="text-xs text-gray-500 mt-0.5">Current score: {currentScore}/100</p>
          </div>
          <button onClick={onCancel} className="text-gray-500 hover:text-gray-300 transition-colors p-1">
            <X className="w-4 h-4" />
          </button>
        </div>

        {/* Fix list */}
        <div className="px-5 py-4 space-y-2">
          <p className="text-xs text-gray-400 mb-3">The following fixes will run:</p>
          <ul className="space-y-1.5">
            {pending.map((fix, i) => (
              <li key={i} className="flex items-center gap-2 text-xs text-gray-300">
                <span className="text-purple-400">•</span>
                {fix.label}
              </li>
            ))}
          </ul>
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 px-5 py-4 border-t border-white/5">
          <button
            onClick={onCancel}
            className="px-3 py-1.5 text-xs text-gray-400 hover:text-gray-200 transition-colors rounded hover:bg-white/5"
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            className="flex items-center gap-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 rounded text-xs text-white transition-colors"
          >
            <Play className="w-3 h-3" />
            Apply {pending.length} fix{pending.length !== 1 ? 'es' : ''}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}
