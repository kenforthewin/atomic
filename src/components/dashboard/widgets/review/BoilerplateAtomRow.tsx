import { useState } from 'react';
import { RefreshCw, Loader2, Check, Scissors } from 'lucide-react';
import { applyFix, type BoilerplateEntry, type ItemStatus } from './types';
import { getTransport } from '../../../../lib/transport';
import { runReviewAction } from './reviewActions';
import { lineDiff } from './diffUtil';
import { toast } from '../../../../stores/toasts';

export interface BoilerplateAtomRowProps {
  atom: BoilerplateEntry;
  onResolved: (atomId: string) => void;
}

export function BoilerplateAtomRow({ atom, onResolved }: BoilerplateAtomRowProps) {
  const [status, setStatus] = useState<ItemStatus>('idle');
  const [stripPreview, setStripPreview] = useState<{ original: string; proposed: string } | null>(null);
  const [stripping, setStripping] = useState(false);

  const reembed = async () => {
    setStatus('saving');
    const ok = await applyFix('Re-embed atom', 'boilerplate_pollution', atom.id, { action: 'reembed' });
    if (ok === undefined) { setStatus('idle'); return; }
    setStatus('done');
    setTimeout(() => onResolved(atom.id), 400);
  };

  const previewStrip = async () => {
    setStripping(true);
    try {
      const a = await getTransport().invoke<{ content: string }>('get_atom', { id: atom.id });
      const resp = await getTransport().invoke<{ content: string }>('health_strip_boilerplate', { atom_id: atom.id, dry_run: true });
      setStripPreview({ original: a.content, proposed: resp.content });
    } catch (e) {
      toast.error('Preview strip failed', {
        detail: e instanceof Error ? e.message : String(e),
        retry: () => previewStrip(),
      });
    } finally {
      setStripping(false);
    }
  };

  const applyStrip = async () => {
    if (!stripPreview) return;
    setStripping(true);
    const ok = await runReviewAction({
      label: 'Apply strip',
      command: 'health_strip_boilerplate',
      args: { atom_id: atom.id, dry_run: false },
    });
    if (ok === undefined) { setStripping(false); return; }
    setStatus('done');
    setTimeout(() => onResolved(atom.id), 400);
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
        <div className="flex items-center gap-1.5 shrink-0">
          <button
            type="button"
            onClick={previewStrip}
            disabled={stripping || status === 'done'}
            className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40 inline-flex items-center gap-1"
            title="Ask LLM to remove template boilerplate, keep unique content"
          >
            {stripping ? <Loader2 className="w-3 h-3 animate-spin" /> : <Scissors className="w-3 h-3" />}
            Strip…
          </button>
          <button
            type="button"
            onClick={reembed}
            disabled={status === 'saving' || status === 'done'}
            className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40 inline-flex items-center gap-1"
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
      </div>
      {stripPreview && (
        <div className="mt-2 space-y-2 border-t border-white/5 pt-2">
          <p className="text-xs text-yellow-300/80">Preview — apply to update the atom</p>
          <pre className="text-xs bg-[#161616] rounded p-2 max-h-72 overflow-y-auto whitespace-pre-wrap leading-relaxed font-sans">
            {lineDiff(stripPreview.original, stripPreview.proposed).map((p, i) => (
              <span key={i} className={
                p.type === 'insert' ? 'bg-green-900/30 text-green-300' :
                p.type === 'delete' ? 'bg-red-900/30 text-red-300' :
                'text-gray-400'
              }>{p.text}</span>
            ))}
          </pre>
          <div className="flex justify-end gap-1.5">
            <button onClick={() => setStripPreview(null)} className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5">Cancel</button>
            <button onClick={applyStrip} disabled={stripping} className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40">Apply strip</button>
          </div>
        </div>
      )}
    </div>
  );
}
