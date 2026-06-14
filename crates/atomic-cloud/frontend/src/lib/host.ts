/**
 * Host / route-context detection.
 *
 * The same build serves two contexts, switched by the request `Host`:
 *
 * - **App host** — the bare base domain and `app.<base>` — serves the public
 *   pre-auth pages (landing, signup, login). No tenant data.
 * - **Tenant subdomain** — `<slug>.<base>` — serves the authenticated
 *   `/account/*` dashboard against same-origin cookie-authed routes.
 *
 * The base domain is injected by the server into the
 * `<meta name="atomic-cloud-base-domain">` tag at serve time. When that
 * placeholder is left untouched (local `vite dev`, or a fixture), we fall back
 * to treating a host with 3+ labels as a tenant subdomain — enough to drive
 * the dashboard locally against e.g. `alpha.localhost`.
 */

const PLACEHOLDER = '__ATOMIC_CLOUD_BASE_DOMAIN__';

/** Read the server-injected base domain, or `null` when running unconfigured. */
export function configuredBaseDomain(): string | null {
  if (typeof document === 'undefined') return null;
  const meta = document.querySelector<HTMLMetaElement>(
    'meta[name="atomic-cloud-base-domain"]',
  );
  const value = meta?.content?.trim();
  if (!value || value === PLACEHOLDER) return null;
  return value.replace(/^\./, '').toLowerCase();
}

/** The current host, lowercased and stripped of any port. */
export function currentHost(): string {
  if (typeof window === 'undefined') return '';
  return window.location.host.split(':')[0].toLowerCase();
}

/**
 * The tenant subdomain (the `<slug>` of `<slug>.<base>`), or `null` when the
 * current host is the app host / not a tenant.
 */
export function tenantSubdomain(): string | null {
  const host = currentHost();
  if (!host) return null;

  const base = configuredBaseDomain();
  if (base) {
    if (host === base || host === `app.${base}`) return null;
    const suffix = `.${base}`;
    if (host.endsWith(suffix)) {
      const label = host.slice(0, -suffix.length);
      // Only a single leading label is a tenant; deeper names aren't ours.
      if (label && !label.includes('.') && label !== 'app') return label;
    }
    return null;
  }

  // Unconfigured fallback: `app.*` and bare/2-label hosts are the app host;
  // a single-label subdomain (`alpha.localhost`, `alpha.example.com`) is a
  // tenant. `localhost` / IPs (no dot, or all-numeric) are the app host.
  const labels = host.split('.');
  if (labels[0] === 'app') return null;
  if (labels.length >= 3) return labels[0];
  return null;
}

/** Whether the SPA is running on the public app host. */
export function isAppHost(): boolean {
  return tenantSubdomain() === null;
}

/**
 * Absolute URL of the app-host login page, used to bounce an unauthenticated
 * dashboard request out of a tenant subdomain. Prefers the configured base
 * domain; otherwise rewrites the current host's first label to `app`.
 *
 * `query` appends a search string (e.g. `'deleted=1'` after an account
 * deletion, so the login page can confirm the account is gone). Pass it without
 * the leading `?`.
 */
export function appHostLoginUrl(query?: string): string {
  const suffix = query ? `?${query}` : '';
  if (typeof window === 'undefined') return `/login${suffix}`;
  const { protocol, port } = window.location;
  const portSuffix = port ? `:${port}` : '';
  const base = configuredBaseDomain();
  const appHost = base ? `app.${base}` : appHostFromCurrent();
  return `${protocol}//${appHost}${portSuffix}/login${suffix}`;
}

/**
 * The tenant root URL — `<scheme>://<this tenant host>/` — where the product
 * knowledge-base app is served (an existing nginx concern at the tenant root,
 * separate from this account dashboard at `/account/*`). The dashboard's "Open
 * knowledge base" link points here.
 */
export function tenantRootUrl(): string {
  if (typeof window === 'undefined') return '/';
  const { protocol, host } = window.location;
  return `${protocol}//${host}/`;
}

function appHostFromCurrent(): string {
  const host = currentHost();
  const labels = host.split('.');
  if (labels.length >= 3) {
    labels[0] = 'app';
    return labels.join('.');
  }
  return host;
}
