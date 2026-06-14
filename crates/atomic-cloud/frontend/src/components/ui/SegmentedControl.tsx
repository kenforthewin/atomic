import { cn } from '../../lib/cn';

export interface Segment<T extends string> {
  value: T;
  label: string;
  /** Optional sub-label rendered under the main label. */
  description?: string;
}

interface SegmentedControlProps<T extends string> {
  label: string;
  segments: Array<Segment<T>>;
  value: T;
  onChange: (value: T) => void;
  /** Disable every option (e.g. while a write is in flight). */
  disabled?: boolean;
  className?: string;
}

/**
 * An accessible radio-group rendered as a segmented set of cards — used for the
 * provider mode (Managed vs Bring-your-own) and the BYOK provider choice. Each
 * segment is a real radio (keyboard + screen-reader friendly); the selected
 * one warms its border to the accent, matching the website's card hover.
 */
export function SegmentedControl<T extends string>({
  label,
  segments,
  value,
  onChange,
  disabled = false,
  className,
}: SegmentedControlProps<T>) {
  return (
    <div role="radiogroup" aria-label={label} className={cn('flex flex-col gap-2', className)}>
      <span className="text-sm font-medium text-text-primary">{label}</span>
      <div className="grid gap-3 sm:grid-cols-2">
        {segments.map((seg) => {
          const selected = seg.value === value;
          return (
            <label
              key={seg.value}
              className={cn(
                'relative flex cursor-pointer flex-col gap-0.5 rounded-xl border p-4 transition-all',
                // Keyboard focus lands on the visually-hidden radio; mirror it
                // onto the visible card with the app's accent focus ring so a
                // tabbing/arrowing user can see where they are. Distinct from
                // selection (border/background warming), which can coexist.
                'has-[:focus-visible]:outline has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-accent',
                selected
                  ? 'border-accent/50 bg-accent-subtle/60 shadow-sm'
                  : 'border-border bg-bg-white hover:border-accent/30 hover:bg-accent-subtle/30',
                disabled && 'cursor-not-allowed opacity-60',
              )}
            >
              <input
                type="radio"
                name={label}
                value={seg.value}
                checked={selected}
                disabled={disabled}
                onChange={() => onChange(seg.value)}
                className="sr-only"
              />
              <span className="flex items-center justify-between">
                <span className="text-sm font-medium text-text-primary">{seg.label}</span>
                <span
                  aria-hidden="true"
                  className={cn(
                    'flex h-4 w-4 items-center justify-center rounded-full border',
                    selected ? 'border-accent' : 'border-border',
                  )}
                >
                  {selected && <span className="h-2 w-2 rounded-full bg-accent" />}
                </span>
              </span>
              {seg.description && (
                <span className="text-xs text-text-muted leading-relaxed">{seg.description}</span>
              )}
            </label>
          );
        })}
      </div>
    </div>
  );
}
