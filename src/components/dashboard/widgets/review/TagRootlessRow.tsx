import { useState, useMemo } from 'react';
import { EyeOff, Loader2, Check } from 'lucide-react';
import { applyFix, type RootlessTag, type ItemStatus } from './types';
import { toast } from '../../../../stores/toasts';

interface TagOption { id: string; name: string; }

export interface TagRootlessRowProps {
  tag: RootlessTag;
  parentOptions: TagOption[];
  onResolved: (tagId: string) => void;
}

export function TagRootlessRow({ tag, parentOptions, onResolved }: TagRootlessRowProps) {
  const [parentId, setParentId] = useState('');
  const [status, setStatus] = useState<ItemStatus>('idle');

  const options = useMemo(
    () => parentOptions.filter(o => o.id !== tag.id),
    [parentOptions, tag.id],
  );

  const move = async () => {
    if (!parentId) {
      toast.error('Pick a parent tag');
      return;
    }
    setStatus('saving');
    const ok = await applyFix('Move tag under parent', 'tag_health', tag.id, { action: 'move_under', parent_id: parentId });
    if (ok === undefined) { setStatus('idle'); return; }
    setStatus('done');
    setTimeout(() => onResolved(tag.id), 400);
  };

  const dismiss = async () => {
    setStatus('saving');
    const ok = await applyFix('Dismiss rootless tag', 'tag_health', tag.id, { action: 'dismiss' });
    if (ok === undefined) { setStatus('idle'); return; }
    setStatus('done');
    setTimeout(() => onResolved(tag.id), 400);
  };

  return (
    <div className="p-2.5 bg-[#1e1e1e] rounded border border-white/5 space-y-2">
      <div className="flex items-center justify-between gap-3">
        <div className="min-w-0 flex-1">
          <p className="text-xs text-gray-200 truncate font-medium">{tag.name}</p>
          <p className="text-xs text-gray-600 mt-0.5">
            {tag.atom_count} atom{tag.atom_count !== 1 ? 's' : ''}
          </p>
        </div>
        <div className="flex items-center gap-1 shrink-0">
          <select
            value={parentId}
            onChange={e => setParentId(e.target.value)}
            className="bg-[#161616] border border-white/10 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-purple-500 max-w-[180px]"
          >
            <option value="">Move under…</option>
            {options.map(opt => (
              <option key={opt.id} value={opt.id}>{opt.name}</option>
            ))}
          </select>
          <button
            type="button"
            onClick={move}
            disabled={!parentId || status === 'saving'}
            className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 transition-colors disabled:opacity-40 inline-flex items-center gap-1"
          >
            {status === 'saving' ? <Loader2 className="w-3 h-3 animate-spin" /> : <Check className="w-3 h-3" />}
          </button>
          <button
            type="button"
            onClick={dismiss}
            disabled={status === 'saving'}
            className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 transition-colors disabled:opacity-40 inline-flex items-center gap-1"
            title="Leave at root — won't be flagged again"
          >
            <EyeOff className="w-3 h-3" />
          </button>
        </div>
      </div>
    </div>
  );
}
