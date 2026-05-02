import { useState, useEffect } from 'react';
import { Loader2, Check, Unlink } from 'lucide-react';
import type { ItemStatus } from './types';
import { runReviewAction } from './reviewActions';
import { getTransport } from '../../../../lib/transport';
import { toast } from '../../../../stores/toasts';

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
  const [loadingSuggestions, setLoadingSuggestions] = useState(false);

  useEffect(() => {
    if (!picking || query.trim().length < 2) { setSuggestions([]); return; }
    const t = window.setTimeout(async () => {
      setLoadingSuggestions(true);
      try {
        const resp = await runReviewAction({
          label: 'Search suggestions',
          command: 'health_broken_link_suggest',
          args: { q: query.trim(), limit: 5 },
        }) as { suggestions: Suggestion[] } | undefined;
        if (resp) setSuggestions(resp.suggestions);
      } finally { setLoadingSuggestions(false); }
    }, 200);
    return () => window.clearTimeout(t);
  }, [query, picking]);

  const openPicker = () => { setQuery(link.target); setPicking(true); };

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

  const autoFixLlm = async () => {
    setStatus('saving');
    type AutoFixResult = { outcome: 'relinked' | 'removed' | 'skipped'; target_atom_id?: string; confidence?: number; reason?: string };
    const result = await runReviewAction({
      label: 'Auto-fix (LLM)',
      command: 'apply_health_item_fix',
      args: { check: 'broken_internal_links', item_id: atomId, action: 'auto_resolve', content: link.raw },
    }) as AutoFixResult | undefined;
    if (result === undefined) { setStatus('idle'); return; }
    if (result.outcome === 'relinked') {
      toast.success('Link relinked', { detail: result.reason });
      setStatus('done');
      setTimeout(() => onRemoved(), 400);
    } else if (result.outcome === 'removed') {
      toast.success('Link removed', { detail: result.reason });
      setStatus('done');
      setTimeout(() => onRemoved(), 400);
    } else {
      toast.info('Skipped', { detail: result.reason ?? 'LLM could not determine a target' });
      setStatus('idle');
    }
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

  if (status === 'done') {
    return (
      <div className="flex items-center gap-2 py-2 px-3 text-xs text-gray-500">
        <Check className="w-3.5 h-3.5 text-green-500 shrink-0" />
        Resolved
      </div>
    );
  }

  return (
    <div className="py-2.5 px-3 border-b border-white/5 last:border-b-0">
      <div className="flex items-start justify-between gap-3">
        <div className="flex items-start gap-2 min-w-0 flex-1">
          <Unlink className="w-3.5 h-3.5 text-yellow-400/70 mt-0.5 shrink-0" />
          <div className="min-w-0">
            <div className="flex items-center gap-1.5 flex-wrap">
              <code
                className="text-xs text-yellow-300/80 bg-[#161616] rounded px-1.5 py-0.5 truncate max-w-[220px]"
                title={link.raw}
              >
                {link.raw}
              </code>
              <span className="text-xs text-gray-600 truncate">→ {link.target}</span>
              <span className={`px-1.5 py-0.5 rounded text-[10px] shrink-0 ${link.kind === 'wikilink' ? 'bg-purple-900/40 text-purple-300' : 'bg-gray-800 text-gray-400'}`}>
                {link.kind}
              </span>
            </div>
          </div>
        </div>
        <div className="flex items-center gap-1.5 shrink-0 flex-wrap justify-end">
          <button
            type="button"
            onClick={autoFixLlm}
            disabled={status === 'saving'}
            className="px-2 py-1 rounded text-[11px] text-white bg-purple-600 hover:bg-purple-500 disabled:opacity-40 inline-flex items-center gap-1"
            title="Let the LLM pick the best target or remove the link"
            aria-label="Auto-fix with LLM"
          >
            {status === 'saving' ? <Loader2 className="w-3 h-3 animate-spin" /> : null}
            Auto-fix (LLM)
          </button>
          <button
            type="button"
            onClick={openPicker}
            disabled={status === 'saving'}
            className="px-2 py-1 rounded text-[11px] text-gray-300 bg-[#2a2a2a] border border-white/10 hover:text-gray-100 disabled:opacity-40"
            title="Search for target atom"
            aria-label="Link to atom"
          >
            Link to…
          </button>
          <button
            type="button"
            onClick={removeLink}
            disabled={status === 'saving'}
            className="px-2 py-1 rounded text-[11px] text-gray-400 hover:text-red-300 bg-[#2a2a2a] border border-white/10 disabled:opacity-40"
            title="Remove this link from the atom"
            aria-label="Remove link"
          >
            Remove
          </button>
          <button
            type="button"
            onClick={dismiss}
            disabled={status === 'saving'}
            className="px-2 py-1 rounded text-[11px] text-gray-500 hover:text-gray-300 disabled:opacity-40"
            title="Ignore this broken link"
            aria-label="Ignore link"
          >
            Ignore
          </button>
        </div>
      </div>
      {picking && (
        <div className="mt-2 ml-5 bg-[#161616] rounded border border-white/5 p-2 space-y-1">
          <input
            type="text"
            value={query}
            onChange={e => setQuery(e.target.value)}
            placeholder="Search atoms…"
            autoFocus
            className="w-full bg-[#1e1e1e] border border-white/10 rounded px-2 py-1 text-xs text-gray-200 focus:outline-none focus:border-purple-500/60"
          />
          {loadingSuggestions && <p className="text-[10px] text-gray-500">searching…</p>}
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
            {!loadingSuggestions && suggestions.length === 0 && query.trim().length >= 2 && (
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

interface Props {
  data: { broken_link_list: BrokenLinkAtom[] };
  onResolved: () => void;
}

export function BrokenLinksSection({ data, onResolved }: Props) {
  const [resolvedAtoms, setResolvedAtoms] = useState<Set<string>>(new Set());
  const [autoFixAllBusy, setAutoFixAllBusy] = useState(false);

  const visibleAtoms = data.broken_link_list.filter(a => !resolvedAtoms.has(a.atom_id));

  const handleResolved = (atomId: string) => {
    setResolvedAtoms(prev => {
      const next = new Set(prev).add(atomId);
      if (next.size >= data.broken_link_list.length) onResolved();
      return next;
    });
  };

  const autoFixAll = async () => {
    setAutoFixAllBusy(true);
    try {
      type BatchResult = { checked: number; relinked: number; removed: number; skipped: number };
      const result = await getTransport().invoke<BatchResult>('health_broken_links_auto_resolve_all', {});
      toast.success(
        `Auto-fix complete: ${result.relinked} relinked, ${result.removed} removed, ${result.skipped} skipped`,
      );
      // Mark all visible atoms as resolved optimistically; caller will re-scan
      setResolvedAtoms(new Set(data.broken_link_list.map(a => a.atom_id)));
      onResolved();
    } catch (err) {
      toast.error('Auto-fix all failed', { detail: err instanceof Error ? err.message : String(err) });
    } finally {
      setAutoFixAllBusy(false);
    }
  };

  if (visibleAtoms.length === 0) {
    return <p className="text-xs text-gray-500 italic py-2">No broken internal links found.</p>;
  }

  return (
    <div className="space-y-3">
      {/* Auto-fix all button */}
      <div className="flex items-center justify-between">
        <p className="text-xs text-gray-500">
          {visibleAtoms.length} atom{visibleAtoms.length !== 1 ? 's' : ''} with broken links
        </p>
        <button
          type="button"
          onClick={autoFixAll}
          disabled={autoFixAllBusy}
          className="flex items-center gap-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 disabled:opacity-40 rounded text-xs text-white font-medium transition-colors"
          aria-label="Auto-fix all broken links with LLM"
        >
          {autoFixAllBusy ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : <Unlink className="w-3.5 h-3.5" />}
          {autoFixAllBusy ? 'Fixing…' : 'Auto-fix all broken links'}
        </button>
      </div>

      {/* Per-atom cards */}
      <div className="space-y-3">
        {visibleAtoms.map(atom => (
          <AtomCard
            key={atom.atom_id}
            atom={atom}
            onResolved={handleResolved}
          />
        ))}
      </div>
    </div>
  );
}

function AtomCard({
  atom,
  onResolved,
}: {
  atom: BrokenLinkAtom;
  onResolved: (atomId: string) => void;
}) {
  const [removedLinks, setRemovedLinks] = useState<Set<string>>(new Set());
  const visibleLinks = atom.links.filter(l => !removedLinks.has(l.raw));

  if (visibleLinks.length === 0) return null;

  return (
    <div className="rounded-md border border-white/5 bg-[#1e1e1e] overflow-hidden">
      <div className="px-3 py-2 bg-[#252525] border-b border-white/5 flex items-center justify-between">
        <p className="text-xs font-medium text-gray-200 truncate">{atom.atom_title || atom.atom_id}</p>
        <span className="text-[10px] text-gray-600 shrink-0 ml-2">
          {visibleLinks.length} broken link{visibleLinks.length !== 1 ? 's' : ''}
        </span>
      </div>
      <div>
        {visibleLinks.map(link => (
          <LinkRow
            key={link.raw}
            link={link}
            atomId={atom.atom_id}
            onRemoved={() => {
              setRemovedLinks(prev => new Set(prev).add(link.raw));
              const remaining = visibleLinks.filter(l => l.raw !== link.raw).length;
              if (remaining === 0) onResolved(atom.atom_id);
            }}
            onIgnore={() => onResolved(atom.atom_id)}
          />
        ))}
      </div>
    </div>
  );
}
