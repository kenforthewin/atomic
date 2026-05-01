import { useState } from 'react';
import { ExternalLink, Check, Loader2, EyeOff } from 'lucide-react';
import type { AtomPreview, ItemStatus } from './types';
import { applyFix } from './types';
import { toast } from '../../../../stores/toasts';

export interface NoSourceRowProps {
  atom: AtomPreview;
  onResolved: (atomId: string) => void;
}

export function NoSourceRow({ atom, onResolved }: NoSourceRowProps) {
  const [editing, setEditing] = useState(false);
  const [url, setUrl] = useState('');
  const [status, setStatus] = useState<ItemStatus>('idle');

  const save = async () => {
    const trimmed = url.trim();
    if (!trimmed) {
      toast.error('Enter a URL');
      return;
    }
    setStatus('saving');
    const ok = await applyFix('Save source URL', 'content_quality', atom.id, { action: 'add_source', url: trimmed });
    if (ok === undefined) { setStatus('idle'); return; }
    setStatus('done');
    setTimeout(() => onResolved(atom.id), 400);
  };

  const dismiss = async () => {
    setStatus('saving');
    const ok = await applyFix('Mark intentional', 'content_quality', atom.id, { action: 'mark_intentional' });
    if (ok === undefined) { setStatus('idle'); return; }
    setStatus('done');
    setTimeout(() => onResolved(atom.id), 400);
  };

  const openAtom = () => {
    window.dispatchEvent(new CustomEvent('app-open-atom', { detail: { atomId: atom.id } }));
  };

  return (
    <div className="p-2.5 bg-[#1e1e1e] rounded border border-white/5 space-y-2">
      <div className="flex items-center gap-3">
        <div className="flex-1 min-w-0">
          <p className="text-xs text-gray-200 truncate">
            {atom.title || <span className="italic text-gray-500">Untitled atom</span>}
          </p>
          {atom.created_at && (
            <p className="text-xs text-gray-600 mt-0.5">
              Created {new Date(atom.created_at).toLocaleDateString()}
            </p>
          )}
        </div>
        <div className="flex items-center gap-1 shrink-0">
          <button
            type="button"
            onClick={() => setEditing(v => !v)}
            disabled={status === 'saving'}
            className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40"
            title="Add a source URL"
          >
            {editing ? 'Cancel' : 'Add source'}
          </button>
          <button
            type="button"
            onClick={dismiss}
            disabled={status === 'saving'}
            className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40 inline-flex items-center gap-1"
            title="Mark as intentional — removes from queue"
          >
            <EyeOff className="w-3 h-3" />
            Intentional
          </button>
          <button
            type="button"
            onClick={openAtom}
            className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors inline-flex items-center gap-1"
            title="Open atom in editor"
          >
            <ExternalLink className="w-3 h-3" />
          </button>
        </div>
      </div>

      {editing && (
        <div className="flex items-center gap-2 pt-1">
          <input
            type="url"
            value={url}
            onChange={e => setUrl(e.target.value)}
            onKeyDown={e => { if (e.key === 'Enter') void save(); }}
            placeholder="https://…"
            autoFocus
            className="flex-1 bg-[#161616] border border-white/10 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-purple-500"
          />
          <button
            type="button"
            onClick={save}
            disabled={status === 'saving'}
            className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 transition-colors disabled:opacity-40 inline-flex items-center gap-1"
          >
            {status === 'saving' ? <Loader2 className="w-3 h-3 animate-spin" /> : <Check className="w-3 h-3" />}
            Save
          </button>
        </div>
      )}
    </div>
  );
}
