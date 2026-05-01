import { useState } from 'react';
import { Loader2, Check } from 'lucide-react';
import { getTransport } from '../../../../lib/transport';
import type { ItemStatus } from './types';

export interface BrokenLink {
  raw: string;
  target: string;
  kind: 'wikilink' | 'markdown' | string;
}

export interface BrokenLinkAtom {
  atom_id: string;
  atom_title: string;
  links: BrokenLink[];
}

interface LinkRowProps {
  link: BrokenLink;
  atomId: string;
  onRemoved: () => void;
  onIgnore: () => void;
}

function LinkRow({ link, atomId, onRemoved, onIgnore }: LinkRowProps) {
  const [status, setStatus] = useState<ItemStatus>('idle');
  const [error, setError] = useState<string | null>(null);

  const removeLink = async () => {
    setStatus('saving');
    setError(null);
    try {
      await getTransport().invoke('apply_health_item_fix', {
        check: 'broken_internal_links',
        item_id: atomId,
        action: 'remove_link',
        content: link.raw,
      });
      setStatus('done');
      setTimeout(() => onRemoved(), 400);
    } catch (e) {
      setStatus('error');
      setError(e instanceof Error ? e.message : 'Failed to remove link');
    }
  };

  const dismiss = async () => {
    setStatus('saving');
    setError(null);
    try {
      await getTransport().invoke('apply_health_item_fix', {
        check: 'broken_internal_links',
        item_id: atomId,
        action: 'dismiss',
      });
      setStatus('done');
      setTimeout(() => onIgnore(), 400);
    } catch (e) {
      setStatus('error');
      setError(e instanceof Error ? e.message : 'Failed to dismiss');
    }
  };

  return (
    <div>
      <div className="group flex items-center justify-between py-1.5 gap-2">
        <div className="flex items-center gap-2 min-w-0 flex-1">
          <code className="text-xs text-yellow-300/80 bg-[#161616] rounded px-1.5 py-0.5 truncate max-w-xs">{link.raw}</code>
          <span className="text-xs text-gray-600 truncate">→ {link.target}</span>
          <span className={`px-1.5 py-0.5 rounded text-[10px] shrink-0 ${link.kind === 'wikilink' ? 'bg-purple-900/40 text-purple-300' : 'bg-gray-800 text-gray-400'}`}>{link.kind}</span>
        </div>
        <div className="opacity-0 group-hover:opacity-100 transition-opacity flex gap-1.5 shrink-0">
          {status === 'done' ? (
            <Check className="w-3 h-3 text-green-500 self-center" />
          ) : (
            <>
              <button
                type="button"
                onClick={removeLink}
                disabled={status === 'saving'}
                className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40 inline-flex items-center gap-1"
              >
                {status === 'saving' ? <Loader2 className="w-3 h-3 animate-spin" /> : null}
                Remove link
              </button>
              <button
                type="button"
                onClick={dismiss}
                disabled={status === 'saving'}
                className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 disabled:opacity-40"
              >
                Ignore
              </button>
            </>
          )}
        </div>
      </div>
      {error && <p className="text-xs text-red-400 mt-1">{error}</p>}
    </div>
  );
}

interface AtomRowProps {
  atom: BrokenLinkAtom;
  selected: boolean;
  onToggleSelect: () => void;
  onResolved: (atomId: string) => void;
}

function AtomRow({ atom, selected, onToggleSelect, onResolved }: AtomRowProps) {
  const [removedLinks, setRemovedLinks] = useState<Set<string>>(new Set());
  const [atomStatus, setAtomStatus] = useState<ItemStatus>('idle');
  const [error, setError] = useState<string | null>(null);

  const visibleLinks = atom.links.filter(l => !removedLinks.has(l.raw));

  const dismissAtom = async () => {
    setAtomStatus('saving');
    setError(null);
    try {
      await getTransport().invoke('apply_health_item_fix', {
        check: 'broken_internal_links',
        item_id: atom.atom_id,
        action: 'dismiss',
      });
      setAtomStatus('done');
      setTimeout(() => onResolved(atom.atom_id), 400);
    } catch (e) {
      setAtomStatus('error');
      setError(e instanceof Error ? e.message : 'Failed to dismiss atom');
    }
  };

  if (atomStatus === 'done') return null;

  return (
    <div className="p-2.5 bg-[#1e1e1e] rounded border border-white/5 space-y-1">
      <div className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={selected}
          onChange={onToggleSelect}
          className="h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none shrink-0"
        />
        <div className="flex-1 min-w-0">
          <p className="text-xs text-gray-200 truncate font-medium">
            {atom.atom_title || <span className="italic text-gray-500">Untitled atom</span>}
          </p>
          <p className="text-xs text-gray-600 mt-0.5">
            {visibleLinks.length} broken link{visibleLinks.length !== 1 ? 's' : ''}
          </p>
        </div>
        <button
          type="button"
          onClick={dismissAtom}
          disabled={atomStatus === 'saving'}
          className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 disabled:opacity-40 shrink-0 inline-flex items-center gap-1"
        >
          {atomStatus === 'saving' ? <Loader2 className="w-3 h-3 animate-spin" /> : null}
          Ignore atom
        </button>
      </div>
      {visibleLinks.length > 0 && (
        <div className="ml-5 border-l border-white/5 pl-3 space-y-0.5">
          {visibleLinks.map(link => (
            <LinkRow
              key={link.raw}
              link={link}
              atomId={atom.atom_id}
              onRemoved={() => {
                setRemovedLinks(prev => new Set(prev).add(link.raw));
                const remaining = visibleLinks.length - 1;
                if (remaining === 0) onResolved(atom.atom_id);
              }}
              onIgnore={() => onResolved(atom.atom_id)}
            />
          ))}
        </div>
      )}
      {error && <p className="text-xs text-red-400 mt-1">{error}</p>}
    </div>
  );
}

interface Props {
  data: Record<string, unknown>;
  onResolved: () => void;
}

export function BrokenLinksSection({ data, onResolved }: Props) {
  const atomList = (data.broken_link_list as BrokenLinkAtom[] | undefined) ?? [];
  const [removed, setRemoved] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState(0);

  const visible = atomList.filter(a => !removed.has(a.atom_id));

  const handleResolved = (atomId: string) => {
    setRemoved(prev => new Set(prev).add(atomId));
    setSelected(prev => { const n = new Set(prev); n.delete(atomId); return n; });
    onResolved();
  };

  const bulkDismiss = async () => {
    setBusy(true);
    setProgress(0);
    try {
      const items = Array.from(selected).map(id => ({
        check: 'broken_internal_links',
        item_id: id,
        action: 'dismiss',
      }));
      const resp = await getTransport().invoke<{ results: Array<{ item_id: string; ok: boolean; error?: string }> }>(
        'health_fix_batch',
        { items },
      );
      const okIds = new Set(resp.results.filter(r => r.ok).map(r => r.item_id));
      setProgress(okIds.size);
      setRemoved(prev => { const next = new Set(prev); okIds.forEach(id => next.add(id)); return next; });
      okIds.forEach(() => onResolved());
      setSelected(new Set());
      const failed = resp.results.filter(r => !r.ok);
      if (failed.length) console.warn('Batch fix partial failure:', failed);
    } finally {
      setBusy(false);
    }
  };

  if (visible.length === 0) {
    return <p className="text-xs text-gray-500 text-center py-8">No broken internal links — all clear</p>;
  }

  return (
    <div className="relative space-y-2 pb-12">
      {visible.map(atom => (
        <AtomRow
          key={atom.atom_id}
          atom={atom}
          selected={selected.has(atom.atom_id)}
          onToggleSelect={() => setSelected(prev => {
            const n = new Set(prev);
            if (n.has(atom.atom_id)) n.delete(atom.atom_id); else n.add(atom.atom_id);
            return n;
          })}
          onResolved={handleResolved}
        />
      ))}

      {selected.size > 0 && (
        <div className="sticky bottom-0 -mx-5 px-5 py-2 bg-[#1a1a1a] border-t border-white/10 flex items-center justify-between">
          <span className="text-xs text-gray-400">{selected.size} selected</span>
          <div className="flex gap-1.5">
            <button
              onClick={() => setSelected(new Set())}
              className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5"
            >
              Clear
            </button>
            <button
              onClick={bulkDismiss}
              disabled={busy}
              className="px-2 py-1 rounded text-xs text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40"
            >
              {busy ? `Dismissing ${progress}/${selected.size}…` : `Ignore ${selected.size}`}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
