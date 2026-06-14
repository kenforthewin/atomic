import { cn } from '../lib/cn';

interface NodeGraphBackdropProps {
  className?: string;
}

/**
 * The brand signature backdrop: a faint dotted grid, pulsing accent nodes with
 * staggered delays, and hair-thin connector lines. Purely decorative —
 * `aria-hidden`, `pointer-events-none` — and motion-reduced via the global CSS.
 * Reproduced from the marketing site's hero.
 */
export function NodeGraphBackdrop({ className }: NodeGraphBackdropProps) {
  return (
    <div
      aria-hidden="true"
      className={cn('pointer-events-none absolute inset-0 overflow-hidden', className)}
    >
      {/* Dotted-grid radial gradient at ~0.03 opacity. */}
      <div className="node-grid absolute inset-0" />

      {/* Pulsing accent nodes, staggered. */}
      <span className="absolute top-20 left-[4%] w-2 h-2 rounded-full bg-accent/20 animate-pulse" />
      <span
        className="absolute bottom-16 left-[6%] w-1.5 h-1.5 rounded-full bg-accent/15 animate-pulse"
        style={{ animationDelay: '0.5s' }}
      />
      <span
        className="absolute top-16 right-[6%] w-2.5 h-2.5 rounded-full bg-accent/20 animate-pulse"
        style={{ animationDelay: '1s' }}
      />
      <span
        className="absolute top-40 right-[12%] w-1.5 h-1.5 rounded-full bg-accent/10 animate-pulse"
        style={{ animationDelay: '1.5s' }}
      />
      <span
        className="absolute bottom-20 right-[8%] w-2 h-2 rounded-full bg-accent/15 animate-pulse"
        style={{ animationDelay: '2s' }}
      />

      {/* Faint connector lines. */}
      <svg
        className="absolute inset-0 w-full h-full opacity-[0.04]"
        xmlns="http://www.w3.org/2000/svg"
      >
        <line x1="4%" y1="20%" x2="6%" y2="85%" stroke="#7c3aed" strokeWidth="1" />
        <line x1="94%" y1="15%" x2="88%" y2="40%" stroke="#7c3aed" strokeWidth="1" />
        <line x1="88%" y1="40%" x2="92%" y2="80%" stroke="#7c3aed" strokeWidth="0.5" />
      </svg>
    </div>
  );
}
