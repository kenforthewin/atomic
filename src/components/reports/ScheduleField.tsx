import { memo, useMemo, useState, useEffect } from 'react';
import { CustomSelect } from '../ui/CustomSelect';
import { getBrowserTimeZone, getSupportedTimeZones } from '../../lib/tz';

/// Preset identities the picker exposes. Each preset is paired with a
/// canonical cron emitter; selecting "custom" hands control over to a
/// raw cron text input. The cron format is 6-field (`SEC MIN HOUR DOM
/// MONTH DOW`, Sun=0) — matching what the runner's `cron` crate
/// accepts and what the legacy briefing migration emits.
type Preset = 'daily' | 'weekdays' | 'weekly' | 'hourly' | 'custom';

const PRESET_LABELS: Record<Preset, string> = {
  daily: 'Daily',
  weekdays: 'Weekdays',
  weekly: 'Weekly',
  hourly: 'Hourly',
  custom: 'Custom cron',
};

const WEEKDAY_LABELS = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];

/// Try to recognize the cron and surface a matching preset so editing
/// an existing report opens on the right control. Falls back to
/// 'custom' for anything we don't generate ourselves.
function detectPreset(cron: string): { preset: Preset; hour: number; minute: number; weekday: number } {
  const parts = cron.trim().split(/\s+/);
  // Accept 5- or 6-field. 5-field is shifted right (no seconds slot).
  const fields = parts.length === 6 ? parts : parts.length === 5 ? ['0', ...parts] : null;
  if (!fields) return { preset: 'custom', hour: 9, minute: 0, weekday: 1 };
  const [sec, min, hour, dom, month, dow] = fields;
  const minN = Number(min);
  const hourN = Number(hour);
  // Hourly: `0 0 * * * *` — fires at top of each hour. We accept any
  // `0 0 * * * *`-like shape (sec=0, min=0, hour=*).
  if (sec === '0' && min === '0' && hour === '*' && dom === '*' && month === '*' && dow === '*') {
    return { preset: 'hourly', hour: 0, minute: 0, weekday: 1 };
  }
  if (sec === '0' && dom === '*' && month === '*' && Number.isFinite(minN) && Number.isFinite(hourN)) {
    if (dow === '*') {
      return { preset: 'daily', hour: hourN, minute: minN, weekday: 1 };
    }
    if (dow === '1-5') {
      return { preset: 'weekdays', hour: hourN, minute: minN, weekday: 1 };
    }
    if (/^[0-6]$/.test(dow)) {
      return { preset: 'weekly', hour: hourN, minute: minN, weekday: Number(dow) };
    }
  }
  return { preset: 'custom', hour: 9, minute: 0, weekday: 1 };
}

function emit(preset: Preset, hour: number, minute: number, weekday: number, custom: string): string {
  switch (preset) {
    case 'daily':    return `0 ${minute} ${hour} * * *`;
    case 'weekdays': return `0 ${minute} ${hour} * * 1-5`;
    case 'weekly':   return `0 ${minute} ${hour} * * ${weekday}`;
    case 'hourly':   return `0 0 * * * *`;
    case 'custom':   return normalizeCustomCron(custom);
  }
}

/// Normalize a custom-cron string to the 6-field shape the backend's
/// `cron` crate accepts. Standard 5-field cron (`MIN HOUR DOM MONTH
/// DOW`) gets a leading `0 ` so the seconds field is explicit — this
/// is the most common copy-paste shape users bring in from POSIX-cron
/// references. 6-field input passes through unchanged. Everything
/// else (whitespace-only, garbage) also passes through; the
/// always-visible cron preview + the live-validation marker flag
/// invalid input either way.
function normalizeCustomCron(expr: string): string {
  const trimmed = expr.trim();
  if (!trimmed) return trimmed;
  const parts = trimmed.split(/\s+/);
  if (parts.length === 5) {
    return `0 ${parts.join(' ')}`;
  }
  return trimmed;
}

/// Minimal cron validator. Accepts 5- or 6-field expressions and
/// confirms each field consists of legal characters (digits, `*`, `/`,
/// `-`, `,`). This is intentionally loose — the authoritative validator
/// is the runner's `cron` crate server-side, which returns a 400 on
/// save if we get it wrong. Catching obviously-bad input client-side
/// just spares a round-trip for typos.
function isPlausibleCron(expr: string): boolean {
  const parts = expr.trim().split(/\s+/);
  if (parts.length !== 5 && parts.length !== 6) return false;
  return parts.every(p => /^[0-9*\/,\-?]+$/.test(p));
}

/// Compute the next 3 fire times for a cron expression in a given tz.
/// Uses the same constrained subset the read-only ScheduleStrip uses,
/// which covers all preset-emitted cron and the common custom shapes
/// (`* * *` DOM/MONTH, DOW = `*`, `1-5`, single digit, or comma list).
/// Anything outside that returns null and the preview hides itself —
/// the server-side validator is the source of truth, and showing a
/// wrong preview would mislead the user.
function nextFires(cron: string, tz: string, count: number): Date[] | null {
  const parts = cron.trim().split(/\s+/);
  const fields = parts.length === 6 ? parts : parts.length === 5 ? ['0', ...parts] : null;
  if (!fields) return null;
  const [, minF, hourF, dom, month, dowF] = fields;
  if (dom !== '*' || month !== '*') return null;

  const dows = parseDow(dowF);
  if (!dows) return null;
  const minute = Number(minF);
  const hour = hourF === '*' ? null : Number(hourF);
  if (!Number.isFinite(minute) || (hour !== null && !Number.isFinite(hour))) return null;

  const out: Date[] = [];
  const now = new Date();
  // Walk forward minute-by-minute candidates; for typical cron we hit a
  // match in <= 7 days. Capped at 10080 iterations (1 week of minutes)
  // so a pathological cron can't spin forever.
  const start = new Date(now.getTime());
  start.setSeconds(0, 0);
  for (let step = 0; step < 60 * 24 * 7 && out.length < count; step++) {
    const t = new Date(start.getTime() + step * 60_000);
    if (t <= now) continue;
    if (t.getMinutes() !== minute) continue;
    if (hour !== null && hourInTz(t, tz) !== hour) continue;
    if (!dows.has(weekdayInTz(t, tz))) continue;
    out.push(t);
  }
  return out;
}

function parseDow(field: string): Set<number> | null {
  if (field === '*' || field === '?') return new Set([0, 1, 2, 3, 4, 5, 6]);
  if (/^[0-6]$/.test(field)) return new Set([Number(field)]);
  // Range `a-b` (where the canonical "weekdays" preset emits `1-5`).
  const rangeMatch = field.match(/^([0-6])-([0-6])$/);
  if (rangeMatch) {
    const a = Number(rangeMatch[1]);
    const b = Number(rangeMatch[2]);
    const lo = Math.min(a, b);
    const hi = Math.max(a, b);
    const s = new Set<number>();
    for (let i = lo; i <= hi; i++) s.add(i);
    return s;
  }
  // Comma-separated list (e.g. `1,3,5`).
  if (/^[0-6](,[0-6])*$/.test(field)) {
    return new Set(field.split(',').map(Number));
  }
  return null;
}

function hourInTz(date: Date, tz: string): number {
  try {
    const fmt = new Intl.DateTimeFormat('en-US', { timeZone: tz, hour: 'numeric', hour12: false });
    return Number(fmt.format(date));
  } catch {
    return date.getHours();
  }
}

function weekdayInTz(date: Date, tz: string): number {
  try {
    const fmt = new Intl.DateTimeFormat('en-US', { timeZone: tz, weekday: 'short' });
    const order = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
    return order.indexOf(fmt.format(date));
  } catch {
    return date.getDay();
  }
}

function formatFire(date: Date, tz: string): string {
  try {
    return new Intl.DateTimeFormat(undefined, {
      timeZone: tz, weekday: 'short', month: 'short', day: 'numeric',
      hour: 'numeric', minute: '2-digit', timeZoneName: 'short',
    }).format(date);
  } catch {
    return date.toString();
  }
}

interface ScheduleFieldProps {
  cron: string;
  tz: string | null;
  onChange: (cron: string, tz: string | null) => void;
  /// When true, render in an embedded inline mode (no border, no label).
  /// Used inside the detail-view's inline-edit panel (4c) where the
  /// surrounding card already provides chrome.
  embedded?: boolean;
}

export const ScheduleField = memo(function ScheduleField({
  cron,
  tz,
  onChange,
  embedded = false,
}: ScheduleFieldProps) {
  const initial = useMemo(() => detectPreset(cron), [cron]);
  const [preset, setPreset] = useState<Preset>(initial.preset);
  const [hour, setHour] = useState(initial.hour);
  const [minute, setMinute] = useState(initial.minute);
  const [weekday, setWeekday] = useState(initial.weekday);
  const [customCron, setCustomCron] = useState(cron);

  const resolvedTz = tz ?? getBrowserTimeZone();
  const supportedTzs = useMemo(() => getSupportedTimeZones(), []);

  // Sync internal state when the `cron` prop changes from outside —
  // template-gallery prefill, edit-modal opening on a different
  // report, etc. `useState` initializers only fire on mount, so
  // without this the modal stays seeded with the DEFAULT_FORM cron
  // ("daily 9am") even after the parent's setForm swaps to the
  // template's "weekly Monday 9am".
  //
  // Idempotent against the user's own edits: when the user changes a
  // control, emit → onChange → parent state → `cron` prop loops back
  // at the same value `detectPreset` will return, so the setState
  // calls below are no-ops (React skips setState when the value is
  // referentially equal). Only externally-initiated prop changes
  // produce real updates.
  useEffect(() => {
    const next = detectPreset(cron);
    setPreset(next.preset);
    setHour(next.hour);
    setMinute(next.minute);
    setWeekday(next.weekday);
    if (next.preset === 'custom') {
      setCustomCron(cron);
    }
  }, [cron]);

  // Lift changes to the parent whenever any control changes. The parent
  // is the source of truth for `cron` + `tz`; this component just
  // surfaces a friendlier UI on top.
  useEffect(() => {
    const next = emit(preset, hour, minute, weekday, customCron);
    if (next !== cron) onChange(next, tz);
    // Intentionally only re-emit when the inputs change. Reading `cron`
    // for the diff check keeps us idempotent without re-firing onChange
    // when the parent echoes our value back.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [preset, hour, minute, weekday, customCron]);

  const previewCron = emit(preset, hour, minute, weekday, customCron);
  const previewValid = isPlausibleCron(previewCron);
  const previewFires = useMemo(
    () => (previewValid ? nextFires(previewCron, resolvedTz, 3) : null),
    [previewCron, resolvedTz, previewValid]
  );

  return (
    <div className={embedded ? '' : 'flex flex-col gap-3'}>
      {!embedded && (
        <label className="text-xs font-medium uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
          Schedule
        </label>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <div className="min-w-[140px]">
          <CustomSelect
            value={preset}
            onChange={(v) => setPreset(v as Preset)}
            options={(['daily', 'weekdays', 'weekly', 'hourly', 'custom'] as Preset[]).map(p => ({
              value: p, label: PRESET_LABELS[p],
            }))}
          />
        </div>

        {(preset === 'daily' || preset === 'weekdays') && (
          <TimeOfDayInput hour={hour} minute={minute} onChange={(h, m) => { setHour(h); setMinute(m); }} />
        )}

        {preset === 'weekly' && (
          <>
            <div className="min-w-[140px]">
              <CustomSelect
                value={String(weekday)}
                onChange={(v) => setWeekday(Number(v))}
                options={WEEKDAY_LABELS.map((label, i) => ({ value: String(i), label }))}
              />
            </div>
            <TimeOfDayInput hour={hour} minute={minute} onChange={(h, m) => { setHour(h); setMinute(m); }} />
          </>
        )}

        {preset === 'hourly' && (
          <span className="text-xs text-[var(--color-text-tertiary)]">at the top of every hour</span>
        )}

        {preset === 'custom' && (
          <input
            type="text"
            value={customCron}
            onChange={(e) => setCustomCron(e.target.value)}
            placeholder="0 0 9 * * 1-5"
            className={`
              flex-1 min-w-[200px] font-mono text-sm px-2.5 py-1.5 rounded-md
              bg-[var(--color-bg-input)] border border-[var(--color-border)]
              text-[var(--color-text-primary)]
              focus:outline-none focus:ring-1 focus:ring-[var(--color-accent)]
              ${!previewValid && customCron ? 'border-red-500/60' : ''}
            `}
            spellCheck={false}
          />
        )}
      </div>

      <div className="flex flex-wrap items-center gap-2">
        <label className="text-[10px] uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
          Timezone
        </label>
        <input
          type="text"
          list="report-schedule-timezones"
          value={resolvedTz}
          onChange={(e) => onChange(previewCron, e.target.value || null)}
          className="
            min-w-[220px] font-mono text-sm px-2.5 py-1.5 rounded-md
            bg-[var(--color-bg-input)] border border-[var(--color-border)]
            text-[var(--color-text-primary)]
            focus:outline-none focus:ring-1 focus:ring-[var(--color-accent)]
          "
          spellCheck={false}
        />
        <datalist id="report-schedule-timezones">
          {supportedTzs.map((z) => <option key={z} value={z} />)}
        </datalist>
      </div>

      {/* Always-visible canonical cron, mono, beneath the picker. Doubles
          as a "what-am-I-emitting" tell when debugging schedules. */}
      <div className="flex items-baseline gap-2">
        <span className="text-[10px] uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">Cron</span>
        <code className="font-mono text-[12px] text-[var(--color-text-secondary)] tabular-nums">
          {previewCron || <span className="opacity-50">(empty)</span>}
        </code>
        {!previewValid && customCron && (
          <span className="text-[11px] text-red-400">Invalid cron format</span>
        )}
      </div>

      {previewFires && previewFires.length > 0 && (
        <div className="flex flex-col gap-0.5">
          <span className="text-[10px] uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">Next 3 fires</span>
          {previewFires.map((d, i) => (
            <span key={i} className="font-mono text-[11px] text-[var(--color-text-secondary)] tabular-nums">
              {formatFire(d, resolvedTz)}
            </span>
          ))}
        </div>
      )}

      {previewValid && previewFires !== null && previewFires.length === 0 && (
        <span className="text-[11px] text-[var(--color-text-tertiary)]">No fires in the next 7 days.</span>
      )}
    </div>
  );
});

interface TimeOfDayInputProps {
  hour: number;
  minute: number;
  onChange: (hour: number, minute: number) => void;
}

/// Simple HH:MM input that emits two integers. Uses `<input type="time">`
/// for native keyboard + picker on supported platforms, with a graceful
/// degrade to text on older webviews (parses HH:MM either way).
function TimeOfDayInput({ hour, minute, onChange }: TimeOfDayInputProps) {
  const value = `${String(hour).padStart(2, '0')}:${String(minute).padStart(2, '0')}`;
  return (
    <input
      type="time"
      value={value}
      onChange={(e) => {
        const [h, m] = e.target.value.split(':');
        const hn = Number(h);
        const mn = Number(m);
        if (Number.isFinite(hn) && Number.isFinite(mn)) {
          onChange(hn, mn);
        }
      }}
      className="
        font-mono text-sm px-2.5 py-1.5 rounded-md
        bg-[var(--color-bg-input)] border border-[var(--color-border)]
        text-[var(--color-text-primary)]
        focus:outline-none focus:ring-1 focus:ring-[var(--color-accent)]
      "
    />
  );
}
