# Public Demo Instance (`demo.atomicapp.ai`)

## Status

Slice 1 (anonymous access + whitelist + frontend demo mode) implemented
2026-07-13 — `demo_plane.rs`, the `DemoVisitor` principal, `--demo-subdomain`
/ `ATOMIC_CLOUD_DEMO_SUBDOMAIN`, `account create --allow-reserved`, and the
product-app demo chrome (banner, chat CTA, read-only pin, hidden edit
affordances), pinned by `tests/demo_plane_e2e.rs`. One design deviation from
the first draft, for the better: the whitelist is enforced at
principal-synthesis time inside `authenticate` rather than as a wrapped
middleware, so every auth'd surface (present and future) is demo-closed by
construction with zero wrap sites. Corpus seeding (landmark papers) is next;
feeds/excerpt-mode/edge-caching stay tracked under Deferred.

## Context

The demo is the conversion fix for spike-and-fade: a read-only, no-signup
tenant at `demo.atomicapp.ai` seeded with an AI/LLM-research corpus, so a
Show HN visitor can feel the product (canvas, tag tree, wikis, search,
briefing) in ten seconds. Decisions already made in discussion:

- **No chat.** Conversations are tenant-scoped, so anonymous chat is a
  shared graffiti wall under our domain. The chat panel stays visible but
  disabled, as the signup CTA.
- **Whitelist, not blacklist.** The demo surface is an explicit allowlist
  of (method, path) — every route we add in the future is demo-closed
  until deliberately opened. Same fail-closed philosophy as
  `cloud_plane_guard`, and the reason the export family was never a
  cross-tenant incident.
- **The demo is a real tenant** on the real cluster — real pipeline, real
  managed key (with its hard credit cap as the spend backstop), real
  backups. No parallel serving path to maintain.

## Design

### Demo designation: deploy config, not a DB flag

`--demo-subdomain <slug>` / `ATOMIC_CLOUD_DEMO_SUBDOMAIN` (precedent:
`--metrics-bind`). Unset → no demo behavior anywhere, which keeps dev,
tests, and self-hosted-adjacent deployments inert by default.

Config beats an `accounts.is_demo` column because the blast radius of
"this tenant is publicly readable" should live in one reviewed file
(`deploy/.env`), not be one stray UPDATE away — and because no migration
is needed. The demo account itself is operator-owned, so subdomain
renaming (the usual argument against subdomain-keyed config) doesn't
apply.

### Anonymous principal: one new branch in `authenticate`

`auth.rs::authenticate` step 3–5 currently ends in
`else { return Err(unauthorized()) }` for credential-less requests. The
change: if the resolved subdomain matches the configured demo subdomain,
synthesize a principal instead:

```rust
CredentialSource::DemoVisitor   // new variant
AuthPrincipal { account_id, scope: TokenScope::Account,
                allowed_db_id: None, source: DemoVisitor }
```

Everything before and after is untouched: the suspended/billing gate, the
straggler gate, the AccountCache load, and the request extensions all run
identically. A demo visitor is a normal resolved tenant whose credential
source says "anonymous" — and the whitelist guard keys on exactly that.

Credentialed requests on the demo host flow through the existing
verification unchanged. Only the demo account's own session/token can
pass (pinned by the existing second-chokepoint test), so "the operator
logged in" retains full access for seeding and feed management, with no
special path.

### `demo_plane.rs`: the whitelist guard

New module alongside `billing_guard.rs` / `export_plane.rs`, wired in
`configure_cloud_app` directly after CloudAuth and (like the billing
guard) outside the dispatch-hint writer, so denied requests never mark
hints.

Behavior: if `ResolvedTenant.principal.source == DemoVisitor` and
`(method, path)` is not in `DEMO_ALLOWED`, return **403** with a
structured body:

```json
{ "error": "demo_forbidden",
  "message": "This is a read-only public demo.",
  "signup_url": "https://atomicapp.ai/cloud" }
```

Non-demo principals pass through untouched. The table is data plus a
matcher function (the `is_write_block_exempt` idiom — exact paths and
one-segment-wildcard patterns, no regex).

### `DEMO_ALLOWED` (initial table)

GET/HEAD only, except the two search POSTs:

| Family | Paths |
|---|---|
| Atoms | `/api/atoms`, `/api/atoms/{id}`, `/api/atoms/{id}/links`, `/api/atoms/{id}/similar`, `/api/atoms/{id}/embedding-status`, `/api/atoms/sources`, `/api/atoms/by-source-url` |
| Tags | `/api/tags` (tree + reads; no mutations) |
| Canvas/graph | `/api/canvas/global`, `/api/canvas/positions`, `/api/canvas/atoms-with-embeddings`, `/api/graph/edges`, `/api/graph/neighborhood/{id}`, `/api/clustering`, `/api/clustering/connection-counts` |
| Wiki | `/api/wiki`, `/api/wiki/{tagId}` + `/links`, `/related`, `/status`, `/versions`, `/api/wiki/versions/{id}`, `/api/wiki/suggestions` |
| Reports | `/api/reports`, `/api/reports/{id}`, `/api/dashboard/featured-report`, `/api/findings/{atom_id}` |
| Boot/status | `/api/setup/status`, `/api/settings` (GET only — see scrub note), `/api/settings/models`, `/api/embeddings/status/all`, `/api/databases` (list) |
| Search | **POST** `/api/search`, **POST** `/api/search/global` — the only AI-spend surface, rate-limited below |

Explicitly *not* listed (and therefore closed, enumerated here so the e2e
suite pins representatives of each): all mutations, exports (the billing
guard's egress exemption does NOT carry over — anonymous full-corpus zips
are a disk/CPU hole), conversations/chat, feeds config, tokens,
provider/ollama test routes, import/ingest, `/ws`, `/mcp`, OAuth.

Notes:
- `atoms-with-embeddings` returns raw vectors. They're embeddings of
  public content, so nothing is exfiltratable that a visitor couldn't
  compute themselves; it's also the heaviest response and the #1 target
  when edge caching lands.
- `GET /api/settings` needs a field-level scrub check: managed-mode
  tenant settings should contain no key material (slice-3 design), and an
  e2e test asserts no `*api_key*`-shaped values in the demo response, so
  a future settings field is caught by CI rather than by a visitor.
- `/ws` stays closed in slice 1. The frontend must degrade quietly
  (no live events, no reconnect storm). Revisit if live feed-ingestion
  events prove worth showing off.

### Search rate limiting

Per-IP sliding-window on the two search endpoints, DemoVisitor only —
reuse the hand-rolled sliding-log limiter the magic-link paths use
(per-pod, approximate, fine here). Start: 30/min per IP, 429 with
`Retry-After`. The managed key's hard credit cap (set to ~$20/mo on the
demo account) is the spend backstop regardless; the limiter is about not
being a free-compute nuisance target.

Also: exclude DemoVisitor requests from dispatch-hint marking (searches
are POSTs; without this, anonymous traffic keeps the demo tenant
permanently on the fast-path poll — harmless-ish, but hints should mean
"tenant work may exist," not "someone searched").

### Provisioning the demo account

`demo` is in the reserved-subdomains blocklist (correctly, for signup);
the operator path bypasses it explicitly. **Runbook (executed 2026-07-13,
account `640ec443…`, tenant `acct_mqhmiq6kubb5ne7wvnkoqm6yye`):**

```bash
# 1. Local deploy/.env (rsynced by deploy.sh): ATOMIC_CLOUD_DEMO_SUBDOMAIN=demo
bash deploy/scripts/deploy.sh root@<droplet>

# 2. Inside the pod container (it holds the provisioning + master keys):
docker exec atomic-cloud-atomic-cloud-1 atomic-cloud account create \
  --email <operator-email> --subdomain demo --allow-reserved --managed
# prints the owner token ONCE — used for seeding, then revoke it
# (`token revoke`); mint fresh ones via `token create --subdomain demo`.

# 3. Comped pro, sweep-proof (plan_pinned), key allowance resized in-band:
docker exec atomic-cloud-atomic-cloud-1 atomic-cloud account set-plan \
  --subdomain demo --plan pro --managed

# 4. Tenant config via the owner token on the demo host:
#    POST /api/tags/configure-autotag-targets
#      {"keep_defaults":["Topics","People","Organizations","Events"],"add_custom":[]}
#    PUT  /api/settings/onboarding_completed {"value":"true"}

# 5. Seed the corpus (idempotent, skip_if_source_exists on the arXiv abs
#    URL, paced under the 60/min atom_creates window): 100 landmark papers,
#    metadata fetched live from the arXiv API — never from model memory.
```

The account is a normal pro tenant in every other respect: managed key
with the plan's monthly credit cap (the spend backstop), due-driven
backups, fleet migrations, metrics.

### Frontend demo mode

The product SPA needs to render logged-out on the demo host instead of
bouncing to login:

- Detection: on boot-sequence 401/403, probe `GET /api/demo-config`
  (whitelisted; returns `{demo: true, signup_url}` on the demo host, 404
  everywhere else) → enter demo mode. Zero cost for normal tenants.
- Demo mode: hide edit affordances (new atom, editor entry, settings,
  feeds, import), chat panel disabled with the signup CTA, persistent
  slim banner ("Live demo — this is a real Atomic instance. Get your
  own →"), tolerate absent WS.
- Everything is runtime-flag-driven in the shared frontend; no separate
  build.

### E2e coverage (the contract, pinned)

In `tests/e2e_cloud.rs` (or a new `tests/demo_plane.rs`):

1. Anonymous GET on each whitelisted family on the demo host → 200.
2. Anonymous mutation representatives (`POST /api/atoms`, `PUT
   /api/settings/x`, `DELETE /api/atoms/{id}`) → 403 `demo_forbidden`.
3. Closed-family representatives: exports POST, `GET /api/conversations`,
   `GET /api/feeds`, `POST /api/tokens`, `/mcp`, `/ws` → 403/404.
4. **Anonymous request on a non-demo host → 401 unchanged** (the
   anonymous path opens only for the configured subdomain).
5. **No demo subdomain configured → demo host behaves like any tenant**
   (anonymous → 401): the feature is opt-in per deployment.
6. Owner session on the demo host → mutations work (operator seeding).
7. Settings scrub: `GET /api/settings` response contains no key-shaped
   fields.
8. Search rate limit: 31st request in a minute → 429; authenticated
   owner unaffected.
9. Default-deny future-proofing: a route registered after the guard that
   isn't in `DEMO_ALLOWED` → 403 (pin with any existing unlisted GET).

## Work items (slice order)

1. `DemoVisitor` + `authenticate` branch + config plumbing (flag → env →
   `AuthCtx`).
2. `demo_plane.rs`: table, matcher, guard, `demo-config` endpoint, unit
   tests.
3. Wire into `configure_cloud_app`; hint-writer exclusion; e2e suite.
4. Search limiter for DemoVisitor.
5. CLI `account provision --allow-reserved`; provision + comp the real
   account; set key cap.
6. Frontend demo mode.
7. Corpus: landmark-papers seeding (separate plan section once slice 1
   is up; ~150 papers, abstract+link atoms, via the bulk-create API
   against the owner token).

## Deferred (explicitly out of slice 1)

- **Edge caching** — ~~deferred~~ shipped 2026-07-15, one dashboard step
  remaining. Origin stamps `Cache-Control: public, s-maxage=60` on
  DemoVisitor 200 GETs only (`demo_plane::demo_cache_headers`,
  e2e-pinned: owner and denial responses never carry it; no Vary on
  Cookie — the whitelisted GET surface is visitor-identical by
  construction). DNS: proxied A record `demo` → droplet (record
  c3317e45…, shadows the unproxied wildcard; zone SSL `full`). REMAINING
  (operator, dashboard — the deploy token is DNS-scoped): one Cache Rule,
  hostname `demo.atomicapp.ai` → eligible for cache, respect origin
  headers, bypass on `Authorization` header. Until it exists Cloudflare
  reports DYNAMIC on JSON and the origin serves every request (status
  quo, just proxied).
- **Feeds**: subscribe-time backfill guard (mark-existing-seen), excerpt
  mode, retention task, permission emails — the whole liveness layer.
- **WS** for demo visitors.
- Turnstile/captcha escalation for search (only if the limiter proves
  insufficient in the metrics).

## Open questions

- `GET /api/feeds` as a transparency touch ("here's what this demo
  ingests") — nice, but default-deny says no until argued for.
- Whether demo-mode UI wants a distinct read-only atom view (it will get
  the standard reader; fine for v1).
