# atomic-cloud frontend

The Atomic Cloud **account-plane SPA** — the cloud "front door". A single
Vite + React 18 + TypeScript + Tailwind v4 build serves two route contexts,
switched by the request `Host`:

| Context | Hosts | What it serves |
|---|---|---|
| **App host** | the bare base domain + `app.<base>` | public pre-auth pages: landing `/`, `/signup`, `/login` |
| **Tenant subdomain** | `<slug>.<base>` | the authenticated `/account/*` dashboard (built in the next phase) |

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

The cloud server (`atomic-cloud`, actix) serves the built `dist/` directory via
`actix-files` with an SPA fallback (any non-API, non-asset path returns
`index.html` so client-side routing works). That serving wiring lands in a
later phase of this slice; this package only produces the bundle. The build
output is intentionally flat and predictable so the Rust side can point a single
`Files` service at `crates/atomic-cloud/frontend/dist`.

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
`ApiError` (validation code, `Retry-After`, structured billing/auth states), and
a `401` → redirect-to-app-host-login for authenticated routes. This phase
implements `requestSignupLink` and `requestLoginLink`; the cookie-authed
dashboard methods are added next phase.

## Layout

```
src/
  components/         SiteNav, SiteFooter, NodeGraphBackdrop, CheckEmail
    ui/               Button, Card, Field, Banner, Spinner, Logo, TextLink
  layouts/            PublicLayout (nav/footer), AuthLayout (centered card + hero)
  lib/                api.ts, host.ts, validate.ts, cn.ts
  pages/              Landing, Signup, Login, AccountShell (placeholder), NotFound
  styles/global.css   @theme tokens + node-graph motif + font setup
  App.tsx             host-split router
  main.tsx            entry
public/               logo.svg, logo-dark.svg, logo-mark.svg, favicon.svg
```
