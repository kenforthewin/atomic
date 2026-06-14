import type { ReactNode } from 'react';
import { SiteNav } from '../components/SiteNav';
import { SiteFooter } from '../components/SiteFooter';

interface PublicLayoutProps {
  children: ReactNode;
}

/** Nav + footer chrome shared by every public app-host page. */
export function PublicLayout({ children }: PublicLayoutProps) {
  return (
    <div className="min-h-dvh flex flex-col bg-bg-primary text-text-primary">
      <SiteNav />
      <main className="flex-1 pt-16">{children}</main>
      <SiteFooter />
    </div>
  );
}
