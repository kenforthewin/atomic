import { memo, useMemo } from 'react';
import { TagSelector } from '../tags/TagSelector';
import { CustomSelect } from '../ui/CustomSelect';
import { useTagsStore } from '../../stores/tags';
import { Tag } from '../../stores/atoms';
import { SourceScopeWindow } from '../../stores/reports';

/// Window options exposed in the editor. The "since_last_run" sentinel
/// is a discriminated-union variant in the API; the ISO durations get
/// wrapped before they hit the wire. Keeping the option keys close to
/// the wire format means the (de)serialization stays one line.
type WindowOption =
  | 'since_last_run'
  | 'all_time'
  | 'last_24h'
  | 'last_7d'
  | 'last_30d';

const WINDOW_LABELS: Record<WindowOption, string> = {
  since_last_run: 'Since last run',
  all_time: 'All time',
  last_24h: 'Last 24 hours',
  last_7d: 'Last 7 days',
  last_30d: 'Last 30 days',
};

const WINDOW_TO_ISO: Record<Exclude<WindowOption, 'since_last_run' | 'all_time'>, string> = {
  last_24h: 'PT24H',
  last_7d: 'P7D',
  last_30d: 'P30D',
};

function windowToOption(w: SourceScopeWindow | null): WindowOption {
  if (!w) return 'all_time';
  if (w.kind === 'since_last_run') return 'since_last_run';
  switch (w.value) {
    case 'PT24H': return 'last_24h';
    case 'P7D': return 'last_7d';
    case 'P30D': return 'last_30d';
    default: return 'all_time'; // unknown duration: surface as all-time for the picker
  }
}

function optionToWindow(opt: WindowOption): SourceScopeWindow | null {
  if (opt === 'all_time') return null;
  if (opt === 'since_last_run') return { kind: 'since_last_run' };
  return { kind: 'iso_duration', value: WINDOW_TO_ISO[opt] };
}

interface ScopeFieldProps {
  label: string;
  tagIds: string[];
  window: SourceScopeWindow | null;
  onChange: (tagIds: string[], window: SourceScopeWindow | null) => void;
  /// When true, omits the "Since last run" option. The context scope
  /// makes no sense with that semantic — context is read once per run,
  /// not as a delta — and the backend rejects it on save anyway.
  hideSinceLastRun?: boolean;
}

/// Tags multi-select + time-window dropdown. v1 keeps kinds locked to
/// Captured (the only sensible default until report-of-reports chains
/// land in phase 5+), so we don't surface a kinds picker here.
export const ScopeField = memo(function ScopeField({
  label, tagIds, window, onChange, hideSinceLastRun,
}: ScopeFieldProps) {
  const tags = useTagsStore(s => s.tags);

  // Find the actual Tag objects for the currently-selected ids. The
  // TagSelector wants `Tag[]`, not `string[]`.
  const selectedTags = useMemo<Tag[]>(() => {
    const flat: Tag[] = [];
    function walk(nodes: any[]) {
      for (const n of nodes) {
        flat.push({
          id: n.id, name: n.name,
          parent_id: n.parent_id ?? null,
          created_at: n.created_at ?? '',
        });
        if (n.children) walk(n.children);
      }
    }
    walk(tags as any);
    const set = new Set(tagIds);
    return flat.filter(t => set.has(t.id));
  }, [tags, tagIds]);

  const windowOptions: WindowOption[] = hideSinceLastRun
    ? ['all_time', 'last_24h', 'last_7d', 'last_30d']
    : ['since_last_run', 'all_time', 'last_24h', 'last_7d', 'last_30d'];

  return (
    <div className="flex flex-col gap-2">
      <label className="text-xs font-medium uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
        {label}
      </label>

      <div>
        <span className="block mb-1 text-[10px] uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
          Tags{tagIds.length === 0 ? ' · all' : ''}
        </span>
        <TagSelector
          selectedTags={selectedTags}
          onTagsChange={(next) => onChange(next.map(t => t.id), window)}
        />
      </div>

      <div>
        <span className="block mb-1 text-[10px] uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
          Window
        </span>
        <div className="max-w-[220px]">
          <CustomSelect
            value={windowToOption(window)}
            onChange={(v) => onChange(tagIds, optionToWindow(v as WindowOption))}
            options={windowOptions.map(o => ({ value: o, label: WINDOW_LABELS[o] }))}
          />
        </div>
      </div>
    </div>
  );
});
