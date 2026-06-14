import type { ReactNode } from 'react';
import {
  CheckCircle2,
  Info,
  AlertTriangle,
  XCircle,
} from 'lucide-react';
import { cn } from '../../lib/cn';

export type BannerTone = 'info' | 'success' | 'warning' | 'error';

interface BannerProps {
  tone?: BannerTone;
  title?: ReactNode;
  children?: ReactNode;
  className?: string;
  /** Optional trailing action (e.g. a retry button). */
  action?: ReactNode;
}

const tones: Record<
  BannerTone,
  { wrap: string; icon: string; Icon: typeof Info; role: 'status' | 'alert' }
> = {
  info: {
    wrap: 'bg-accent-subtle border-accent-light/40 text-text-secondary',
    icon: 'text-accent',
    Icon: Info,
    role: 'status',
  },
  success: {
    wrap: 'bg-emerald-50 border-emerald-200 text-emerald-900',
    icon: 'text-emerald-600',
    Icon: CheckCircle2,
    role: 'status',
  },
  warning: {
    wrap: 'bg-amber-50 border-amber-200 text-amber-900',
    icon: 'text-amber-600',
    Icon: AlertTriangle,
    role: 'status',
  },
  error: {
    wrap: 'bg-red-50 border-red-200 text-red-900',
    icon: 'text-red-600',
    Icon: XCircle,
    role: 'alert',
  },
};

/**
 * An inline banner for info / success / warning / error states — used for the
 * auth confirmations and (later) the billing read-only / past-due / suspended
 * notices. `error` uses `role="alert"` so it's announced; the rest are polite
 * `status`.
 */
export function Banner({
  tone = 'info',
  title,
  children,
  className,
  action,
}: BannerProps) {
  const { wrap, icon, Icon, role } = tones[tone];
  return (
    <div
      role={role}
      className={cn(
        'flex items-start gap-3 p-4 rounded-xl border text-sm leading-relaxed',
        wrap,
        className,
      )}
    >
      <Icon className={cn('w-5 h-5 shrink-0 mt-0.5', icon)} aria-hidden="true" />
      <div className="min-w-0 flex-1">
        {title && <p className="font-semibold text-current">{title}</p>}
        {children && <div className={cn(title ? 'mt-1' : '')}>{children}</div>}
      </div>
      {action && <div className="shrink-0">{action}</div>}
    </div>
  );
}
