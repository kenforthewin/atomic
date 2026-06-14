import { forwardRef } from 'react';
import type { ButtonHTMLAttributes } from 'react';
import { cn } from '../../lib/cn';
import { Spinner } from './Spinner';

export type ButtonVariant = 'primary' | 'secondary' | 'ghost';
export type ButtonSize = 'sm' | 'md';

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  /** Shows a spinner, disables the button, and announces a busy state. */
  loading?: boolean;
  fullWidth?: boolean;
}

const base =
  'group inline-flex items-center justify-center gap-2.5 font-medium rounded-xl ' +
  'transition-all focus-visible:outline-none disabled:opacity-60 disabled:pointer-events-none ' +
  'disabled:cursor-not-allowed';

const variants: Record<ButtonVariant, string> = {
  primary:
    'text-white bg-accent hover:bg-accent-dark hover:shadow-lg hover:shadow-accent/20',
  secondary:
    'text-text-primary bg-bg-white border border-border hover:border-accent/30 ' +
    'hover:bg-accent-subtle/50',
  ghost:
    'text-text-secondary hover:text-text-primary hover:bg-bg-tertiary/60',
};

const sizes: Record<ButtonSize, string> = {
  sm: 'px-4 py-2 text-sm rounded-lg',
  md: 'px-7 py-3.5 text-base',
};

/**
 * The website's button, ported faithfully. `primary` is the filled purple CTA;
 * `secondary` is the warm-paper outline; `ghost` is a quiet text action. The
 * `loading` state swaps in a spinner while keeping the label for layout
 * stability and announces `aria-busy`.
 */
export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  (
    {
      variant = 'primary',
      size = 'md',
      loading = false,
      fullWidth = false,
      disabled,
      className,
      children,
      type = 'button',
      ...props
    },
    ref,
  ) => {
    return (
      <button
        ref={ref}
        type={type}
        disabled={disabled || loading}
        aria-busy={loading || undefined}
        className={cn(
          base,
          variants[variant],
          sizes[size],
          fullWidth && 'w-full',
          className,
        )}
        {...props}
      >
        {loading && (
          <Spinner
            className={size === 'sm' ? 'w-4 h-4' : 'w-5 h-5'}
            label="Working"
          />
        )}
        {children}
      </button>
    );
  },
);

Button.displayName = 'Button';
