# atomic-cloud frontend

The Atomic Cloud **account-plane SPA** — the cloud "front door". A single
Vite + React 18 + TypeScript + Tailwind v4 build serves two route contexts,
switched by the request `Host`:

| Context | Hosts | What it serves |
|---|---|---|
| **App host** | the bare base domain + `app.<base>` | public pre-auth pages: landing `/`, `/signup`, `/login` |
| **Tenant subdomain** | `<slug>.<base>` | the authenticated `/account/*` dashboard: overview, AI provider, billing, MCP, account |

It is visually consistent with the marketing site (`atomic-website`): the
warm light-paper palette, Crimson Pro serif display, DM Sans body, one purple
accent, and the node-graph motif. The dark **product** app (`atomic/src`) is a
separate surface and is untouched here.

## Stack

- **Vite 6** + **React 18** + **TypeScript** (strict).
- **Tailwind v4**, CSS-first: `@import "tailwindcss"` + an `@theme` block in
  `src/styles/global.css` with the exact website tokens. No `tailwind.config`.
- **react-router-dom v7** for routing.
- **lucide-react** for outline icons.
- Self-hosted fonts via **@fontsource** (`crimson-pro`, `dm-sans`, `dm-mono`) —
  no runtime CDN dependency.
- **Vitest** + **@testing-library/react** for unit tests.

## Develop, build, test

```bash
npm install          # uses the committed package-lock.json
npm run dev          # Vite dev server
npm run build        # tsc typecheck + vite build  →  dist/   (must be clean)
npm run lint         # eslint                                  (must be clean)
npm test             # vitest run
```

`dist/` and `node_modules/` are git-ignored; only source is committed. Produce
the deployable bundle with `npm run build`.

## How the dist is served

The cloud server (`atomic-cloud`, actix) serves the built `dist/` directory
after every JSON/OAuth/MCP/WS route — so the SPA can never shadow an API route.
The serving layer lives in [`src/spa.rs`](../src/spa.rs), in two pieces:

- **The tenant dashboard gate.** A tenant-host `GET /account/*` navigation is
  session-gated **server-side**: a request carrying a valid session cookie is
  served the SPA shell; anything else is a `302` to the app-host login
  (`https://app.<base>/login`). So an unauthenticated browser never flashes the
  dashboard chrome — it lands on login immediately. The data plane is untouched:
  `/api/*` is matched earlier (by `CloudAuth`), so an unauthenticated background
  fetch still gets the structured JSON `401`, never the redirect.
- **The fallback** (registered last, the app's `default_service`). A real file
  under `dist/` (a hashed asset, the favicon, a logo) is served as that file
  with an appropriate cache header; anything else (an app-host client-routed
  page like `/login`) returns `index.html` (the SPA shell) so client-side
  routing takes over. The base-domain meta placeholder is rewritten **once at
  startup** with the deployment's real base domain.

Point the server at the build with `--spa-dir` (env `ATOMIC_CLOUD_SPA_DIR`); it
defaults to `crates/atomic-cloud/frontend/dist`. If that directory has no
`index.html` (a pure-API pod, or a dev run that hasn't built the frontend) the
fallback is simply absent and unmatched paths 404 — so the API runs without the
SPA. Produce the bundle the server serves with:

```bash
npm install && npm run build      # → crates/atomic-cloud/frontend/dist
```

`dist/` is git-ignored and **not** committed; build it as part of the deploy.
The Rust serving tests don't depend on a full build — they generate a tiny
fixture `dist/` in a tempdir (see `tests/spa_serving.rs` and the `e2e_cloud.rs`
harness).

## Host / context detection

The SPA must tell the app host from a tenant subdomain at runtime. The base
domain is **injected by the server** into a meta tag in `index.html` at serve
time:

```html
<meta name="atomic-cloud-base-domain" content="__ATOMIC_CLOUD_BASE_DOMAIN__" />
```

`src/lib/host.ts` reads it (`configuredBaseDomain()`):

- A request to the base domain or `app.<base>` → **app host** (public pages).
- A request to `<slug>.<base>` (single leading label, not `app`) → **tenant**.

When the placeholder is left untouched (local `vite dev`, or a test fixture),
detection falls back to a heuristic: `app.*` and 2-label/localhost hosts are the
app host; a 3-plus-label host's first label is the tenant slug. This keeps the
dashboard drivable locally against e.g. `alpha.localhost` without a server.

## API client

`src/lib/api.ts` is a small typed `fetch` client — **same-origin**,
`credentials: 'include'` (so the `.<base>` session cookie rides along on the
tenant dashboard), JSON in/out, the cloud error shapes parsed into a typed
`ApiError` (validation code, `Retry-After`, the parsed body for structured
fields, structured billing/auth states), and a `401` →
redirect-to-app-host-login for authenticated routes. It covers the public
signup/login routes and the dashboard methods: `getOverview`,
`getProviderStatus`, `saveByokProvider`, `activateProvider`, `updateModels`, and
`deleteAccount`.

## The dashboard

The authenticated `/account/*` surface lives under `src/pages/account/`. The
shell (`AccountShell`) loads the account overview once on mount and routes the
structured cloud states into branded frames:

- **provisioning / upgrading** (503) → a friendly auto-retrying hold;
- **suspended** (402) → a blocking notice with the billing upgrade link;
- **trialing / past_due / read_only** → a global `BillingBanner`;
- **ready** → the chrome (top bar, nav) wrapping the active section.

Authentication is enforced at two depths. A browser that isn't logged in never
even loads the shell on `/account/*` — the server-side gate (above) `302`s it to
login first. And once the shell *is* loaded, a `401` from a later fetch (a
session that expired mid-session) never reaches the shell either — the API
client redirects to the app-host login. The loaded overview is shared with child
routes via an outlet context (`src/lib/accountContext.ts`), so the overview page
renders without its own fetch; the provider page fetches the fuller provider
status.

## Layout

```
src/
  components/         SiteNav, SiteFooter, NodeGraphBackdrop, CheckEmail
    ui/               Button, Card, Field, PasswordField, Select, Banner,
                      Spinner, Logo, TextLink, StatusPill, SegmentedControl
    account/          AccountTopbar, AccountNav, BillingBanner, HoldScreen,
                      UsageMeter, ByokForm, ManagedModels
  layouts/            PublicLayout (nav/footer), AuthLayout (centered card + hero)
  lib/                api.ts, host.ts, validate.ts, cn.ts, format.ts, models.ts,
                      provider.ts, accountContext.ts, useOverview.ts,
                      useProviderStatus.ts
  pages/              Landing, Signup, Login, NotFound
    account/          AccountShell, Overview, Provider, Billing, Mcp, Danger
  styles/global.css   @theme tokens + node-graph motif + font setup
  App.tsx             host-split router (nested /account routes on the tenant host)
  main.tsx            entry
public/               logo.svg, logo-dark.svg, logo-mark.svg, favicon.svg
```
