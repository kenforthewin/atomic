import { cn } from '../../lib/cn';

interface LogoProps {
  /** Show the full wordmark (default) or just the node-graph mark. */
  variant?: 'wordmark' | 'mark';
  className?: string;
}

/**
 * The Atomic brand logo, served from the copied website SVGs. The wordmark is
 * the node-graph + "atomic" lockup; the mark is the node-graph alone (used as a
 * favicon-scale glyph). Height is set via `className` (e.g. `h-6`).
 */
export function Logo({ variant = 'wordmark', className }: LogoProps) {
  const src = variant === 'mark' ? '/logo-mark.svg' : '/logo.svg';
  return (
    <img
      src={src}
      alt="Atomic"
      className={cn(variant === 'wordmark' ? 'h-6' : 'h-8 w-8', className)}
      width={variant === 'wordmark' ? 205 : 32}
      height={variant === 'wordmark' ? 60 : 32}
    />
  );
}
