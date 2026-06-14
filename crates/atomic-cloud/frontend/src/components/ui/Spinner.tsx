import { cn } from '../../lib/cn';

interface SpinnerProps {
  className?: string;
  /** Accessible label; defaults to "Loading". */
  label?: string;
}

/** A small purposeful spinner that respects `currentColor`. */
export function Spinner({ className, label = 'Loading' }: SpinnerProps) {
  return (
    <svg
      className={cn('animate-spin', className)}
      viewBox="0 0 24 24"
      fill="none"
      role="status"
      aria-label={label}
      width="20"
      height="20"
    >
      <circle
        className="opacity-25"
        cx="12"
        cy="12"
        r="10"
        stroke="currentColor"
        strokeWidth="4"
      />
      <path
        className="opacity-90"
        fill="currentColor"
        d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z"
      />
    </svg>
  );
}
