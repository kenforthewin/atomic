import { useState, useEffect } from 'react';
import { Loader2, Check } from 'lucide-react';
import type { ItemStatus } from './types';
import { runReviewAction } from './reviewActions';

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

interface Suggestion {
  atom_id: string;
  title: string;
  source_url: string | null;
  score: number;
}

interface LinkRowProps {
  link: BrokenLink;
  atomId: string;
  onRemoved: () => void;
  onIgnore: () => void;
}

function LinkRow({ link, atomId, onRemoved, onIgnore }: LinkRowProps) {
  const [status, setStatus] = useState<ItemStatus>('idle');
  const [picking, setPicking] = useState(false);
  const [query, setQuery] = useState('');
  const [suggestions, setSuggestions] = useState<Suggestion[]>([]);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!picking || query.trim().length < 2) { setSuggestions([]); return; }
    const t = window.setTimeout(async () => {
      setLoading(true);
      try {
        const resp = await runReviewAction({
          label: 'Search suggestions',
          command: 'health_broken_link_suggest',
          args: { q: query.trim(), limit: 5 },
        }) as { suggestions: Suggestion[] } | undefined;
        if (resp) setSuggestions(resp.suggestions);
      } finally { setLoading(false); }
    }, 200);
    return () => window.clearTimeout(t);
  }, [query, picking]);

  const openPicker = () => {
    setQuery(link.target);
    setPicking(true);
  };

  const removeLink = async () => {
    setStatus('saving');
    const ok = await runReviewAction({
      label: 'Remove link',
      command: 'apply_health_item_fix',
      args: { check: 'broken_internal_links', item_id: atomId, action: 'remove_link', content: link.raw },
    });
    if (ok === undefined) { setStatus('idle'); return; }
    setStatus('done');
    setTimeout(() => onRemoved(), 400);
  };

  const dismiss = async () => {
    setStatus('saving');
    const ok = await runReviewAction({
      label: 'Ignore link',
      command: 'apply_health_item_fix',
      args: { check: 'broken_internal_links', item_id: atomId, action: 'dismiss' },
    });
    if (ok === undefined) { setStatus('idle'); return; }
    setStatus('done');
    setTimeout(() => onIgnore(), 400);
  };

  const relinkTo = async (targetId: string) => {
    setStatus('saving');
    const ok = await runReviewAction({
      label: 'Relink',
      command: 'apply_health_item_fix',
      args: { check: 'broken_internal_links', item_id: atomId, action: 'relink', content: link.raw, into_tag_id: targetId },
    });
    if (ok === undefined) { setStatus('idle'); return; }
    setStatus('done');
    setPicking(false);
    setTimeout(() => onRemoved(), 400);
  };

  return (
    <div>
      <div className="flex items-center justify-between py-1.5 gap-2">
        <div className="flex items-center gap-2 min-w-0 flex-1">
          <code className="text-xs text-yellow-300/80 bg-[#161616] rounded px-1.5 py-0.5 truncate max-w-xs">{link.raw}</code>
          <span className="text-xs text-gray-600 truncate">→ {link.target}</span>
          <span className={`px-1.5 py-0.5 rounded text-[10px] shrink-0 ${link.kind === 'wikilink' ? 'bg-purple-900/40 text-purple-300' : 'bg-gray-800 text-gray-400'}`}>{link.kind}</span>
        </div>
        <div className="flex gap-1.5 shrink-0">
          {status === 'done' ? (
            <Check className="w-3 h-3 text-green-500 self-center" />
          ) : (
            <>
              <button
                type="button"
                onClick={openPicker}
                disabled={status === 'saving'}
                className="px-1.5 py-0.5 rounded text-[11px] text-gray-300 bg-[#2a2a2a] border border-white/5 hover:text-gray-100 disabled:opacity-40"
              >
                Link…
              </button>
              <button
                type="button"
                onClick={removeLink}
                disabled={status === 'saving'}
                className="px-1.5 py-0.5 rounded text-[11px] text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40 inline-flex items-center gap-1"
              >
                {status === 'saving' ? <Loader2 className="w-3 h-3 animate-spin" /> : null}
                Remove link
              </button>
              <button
                type="button"
                onClick={dismiss}
                disabled={status === 'saving'}
                className="px-1.5 py-0.5 rounded text-[11px] text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 disabled:opacity-40"
              >
                Ignore
              </button>
            </>
          )}
        </div>
      </div>
      {picking && (
        <div className="mt-1 ml-6 bg-[#161616] rounded border border-white/5 p-2 space-y-1">
          <input
            type="text"
            value={query}
            onChange={e => setQuery(e.target.value)}
            placeholder="Search atoms…"
            autoFocus
            className="w-full bg-[#1e1e1e] border border-white/10 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-purple-500/60"
          />
          {loading && <p className="text-[10px] text-gray-500">searching…</p>}
          <ul className="space-y-0.5 max-h-48 overflow-y-auto">
            {suggestions.map(s => (
              <li key={s.atom_id}>
                <button
                  type="button"
                  onClick={() => relinkTo(s.atom_id)}
                  className="w-full text-left px-2 py-1 rounded hover:bg-purple-900/20 transition-colors"
                >
                  <p className="text-xs text-gray-200 truncate">{s.title || s.atom_id}</p>
                  {s.source_url && <p className="text-[10px] text-gray-600 truncate">{s.source_url}</p>}
                </button>
              </li>
            ))}
            {!loading && suggestions.length === 0 && query.trim().length >= 2 && (
              <li className="text-[10px] text-gray-500 italic px-2">No matches.</li>
            )}
          </ul>
          <div className="flex justify-end pt-1">
            <button type="button" onClick={() => setPicking(false)} className="px-1.5 py-0.5 rounded text-[11px] text-gray-400 hover:text-gray-200">Cancel</button>
          </div>
        </div>
      )}
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

  const visibleLinks = atom.links.filter(l => !removedLinks.has(l.raw));

  const dismissAtom = async () => {
    setAtomStatus('saving');
    const ok = await runReviewAction({
      label: 'Ignore atom',
      command: 'apply_health_item_fix',
      args: { check: 'broken_internal_links', item_id: atom.atom_id, action: 'dismiss' },
    });
    if (ok === undefined) { setAtomStatus('idle'); return; }
    setAtomStatus('done');
    setTimeout(() => onResolved(atom.atom_id), 400);
  };

  if (atomStatus === 'done') return null;

  return (
    <div className="py-2 border-b border-white/5 last:border-b-0">
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
        <div className="pl-6 space-y-0.5 mt-1">
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
    </div>
  );
}

interface Props {
  data: { broken_link_list: BrokenLinkAtom[] };
  onResolved: () => void;
}

export function BrokenLinksSection({ data, onResolved }: Props) {
  const [resolvedAtoms, setResolvedAtoms] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [bulkStatus, setBulkStatus] = useState<ItemStatus>('idle');

  const visibleAtoms = data.broken_link_list.filter(a => !resolvedAtoms.has(a.atom_id));

  const handleResolved = (atomId: string) => {
    setResolvedAtoms(prev => {
      const next = new Set(prev).add(atomId);
      if (next.size === data.broken_link_list.length) onResolved();
      return next;
    });
  };

  const toggleSelect = (atomId: string) => {
    setSelected(prev => {
      const next = new Set(prev);
      if (next.has(atomId)) next.delete(atomId);
      else next.add(atomId);
      return next;
    });
  };

  const toggleAll = () => {
    if (selected.size === visibleAtoms.length) setSelected(new Set());
    else setSelected(new Set(visibleAtoms.map(a => a.atom_id)));
  };

  const dismissSelected = async () => {
    if (selected.size === 0) return;
    setBulkStatus('saving');
    const results = await Promise.all(
      [...selected].map(id =>
        runReviewAction({
          label: 'Ignore selected',
          command: 'apply_health_item_fix',
          args: { check: 'broken_internal_links', item_id: id, action: 'dismiss' },
        }),
      ),
    );
    const anyFailed = results.some(r => r === undefined);
    if (anyFailed) {
      setBulkStatus('idle');
      return;
    }
    setBulkStatus('done');
    setResolvedAtoms(prev => {
      const next = new Set(prev);
      selected.forEach(id => next.add(id));
      return next;
    });
    setSelected(new Set());
    setTimeout(() => { setBulkStatus('idle'); }, 400);
  };

  if (visibleAtoms.length === 0) {
    return <p className="text-xs text-gray-500 italic py-2">No broken internal links found.</p>;
  }

  return (
    <div>
      {visibleAtoms.length > 1 && (
        <div className="flex items-center gap-2 pb-2 mb-1 border-b border-white/5">
          <input
            type="checkbox"
            checked={selected.size === visibleAtoms.length}
            onChange={toggleAll}
            className="h-3.5 w-3.5 rounded border border-white/20 bg-[#161616] checked:bg-purple-600 checked:border-purple-600 focus:outline-none"
          />
          <span className="text-xs text-gray-500 flex-1">
            {selected.size > 0 ? `${selected.size} selected` : 'Select all'}
          </span>
          {selected.size > 0 && (
            <button
              type="button"
              onClick={dismissSelected}
              disabled={bulkStatus === 'saving'}
              className="px-2 py-1 rounded text-xs text-gray-400 hover:text-gray-200 bg-[#2a2a2a] border border-white/5 disabled:opacity-40 inline-flex items-center gap-1"
            >
              {bulkStatus === 'saving' ? <Loader2 className="w-3 h-3 animate-spin" /> : null}
              Ignore selected
            </button>
          )}
        </div>
      )}
      <div>
        {visibleAtoms.map(atom => (
          <AtomRow
            key={atom.atom_id}
            atom={atom}
            selected={selected.has(atom.atom_id)}
            onToggleSelect={() => toggleSelect(atom.atom_id)}
            onResolved={handleResolved}
          />
        ))}
      </div>
    </div>
  );
}
