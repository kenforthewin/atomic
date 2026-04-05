import { useState, useEffect, useRef, useCallback } from 'react';
import { createPortal } from 'react-dom';
import { getTransport } from '../../lib/transport';
import { useTagsStore } from '../../stores/tags';
import { useAtomsStore } from '../../stores/atoms';

// ── Types ──────────────────────────────────────────────

type EventCategory = 'embed' | 'tag' | 'atom' | 'ingest' | 'feed' | 'system' | 'import';

interface LogEntry {
  id: number;
  time: string;
  category: EventCategory;
  level: 'info' | 'success' | 'error' | 'warning' | 'muted';
  message: string;
}

interface EventLogModalProps {
  isOpen: boolean;
  onClose: () => void;
}

// ── Constants ──────────────────────────────────────────

const MAX_ENTRIES = 500;
let nextId = 0;

const CATEGORY_LABELS: Record<EventCategory, string> = {
  embed: 'Embedding',
  tag: 'Tagging',
  atom: 'Atom',
  ingest: 'Ingestion',
  feed: 'Feed',
  system: 'System',
  import: 'Import',
};

const BADGE_COLORS: Record<string, string> = {
  'embed-info': 'bg-blue-500/20 text-blue-400',
  'embed-success': 'bg-emerald-500/20 text-emerald-400',
  'embed-error': 'bg-red-500/20 text-red-400',
  'tag-success': 'bg-emerald-500/20 text-emerald-400',
  'tag-error': 'bg-red-500/20 text-red-400',
  'tag-muted': 'bg-zinc-500/20 text-zinc-400',
  'atom-info': 'bg-blue-500/20 text-blue-400',
  'ingest-info': 'bg-blue-500/20 text-blue-400',
  'ingest-success': 'bg-emerald-500/20 text-emerald-400',
  'ingest-error': 'bg-red-500/20 text-red-400',
  'feed-success': 'bg-emerald-500/20 text-emerald-400',
  'feed-error': 'bg-red-500/20 text-red-400',
  'system-warning': 'bg-yellow-500/20 text-yellow-400',
  'import-info': 'bg-blue-500/20 text-blue-400',
};

const LEVEL_TEXT_COLORS: Record<string, string> = {
  info: 'text-[var(--color-text-secondary)]',
  success: 'text-emerald-400',
  error: 'text-red-400',
  warning: 'text-yellow-400',
  muted: 'text-zinc-500',
};

// ── Helpers ────────────────────────────────────────────

function shortId(id: string): string {
  return id?.slice(0, 8) ?? '?';
}

function resolveTagNames(tagIds: string[]): string {
  if (!tagIds?.length) return '(none)';
  const tags = useTagsStore.getState().tags;
  // Build a flat lookup from the hierarchical tree
  const lookup = new Map<string, string>();
  const walk = (items: typeof tags) => {
    for (const t of items) {
      lookup.set(t.id, t.name);
      if (t.children?.length) walk(t.children);
    }
  };
  walk(tags);
  return tagIds.map(id => lookup.get(id) || shortId(id)).join(', ');
}

function resolveAtomTitle(atomId: string): string {
  const atoms = useAtomsStore.getState().atoms;
  const atom = atoms.find(a => a.id === atomId);
  return atom?.title?.slice(0, 60) || shortId(atomId);
}

function shortUrl(url: string, max = 50): string {
  if (!url) return '?';
  try {
    const u = new URL(url);
    const path = u.pathname.length > max - u.host.length
      ? u.pathname.slice(0, max - u.host.length) + '…'
      : u.pathname;
    return u.host + path;
  } catch {
    return url.length > max ? url.slice(0, max) + '…' : url;
  }
}

function timestamp(): string {
  const d = new Date();
  return d.toLocaleTimeString('en-US', { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' })
    + '.' + String(d.getMilliseconds()).padStart(3, '0');
}

function makeEntry(category: EventCategory, level: LogEntry['level'], message: string): LogEntry {
  return { id: nextId++, time: timestamp(), category, level, message };
}

// ── Component ──────────────────────────────────────────

function useQueueStats() {
  const atoms = useAtomsStore(s => s.atoms);
  const embed = { pending: 0, processing: 0, failed: 0, complete: 0 };
  const tag = { pending: 0, processing: 0, failed: 0, complete: 0, skipped: 0 };
  for (const a of atoms) {
    embed[a.embedding_status] = (embed[a.embedding_status] || 0) + 1;
    tag[a.tagging_status] = (tag[a.tagging_status] || 0) + 1;
  }
  return { total: atoms.length, embed, tag };
}

export function EventLogModal({ isOpen, onClose }: EventLogModalProps) {
  const [entries, setEntries] = useState<LogEntry[]>([]);
  const [filters, setFilters] = useState<Set<EventCategory>>(new Set(Object.keys(CATEGORY_LABELS) as EventCategory[]));
  const [isAtBottom, setIsAtBottom] = useState(true);
  const queue = useQueueStats();
  const [newCount, setNewCount] = useState(0);
  const scrollRef = useRef<HTMLDivElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const entriesRef = useRef<LogEntry[]>([]);
  const rafRef = useRef<number>();

  // Push entry to ring buffer
  const pushEntry = useCallback((entry: LogEntry) => {
    entriesRef.current = [...entriesRef.current.slice(-(MAX_ENTRIES - 1)), entry];
    // Batch renders via rAF
    if (!rafRef.current) {
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = undefined;
        setEntries([...entriesRef.current]);
      });
    }
  }, []);

  // Subscribe to all events when modal is open
  useEffect(() => {
    if (!isOpen) return;

    const transport = getTransport();
    const unsubs: (() => void)[] = [];

    // Embedding
    unsubs.push(transport.subscribe<any>('embedding-started', (p) => {
      pushEntry(makeEntry('embed', 'info', `Starting "${resolveAtomTitle(p.atom_id)}"`));
    }));
    unsubs.push(transport.subscribe<any>('embedding-complete', (p) => {
      const title = resolveAtomTitle(p.atom_id);
      if (p.status === 'failed') {
        pushEntry(makeEntry('embed', 'error', `Failed "${title}": ${p.error}`));
      } else {
        pushEntry(makeEntry('embed', 'success', `Complete "${title}"`));
      }
    }));

    // Tagging
    unsubs.push(transport.subscribe<any>('tagging-complete', (p) => {
      const title = resolveAtomTitle(p.atom_id);
      if (p.status === 'failed') {
        pushEntry(makeEntry('tag', 'error', `Failed "${title}": ${p.error}`));
      } else if (p.status === 'skipped') {
        pushEntry(makeEntry('tag', 'muted', `Skipped "${title}"`));
      } else {
        const tagNames = resolveTagNames(p.tags_extracted);
        pushEntry(makeEntry('tag', 'success', `"${title}" → ${tagNames}`));
      }
    }));

    // Atom
    unsubs.push(transport.subscribe<any>('atom-created', (p) => {
      const title = (p.title || p.snippet || p.id || '?').slice(0, 60);
      pushEntry(makeEntry('atom', 'info', `Created: "${title}"`));
    }));

    // Ingestion
    unsubs.push(transport.subscribe<any>('ingestion-fetch-started', (p) => {
      pushEntry(makeEntry('ingest', 'info', `Fetching ${shortUrl(p.url)}`));
    }));
    unsubs.push(transport.subscribe<any>('ingestion-complete', (p) => {
      pushEntry(makeEntry('ingest', 'success', `${p.title || '?'} (${shortUrl(p.url)})`));
    }));
    unsubs.push(transport.subscribe<any>('ingestion-failed', (p) => {
      pushEntry(makeEntry('ingest', 'error', `Failed ${shortUrl(p.url)}: ${p.error}`));
    }));
    unsubs.push(transport.subscribe<any>('ingestion-fetch-failed', (p) => {
      pushEntry(makeEntry('ingest', 'error', `Fetch failed ${shortUrl(p.url)}: ${p.error}`));
    }));
    unsubs.push(transport.subscribe<any>('ingestion-skipped', (p) => {
      pushEntry(makeEntry('ingest', 'muted', `Skipped ${shortUrl(p.url)}: ${p.reason}`));
    }));

    // Feed
    unsubs.push(transport.subscribe<any>('feed-poll-complete', (p) => {
      pushEntry(makeEntry('feed', 'success', `Poll: ${p.new_items} new, ${p.skipped} skipped`));
    }));
    unsubs.push(transport.subscribe<any>('feed-poll-failed', (p) => {
      pushEntry(makeEntry('feed', 'error', `Poll failed: ${p.error}`));
    }));

    // System
    unsubs.push(transport.subscribe<any>('embeddings-reset', (p) => {
      pushEntry(makeEntry('system', 'warning', `Re-embedding ${p.pending_count} atoms — ${p.reason}`));
    }));

    // Import
    unsubs.push(transport.subscribe<any>('import-progress', (p) => {
      pushEntry(makeEntry('import', 'info', `${p.current}/${p.total}: ${p.current_file}`));
    }));

    return () => {
      unsubs.forEach(fn => fn());
      if (rafRef.current) cancelAnimationFrame(rafRef.current);
    };
  }, [isOpen, pushEntry]);

  // Auto-scroll
  useEffect(() => {
    if (isAtBottom && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
      setNewCount(0);
    } else if (!isAtBottom) {
      setNewCount(prev => prev + 1);
    }
  }, [entries, isAtBottom]);

  // Track scroll position
  const handleScroll = useCallback(() => {
    if (!scrollRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = scrollRef.current;
    const atBottom = scrollHeight - scrollTop - clientHeight < 40;
    setIsAtBottom(atBottom);
    if (atBottom) setNewCount(0);
  }, []);

  // ESC to close
  useEffect(() => {
    if (!isOpen) return;
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', handleKey);
    return () => document.removeEventListener('keydown', handleKey);
  }, [isOpen, onClose]);

  // Toggle filter
  const toggleFilter = (cat: EventCategory) => {
    setFilters(prev => {
      const next = new Set(prev);
      if (next.has(cat)) next.delete(cat);
      else next.add(cat);
      return next;
    });
  };

  const scrollToBottom = () => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
      setIsAtBottom(true);
      setNewCount(0);
    }
  };

  const clearLog = () => {
    entriesRef.current = [];
    setEntries([]);
    setNewCount(0);
  };

  if (!isOpen) return null;

  const filtered = entries.filter(e => filters.has(e.category));

  return createPortal(
    <div
      ref={overlayRef}
      onClick={(e) => e.target === overlayRef.current && onClose()}
      data-modal="true"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm"
    >
      <div className="relative bg-[var(--color-bg-panel)] rounded-lg shadow-xl border border-[var(--color-border)] w-full max-w-4xl mx-4 h-[85vh] flex flex-col animate-in fade-in zoom-in-95 duration-200">
        {/* Header */}
        <div className="flex items-center justify-between px-5 py-3 border-b border-[var(--color-border)]">
          <h2 className="text-base font-semibold text-[var(--color-text-primary)]">Event Log</h2>
          <div className="flex items-center gap-2">
            <button
              onClick={clearLog}
              className="px-2.5 py-1 text-xs font-medium rounded-md text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-hover)] transition-colors"
            >
              Clear
            </button>
            <button
              onClick={onClose}
              className="text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
            >
              <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
              </svg>
            </button>
          </div>
        </div>

        {/* Filter chips */}
        <div className="flex items-center gap-1.5 px-5 py-2.5 border-b border-[var(--color-border)]">
          {(Object.keys(CATEGORY_LABELS) as EventCategory[]).map(cat => (
            <button
              key={cat}
              onClick={() => toggleFilter(cat)}
              className={`px-2.5 py-1 text-xs font-medium rounded-full transition-colors ${
                filters.has(cat)
                  ? 'bg-[var(--color-accent)] text-white'
                  : 'bg-[var(--color-bg-card)] text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]'
              }`}
            >
              {CATEGORY_LABELS[cat]}
            </button>
          ))}
        </div>

        {/* Queue status */}
        <div className="flex items-center gap-4 px-5 py-2 border-b border-[var(--color-border)] text-xs font-mono">
          <span className="text-[var(--color-text-secondary)]">{queue.total} atoms</span>
          <span className="text-[var(--color-text-secondary)]">
            Embed:
            {queue.embed.pending > 0 && <span className="ml-1 text-yellow-400">{queue.embed.pending} pending</span>}
            {queue.embed.processing > 0 && <span className="ml-1 text-blue-400">{queue.embed.processing} processing</span>}
            {queue.embed.failed > 0 && <span className="ml-1 text-red-400">{queue.embed.failed} failed</span>}
            {queue.embed.pending === 0 && queue.embed.processing === 0 && queue.embed.failed === 0 && <span className="ml-1 text-emerald-400">✓ done</span>}
          </span>
          <span className="text-[var(--color-text-secondary)]">
            Tag:
            {queue.tag.pending > 0 && <span className="ml-1 text-yellow-400">{queue.tag.pending} pending</span>}
            {queue.tag.processing > 0 && <span className="ml-1 text-blue-400">{queue.tag.processing} processing</span>}
            {queue.tag.failed > 0 && <span className="ml-1 text-red-400">{queue.tag.failed} failed</span>}
            {queue.tag.pending === 0 && queue.tag.processing === 0 && queue.tag.failed === 0 && <span className="ml-1 text-emerald-400">✓ done</span>}
          </span>
        </div>

        {/* Log area */}
        <div
          ref={scrollRef}
          onScroll={handleScroll}
          className="flex-1 overflow-y-auto px-4 py-2 font-mono text-xs leading-relaxed bg-[var(--color-bg-main)]"
        >
          {filtered.length === 0 ? (
            <div className="flex items-center justify-center h-full text-[var(--color-text-secondary)]">
              {entries.length === 0 ? 'Waiting for events…' : 'No events match filters'}
            </div>
          ) : (
            filtered.map(entry => (
              <div key={entry.id} className="flex items-start gap-2 py-0.5 hover:bg-[var(--color-bg-hover)] rounded px-1 -mx-1">
                <span className="text-[var(--color-text-secondary)] opacity-60 shrink-0 tabular-nums">
                  {entry.time}
                </span>
                <span className={`shrink-0 px-1.5 py-0 rounded text-[10px] font-semibold uppercase tracking-wider ${
                  BADGE_COLORS[`${entry.category}-${entry.level}`] || 'bg-zinc-500/20 text-zinc-400'
                }`}>
                  {entry.category}
                </span>
                <span className={LEVEL_TEXT_COLORS[entry.level] || ''}>
                  {entry.message}
                </span>
              </div>
            ))
          )}
        </div>

        {/* New events pill */}
        {!isAtBottom && newCount > 0 && (
          <button
            onClick={scrollToBottom}
            className="absolute bottom-4 left-1/2 -translate-x-1/2 px-3 py-1.5 text-xs font-medium rounded-full bg-[var(--color-accent)] text-white shadow-lg hover:brightness-110 transition-all"
          >
            ↓ {newCount} new event{newCount !== 1 ? 's' : ''}
          </button>
        )}
      </div>
    </div>,
    document.body
  );
}
