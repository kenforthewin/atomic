import type { HTMLAttributes, ReactNode } from 'react';
import { cn } from '../../lib/cn';

interface CardProps extends HTMLAttributes<HTMLDivElement> {
  /** Apply the website's hover lift (border warm + soft shadow). */
  interactive?: boolean;
  children: ReactNode;
}

/** The website card: white surface, light border, gentle hover warm. */
export function Card({ interactive = false, className, children, ...props }: CardProps) {
  return (
    <div
      className={cn(
        'p-6 bg-bg-white rounded-xl border border-border-light',
        interactive && 'transition-all hover:border-accent/20 hover:shadow-md',
        className,
      )}
      {...props}
    >
      {children}
    </div>
  );
}

interface CardIconProps {
  children: ReactNode;
  className?: string;
}

/** The accent icon tile that fronts website cards. */
export function CardIcon({ children, className }: CardIconProps) {
  return (
    <div
      className={cn(
        'w-10 h-10 rounded-lg bg-accent-subtle text-accent flex items-center justify-center',
        className,
      )}
    >
      {children}
    </div>
  );
}
