import { useState } from 'react';
import { ArrowUpRight, LogOut } from 'lucide-react';
import { Logo } from '../ui/Logo';
import { appHostLoginUrl, tenantRootUrl } from '../../lib/host';

interface AccountTopbarProps {
  /** The account's subdomain, shown beside the logo (null while loading). */
  subdomain: string | null;
}

/**
 * The fixed top bar of the authenticated dashboard: the brand, the account's
 * subdomain, a link out to the product knowledge base (the tenant root, an
 * nginx-served app separate from this dashboard), and sign-out.
 *
 * Sign-out is a real server-side revoke. The session cookie is `HttpOnly`
 * (the browser can't clear it itself) and `Domain=.<base>`, so it rides to the
 * app host as readily as any tenant subdomain — `POST /account/logout` lives
 * there (the account plane), deletes the session row, and clears the cookie via
 * `Set-Cookie ...Max-Age=0`. We send the cross-origin POST with credentials,
 * then bounce to the app-host login regardless of the outcome: the cookie is
 * cleared client-side either way, so a logged-out browser never lingers on a
 * failed delete.
 */
export function AccountTopbar({ subdomain }: AccountTopbarProps) {
  const [signingOut, setSigningOut] = useState(false);

  async function handleSignOut() {
    if (signingOut) return;
    setSigningOut(true);
    try {
      // `/account/logout` is app-host-only and the session cookie spans
      // `.<base>`, so the revoke goes cross-origin to the app host (the login
      // URL minus its `/login` path), credentials included so the cookie rides.
      await fetch(appHostLogoutUrl(), {
        method: 'POST',
        headers: { Accept: 'application/json' },
        credentials: 'include',
      });
    } catch {
      // A failed revoke still clears the cookie (the response sets Max-Age=0)
      // and we sign the browser out regardless — leaving it "signed in" would
      // be the worse outcome.
    }
    window.location.assign(appHostLoginUrl());
  }

  return (
    <header className="fixed top-0 left-0 right-0 z-50 h-16 backdrop-blur-md bg-bg-primary/80 border-b border-border-light">
      <div className="max-w-6xl mx-auto h-full px-6 flex items-center justify-between gap-4">
        <div className="flex items-center min-w-0">
          <Logo className="h-6 shrink-0" />
          {subdomain && (
            <span className="ml-3 hidden truncate text-sm text-text-muted sm:inline">
              <span className="text-text-secondary font-medium">{subdomain}</span>
              <span className="text-text-muted">’s workspace</span>
            </span>
          )}
        </div>

        <nav className="flex items-center gap-1 sm:gap-2" aria-label="Account actions">
          <a
            href={tenantRootUrl()}
            className="inline-flex items-center gap-1.5 rounded-lg px-3 py-2 text-sm font-medium text-text-secondary transition-colors hover:bg-accent-subtle/50 hover:text-accent-dark focus-visible:outline-2"
          >
            <span className="hidden sm:inline">Open knowledge base</span>
            <span className="sm:hidden">Knowledge base</span>
            <ArrowUpRight className="h-4 w-4" aria-hidden="true" />
          </a>
          <button
            type="button"
            onClick={handleSignOut}
            disabled={signingOut}
            aria-busy={signingOut || undefined}
            className="inline-flex items-center gap-1.5 rounded-lg px-3 py-2 text-sm font-medium text-text-secondary transition-colors hover:bg-bg-tertiary/60 hover:text-text-primary focus-visible:outline-2 disabled:cursor-not-allowed disabled:opacity-60"
          >
            <LogOut className="h-4 w-4" aria-hidden="true" />
            <span className="hidden sm:inline">{signingOut ? 'Signing out…' : 'Sign out'}</span>
          </button>
        </nav>
      </div>
    </header>
  );
}

/**
 * The app-host `POST /account/logout` URL — the app-host login URL with its
 * `/login` path swapped for `/account/logout`. Reuses {@link appHostLoginUrl}'s
 * host derivation (configured base domain, else the current host's first label
 * rewritten to `app`) so the two stay in lockstep.
 */
function appHostLogoutUrl(): string {
  return appHostLoginUrl().replace(/\/login(?:\?.*)?$/, '/account/logout');
}
