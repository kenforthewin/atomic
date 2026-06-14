import type { ReactNode } from 'react';
import { NodeGraphBackdrop } from '../components/NodeGraphBackdrop';

interface AuthLayoutProps {
  /** Serif eyebrow + headline shown above the card. */
  eyebrow: string;
  title: ReactNode;
  subtitle?: ReactNode;
  children: ReactNode;
  /** Quiet footer slot below the card (e.g. the cross-link to the other flow). */
  footer?: ReactNode;
}

/**
 * The centered card shell shared by /signup and /login. A node-graph backdrop
 * sits behind a narrow column: serif eyebrow + headline, then the form card,
 * then an optional quiet footer link. Generous whitespace, warm paper.
 */
export function AuthLayout({
  eyebrow,
  title,
  subtitle,
  children,
  footer,
}: AuthLayoutProps) {
  return (
    <section className="relative overflow-hidden">
      <NodeGraphBackdrop />
      <div className="relative max-w-md mx-auto px-6 py-16 md:py-24">
        <div className="fade-in-up">
          <p className="text-xs font-medium tracking-wide uppercase text-text-muted mb-3">
            {eyebrow}
          </p>
          <h1 className="font-display text-4xl md:text-5xl leading-[1.1] tracking-tight mb-3">
            {title}
          </h1>
          {subtitle && (
            <p className="text-text-secondary leading-relaxed mb-8">{subtitle}</p>
          )}
          <div className="mt-8">{children}</div>
          {footer && (
            <div className="mt-8 text-center text-sm text-text-muted">{footer}</div>
          )}
        </div>
      </div>
    </section>
  );
}
