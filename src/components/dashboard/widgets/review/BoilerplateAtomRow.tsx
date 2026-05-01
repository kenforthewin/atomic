import { useState } from 'react';
import { RefreshCw, Loader2, Check } from 'lucide-react';
import { applyFix, type BoilerplateEntry, type ItemStatus } from './types';

export interface BoilerplateAtomRowProps {
  atom: BoilerplateEntry;
  onResolved: (atomId: string) => void;
}

export function BoilerplateAtomRow({ atom, onResolved }: BoilerplateAtomRowProps) {
  const [status, setStatus] = useState<ItemStatus>('idle');
  const [error, setError] = useState<string | null>(null);

  const reembed = async () => {
    setStatus('saving');
    setError(null);
    try {
      await applyFix('boilerplate_pollution', atom.id, { action: 'reembed' });
      setStatus('done');
      setTimeout(() => onResolved(atom.id), 400);
    } catch (e) {
      setStatus('error');
      setError(e instanceof Error ? e.message : 'Failed to re-embed');
    }
  };

  return (
    <div className="p-2.5 bg-[#1e1e1e] rounded border border-white/5">
      <div className="flex items-center gap-3">
        <div className="flex-1 min-w-0">
          <p className="text-xs text-gray-200 truncate">
            {atom.title || <span className="italic text-gray-500">Untitled atom</span>}
          </p>
          <p className="text-xs text-gray-600 mt-0.5">
            {atom.clone_count} near-identical edge{atom.clone_count !== 1 ? 's' : ''}
          </p>
        </div>
        <button
          type="button"
          onClick={reembed}
          disabled={status === 'saving' || status === 'done'}
          className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40 inline-flex items-center gap-1 shrink-0"
          title="Reset the embedding — useful after editing content"
        >
          {status === 'saving'
            ? <Loader2 className="w-3 h-3 animate-spin" />
            : status === 'done'
              ? <Check className="w-3 h-3 text-green-500" />
              : <RefreshCw className="w-3 h-3" />}
          Re-embed
        </button>
      </div>
      {error && <p className="text-xs text-red-400 mt-2">{error}</p>}
    </div>
  );
}
