import { forwardRef, useId } from 'react';
import type { InputHTMLAttributes, ReactNode } from 'react';
import { cn } from '../../lib/cn';

interface FieldProps extends InputHTMLAttributes<HTMLInputElement> {
  label: string;
  /** Inline error message; sets the invalid styling + `aria-invalid`. */
  error?: string | null;
  /** Quiet helper text shown beneath the input when there's no error. */
  help?: ReactNode;
  /** Content rendered inside the field's right edge (e.g. a live preview). */
  trailing?: ReactNode;
  /** Wrapper class for layout (the `<div>` around label + input). */
  wrapperClassName?: string;
}

const inputBase =
  'w-full px-3.5 py-2.5 text-base text-text-primary bg-bg-white rounded-xl border ' +
  'placeholder:text-text-muted transition-colors ' +
  'focus:outline-none focus-visible:outline-none ' +
  'disabled:opacity-60 disabled:cursor-not-allowed';

/**
 * A labelled text input with inline error + helper copy and an accessible
 * wiring (`aria-invalid`, `aria-describedby`). The focus ring warms the border
 * to the accent, matching the website's input feel.
 */
export const Field = forwardRef<HTMLInputElement, FieldProps>(
  (
    { label, error, help, trailing, id, wrapperClassName, className, ...props },
    ref,
  ) => {
    const autoId = useId();
    const inputId = id ?? autoId;
    const errorId = `${inputId}-error`;
    const helpId = `${inputId}-help`;
    const describedBy = error ? errorId : help ? helpId : undefined;

    return (
      <div className={cn('flex flex-col gap-1.5', wrapperClassName)}>
        <label
          htmlFor={inputId}
          className="text-sm font-medium text-text-primary"
        >
          {label}
        </label>
        <div className="relative">
          <input
            ref={ref}
            id={inputId}
            aria-invalid={error ? true : undefined}
            aria-describedby={describedBy}
            className={cn(
              inputBase,
              error
                ? 'border-red-400 focus:border-red-500'
                : 'border-border focus:border-accent',
              trailing ? 'pr-3' : '',
              className,
            )}
            {...props}
          />
          {trailing && (
            <div className="pointer-events-none absolute inset-y-0 right-3 flex items-center">
              {trailing}
            </div>
          )}
        </div>
        {error ? (
          <p id={errorId} role="alert" className="text-sm text-red-600">
            {error}
          </p>
        ) : help ? (
          <p id={helpId} className="text-sm text-text-muted">
            {help}
          </p>
        ) : null}
      </div>
    );
  },
);

Field.displayName = 'Field';
