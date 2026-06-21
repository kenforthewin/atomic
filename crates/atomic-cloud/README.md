# atomic-cloud

Multi-tenant cloud hosting for Atomic. This crate turns the single-tenant
[`atomic-server`](../atomic-server) into a cloud deployment **by composition,
not modification** — it wraps atomic-server's routes under its own middleware
and adds the account, auth, provisioning, and background-execution machinery a
hosted service needs.

The design lives in [`docs/plans/atomic-cloud.md`](../../docs/plans/atomic-cloud.md);
that document is the source of truth for *why*. This README is the source of
truth for *what's here and how to run it*.

---

## The one rule that shapes everything

**The dependency arrow is strictly one-way:**

```
atomic-cloud  →  atomic-server  →  atomic-core
```

Neither lower crate contains any cloud-aware code. Grep `atomic-core` and
`atomic-server` for `cloud`, `tenant`, or `account` and you should find
nothing. When cloud needs a capability from a lower crate, that capability is
added as a **cloud-unaware generality** (e.g. `AtomicCore::open_postgres_with_pool`,
`DatabaseManager::new_postgres_with_pool_and_provider`,
`PostgresStorage::target_schema_version()`, the `inline_pipeline` knob) — useful
on its own merits, named without cloud vocabulary, with self-hosted behavior
unchanged by default.

If you find yourself wanting to teach atomic-core or atomic-server about tenants,
stop: the seam belongs in this crate.

## Two tiers of "database" — don't conflate them

| Tier | What | Boundary for |
|---|---|---|
| **Tenant database** (`acct_<base32(uuid)>`) | One Postgres database per account, on the shared cluster. Runs atomic-core's existing tenant migrations. | Isolation, backup, billing, (eventual) sharding |
| **Knowledge base** (`db_id` column *inside* a tenant DB) | The existing per-KB unit. One account can have several. | User-level organization |

Plus the **control-plane database** (`atomic_cloud_control`), separate from any
tenant: accounts, tenant-DB mappings, tokens, sessions, subdomain reservations,
provider credentials, dispatch hints, and deploy-run history.

## Request lifecycle

Routing is split by `Host`:

- **App host** — the bare base domain and `app.<base-domain>` — serves the
  unauthenticated **account plane** (signup/login). No tenant state.
- **Tenant subdomains** (`<slug>.<base-domain>`) serve the **data plane**:
  atomic-server's full `api_scope()`, wrapped in `CloudAuth`.

`CloudAuth` ([`auth.rs`](src/auth.rs)) is the entire authorization layer. Per request:

1. `Host` → strip base domain → subdomain.
2. `accounts WHERE subdomain` → **404** if absent; non-`active` status → **503**
   (`account_provisioning`); schema version behind the compiled target → **503**
   (`account_upgrading`).
3. Bearer token **or** session cookie → verified against
   `cloud_tokens`/`sessions` `WHERE account_id = ?` (the cross-tenant chokepoint).
4. Credits-paused tenants get a structured `out_of_ai_credits` on interactive AI
   routes (atom CRUD still works).
5. [`AccountCache`](src/account_cache.rs) resolves the tenant's
   `DatabaseManager` (rebuilding/refreshing if `provider_generation` advanced).
6. `RequestDatabaseManager`, `RequestEventChannel`, and `ResolvedTenant` are
   injected into request extensions; atomic-server's handlers run against the
   injected manager, never knowing they're multi-tenant.

A `cloud_plane_guard` ([`server.rs`](src/server.rs)) **fail-closes** routes that
bind atomic-server's process-global state and have no per-tenant story yet —
`/api/auth/*`, the export-job routes (`/api/exports/*`,
`/api/databases/{id}/exports/*`), and `/api/logs` all return 404 under cloud.

### OAuth + MCP on the tenant subdomain

Each tenant subdomain also serves cloud's **own** OAuth 2.0 flow and the MCP
endpoint, so Claude Desktop's `https://<slug>.<base>/mcp` connect-and-authorize
journey works per account:

- **OAuth** ([`oauth_routes.rs`](src/oauth_routes.rs)) — discovery
  (`/.well-known/oauth-authorization-server`,
  `/.well-known/oauth-protected-resource[/mcp]`), Dynamic Client Registration
  (`POST /oauth/register`), Authorization Code + PKCE (`GET`/`POST
  /oauth/authorize`, `POST /oauth/token`). These sit **alongside** CloudAuth,
  not behind it (a bootstrapping client has no token yet); each handler
  resolves the account from `Host` itself and scopes every
  [`oauth_store`](src/oauth_store.rs) query by `account_id` — the same
  cross-tenant chokepoint. The approve step authenticates the **session
  cookie** (the user is logged in on their subdomain), not a pasted token, so
  the flow is structurally atomic-server's shape with a control-plane store and
  a session-based approving identity. atomic-server's self-hosted OAuth
  handlers are untouched.
- **`/mcp`** sits **behind** CloudAuth (it carries the bearer MCP token the
  OAuth flow mints) and, like the REST `api_scope`, is wrapped with the
  **data-plane guards** — its `create_atom` tool is charged against the plan's
  atom quota, write-blocked under a `read_only` / storage-`restricted` billing
  hold, and rate-limited (the `quota_guard` peeks the JSON-RPC body to charge
  only atom-creating calls, so the handshake and read tools stay free). CloudAuth
  injects the tenant's `DatabaseManager` as a
  `RequestDatabaseManager` extension; atomic-server's MCP transport resolves
  its manager from that extension per-request (falling back to its baked-in
  manager when none is installed — exactly how self-hosted runs). An
  unauthenticated `/mcp` request gets a 401 with the MCP-compliant
  `WWW-Authenticate` challenge pointing at *this tenant's* protected-resource
  metadata, so the client discovers the right per-account OAuth flow.

**MCP-token default scope**: OAuth-minted tokens are classified `scope='mcp'`
in `cloud_tokens` and default to **account-level access** (`allowed_db_id =
NULL`) — one MCP URL per account, full access to all its KBs, matching "one
account = one user". A db-pinned authorization still mints a KB-pinned `mcp`
token, and CloudAuth's `allowed_db_id` chokepoint enforces the pin (a pinned
MCP token can't reach another KB via the `X-Atomic-Database` header). Per-KB
MCP scoping by default is not yet supported.

## Module map

**Composition & entry**
- [`lib.rs`](src/lib.rs) — crate doc + public re-exports
- [`main.rs`](src/main.rs) — the `atomic-cloud` binary: `serve`, `migrate`, `account`, `token`, `deploy`
- [`server.rs`](src/server.rs) — `configure_cloud_app`, the Host-split, `cloud_plane_guard`, the inert `FallbackAppState`

**Auth & routing**
- [`auth.rs`](src/auth.rs) — `CloudAuth` middleware, `AuthPrincipal`, `ResolvedTenant`
- [`account_cache.rs`](src/account_cache.rs) — per-account `DatabaseManager` cache (idle TTL, hard cap, WS-receiver eviction pinning, generation-checked refresh)
- [`tenant_plane.rs`](src/tenant_plane.rs) — cloud-owned tenant routes (`DELETE /api/account`, the provider routes)
- [`account_plane.rs`](src/account_plane.rs) — signup/login request-link + complete, and `POST /account/logout` (session revocation + cookie clear)
- [`oauth_routes.rs`](src/oauth_routes.rs) — cloud's per-account OAuth flow (DCR + Auth Code + PKCE), public discovery/register/token + session-authenticated approve; the `/mcp` mount is wired in `server.rs`
- [`oauth_store.rs`](src/oauth_store.rs) — control-plane `oauth_clients`/`oauth_codes` storage (per-account, hash-only secrets, single-use codes)
- [`spa.rs`](src/spa.rs) — serves the account-plane SPA ([`frontend/`](frontend)): the base-domain-injected `index.html`, the traversal-guarded asset/shell fallback (`default_service`), and the tenant dashboard **session gate** (an unauthenticated tenant `GET /account/*` → `302` to the app-host login; a valid session → the shell)

**Control plane & provisioning**
- [`control_plane.rs`](src/control_plane.rs) — `ControlPlane` handle, connect-or-create, the hardened migration runner
- [`provision.rs`](src/provision.rs) — `provision_account` / `delete_account` (idempotent, race-guarded; the active-deletion path best-effort cancels the tenant's Stripe subscription and purges its magic links before the drop)
- [`tokens.rs`](src/tokens.rs) — `atm_`/`ats_` token & session issuance (hash-only storage)
- [`reserved_subdomains.rs`](src/reserved_subdomains.rs) — the vanity-slug blocklist

**Signup & email**
- [`magic_links.rs`](src/magic_links.rs) — `aml_` links, single-use atomic consume
- [`email.rs`](src/email.rs) — `EmailSender` trait + `LogSender` (dev) / `MailgunSender`
- [`rate_limit.rs`](src/rate_limit.rs) — per-IP / per-email sliding-window limiters (signup surface) + the per-account data-plane limiters (API requests / atom creates / URL ingestion) and their guard

**Plans, quotas & billing**
- [`plans.rs`](src/plans.rs) — the seeded plan catalogue + in-memory `PlanRegistry`
- [`quota.rs`](src/quota.rs) — the data-plane resource-limit guard (atom/KB creates → 402 `quota_exceeded`)
- [`quota_usage.rs`](src/quota_usage.rs) — the two control-plane jobs that write `quota_usage`: the monthly `roll_over_period` (idempotent, cross-pod safe) and the storage-bytes `recompute_storage` arm (`pg_database_size` → `StorageState` warn → restrict; data always retained)
- [`billing.rs`](src/billing.rs) — `BillingProvider` trait + `StripeClient`, webhook signature verification + event projection
- [`billing/dunning.rs`](src/billing/dunning.rs) — `BillingState` (incl. `trialing`), subscription/payment transitions, the time-driven `advance_dunning` (+ `advance_dunning_with` for configurable `DunningThresholds`) + `advance_expired_trials` sweeps, and `start_trial` (signup grants the 14-day paid trial)
- [`billing_routes.rs`](src/billing_routes.rs) — portal/checkout redirects (tenant) + the signed webhook (app host)
- [`billing_guard.rs`](src/billing_guard.rs) — the write-guard that 402s mutations under EITHER the dunning `read_only` state (`account_read_only`) or the storage `restricted` state (`account_storage_restricted`); suspended is gated in `CloudAuth`

**Providers** (managed keys + BYOK)
- [`keyvault.rs`](src/keyvault.rs) — `KeyVault` trait, AES-256-GCM `EnvMasterKeyVault`, `SecretKey`
- [`provider_credentials.rs`](src/provider_credentials.rs) — encrypted credential store + active-provider pointer
- [`provider_config.rs`](src/provider_config.rs) — control-plane row → `atomic_core::ProviderConfig`
- [`managed_keys.rs`](src/managed_keys.rs) — managed OpenRouter key lifecycle
- [`provisioning_api.rs`](src/provisioning_api.rs) — `ProvisioningApi` trait + OpenRouter client
- [`curated_models.rs`](src/curated_models.rs) — pinned embedding model + curated LLM list

**Backups & disaster recovery**
- [`backup_store.rs`](src/backup_store.rs) — `BackupStore` trait + `LocalFileSystemStore` (dev/tests, pure `tokio::fs`) / `S3Store` (production, via `object_store`)
- [`backup.rs`](src/backup.rs) — the `pg_dump -Fc` / `pg_restore` runner (password via `PGPASSWORD` in the child env, never argv; bounded stderr capture)
- [`backups.rs`](src/backups.rs) — the nightly pass (per-tenant advisory-locked + control plane), the fail-closed final dump before deletion, the `backup_runs` ledger queries, and the staleness monitor

**Background execution**
- [`dispatcher.rs`](src/dispatcher.rs) — the per-pod dispatcher loop (hint scan → N+1 poll → round-robin drain)
- [`pools.rs`](src/pools.rs) — four bounded worker pools with per-tenant caps
- [`dispatch_hints.rs`](src/dispatch_hints.rs) — the `dispatch_hints` pending-work bit
- [`backpressure.rs`](src/backpressure.rs) — provider 429/402/401 + transient (5xx/timeout) failure classification + per-tenant circuit breaker (transient faults defer-and-retry rather than terminal-fail)
- [`chat_streams.rs`](src/chat_streams.rs) — per-tenant streaming-chat semaphore (not pooled)

**Lifecycle & ops**
- [`reaper.rs`](src/reaper.rs) — periodic recovery: stuck provisions, orphan DBs, self-reservations, expiry, lagging migrations. Orphan reclaim is data-loss-guarded: it refuses outright when the control plane reports zero accounts but the cluster holds tenant DBs (the misdirected-`--control-url` signature) and caps drops per pass
- [`fleet_migration.rs`](src/fleet_migration.rs) — boot-time fleet migration over lagging tenants
- [`deploy.rs`](src/deploy.rs) — readiness state machine + failure-rate policy + `deploy_runs` history

- [`error.rs`](src/error.rs) — `CloudError`

## Frontend (account-plane SPA)

The cloud "front door" — signup, login, and the authenticated per-tenant
account dashboard — is a Vite + React 18 + TypeScript + Tailwind v4 SPA in
[`frontend/`](frontend). Its design language (warm light-paper palette, Crimson
Pro serif display, DM Sans body, one purple accent, the node-graph motif) is
**sourced from the marketing site** (`atomic-website`); the dark product app
(`atomic/src`) is a separate surface and untouched here. One build serves two
route contexts, switched by the request `Host`:

| Context | Hosts | What it serves |
|---|---|---|
| **App host** | the bare base domain + `app.<base>` | public pre-auth pages: landing, `/signup`, `/login` |
| **Tenant subdomain** | `<slug>.<base>` | the authenticated `/account/*` dashboard (overview, provider/BYOK, billing, MCP, danger) — same-origin, session-cookie authed |

**Build it** (`dist/` and `node_modules/` are git-ignored, never committed —
produce the bundle as part of the deploy):

```bash
npm --prefix crates/atomic-cloud/frontend ci
npm --prefix crates/atomic-cloud/frontend run build   # tsc + vite → dist/
```

**Serving** is wired in [`spa.rs`](src/spa.rs) and registered last in
`configure_cloud_app`, after every JSON/OAuth/MCP/WS route, so the SPA can never
shadow an API route. The server (`serve`) points at the build with `--spa-dir`
(env `ATOMIC_CLOUD_SPA_DIR`, default `crates/atomic-cloud/frontend/dist`); if
that directory has no `index.html` (a pure-API pod, or a dev run that hasn't
built the frontend) the SPA is simply absent and unmatched paths 404 — **the
APIs still run without a built frontend**. Two pieces:

- **The tenant dashboard gate** — an unauthenticated tenant `GET /account/*`
  navigation is a server-side `302` to the app-host login (never a flash of the
  dashboard shell); a valid session cookie serves the shell. `/api/*` is matched
  earlier (CloudAuth), so an unauthenticated API call still gets the structured
  JSON `401`, never the redirect.
- **The fallback** (`default_service`) — a real file under `dist/` is served as
  that file; anything else returns the base-domain-injected `index.html` so
  client-side routing takes over.

**Dev story.** Run the SPA's Vite dev server (`npm --prefix
crates/atomic-cloud/frontend run dev`) and a local `serve` (above) side by side;
point the dev server's proxy at the cloud server on `:8080` for the same-origin
JSON routes. Without the server-injected base-domain meta tag, the SPA's host
detection ([`frontend/src/lib/host.ts`](frontend/src/lib/host.ts)) falls back to
a label-count heuristic, so the dashboard is drivable against e.g.
`alpha.localhost`. See [`frontend/README.md`](frontend/README.md) for the full
component map, the typed API client, and the dashboard's state handling.

## Running it locally

> Running it on a **headless box over Tailscale/SSH** to click through the UI
> in a browser? See **[TESTING.md](TESTING.md)** — it covers the host routing,
> the SSH tunnel, and the `--dangerously-insecure-cookies` dev flag the
> plain-HTTP session cookie needs.
>
> Standing up a **production** deployment? See **[DEPLOY.md](DEPLOY.md)** — the
> host-split reverse proxy (and the tenant-app cloud-marker it must inject),
> wildcard DNS/TLS, `--trust-proxy-header`, the single-pod LISTEN/NOTIFY
> constraint, the required-env/secret checklist, and the accepted-risk register.

Cloud is Postgres-only. A dev cluster (superuser, can `CREATE/DROP DATABASE`)
is the only prerequisite — the repo's test compose file works:

```bash
docker compose -f docker-compose.test.yml up -d   # Postgres on :5433
```

```bash
# Provider credentials are encrypted at rest, so a master key is mandatory at boot.
export ATOMIC_CLOUD_MASTER_KEY=$(openssl rand -hex 32)   # 32 bytes, hex or base64

CTL=postgres://atomic:atomic_test@localhost:5433/atomic_cloud_control
CLUSTER=postgres://atomic:atomic_test@localhost:5433/atomic_test

# 1. Create + migrate the control plane.
cargo run -p atomic-cloud -- --control-url $CTL migrate

# 2. Boot the server (email-mode log = magic links print to the log;
#    provisioning defaults to disabled = accounts created without an AI key).
cargo run -p atomic-cloud -- --control-url $CTL serve \
  --cluster-url $CLUSTER --base-domain cloudtest.local --port 8080 --email-mode log

# 3. (other shell) Provision an account — prints a one-time account token.
cargo run -p atomic-cloud -- --control-url $CTL account create \
  --cluster-url $CLUSTER --email you@example.com --subdomain alpha
```

Drive tenant requests with an explicit `Host` header (no `/etc/hosts` needed for
curl):

```bash
TOKEN=atm_...        # from `account create`
curl http://127.0.0.1:8080/api/atoms \
  -H "Host: alpha.cloudtest.local" -H "Authorization: Bearer $TOKEN"
```

Public, unauthenticated: `GET /health` (liveness) and `GET /ready` (gated behind
boot fleet migration). For browser testing, add
`127.0.0.1 cloudtest.local app.cloudtest.local alpha.cloudtest.local` to
`/etc/hosts`.

### Lighting up AI

With `--provisioning-mode disabled` (the default), atoms create and the pipeline
runs, but the embedding/LLM steps report a structured "provider not configured"
error. To make AI work:

- **BYOK** — `PUT /api/account/provider` with an OpenRouter key, or an
  `openai_compat` key pointed at any OpenAI-compatible endpoint. Validated
  before storage; takes effect live without a cache evict. The base URL is
  **SSRF-gated**: it must be `https` to a public host — private / loopback /
  link-local / cloud-metadata addresses are rejected so a tenant can't aim our
  outbound client at internal services, and a rejected validation never echoes
  the upstream body back (no read-oracle). To point at a **local** model
  (Ollama / LM Studio on `http://127.0.0.1:…`) set
  `ATOMIC_CLOUD_ALLOW_PRIVATE_PROVIDER_URLS=1` — a dev/test-only escape that
  reopens the gate (`serve` warns loudly if it finds it set on a non-localhost
  deployment).
- **Managed** — `--provisioning-mode openrouter` + a provisioning key in
  `ATOMIC_CLOUD_OPENROUTER_PROVISIONING_KEY` mints a per-account runtime key at
  signup, sized to the account's **plan** allowance (`ai_credits_monthly_cents`;
  `--managed-key-allowance-cents` is the fleet fallback) and re-sized on every
  plan transition.

## CLI

```
atomic-cloud --control-url <URL> <command>

  serve      Run the multi-tenant HTTP server
  migrate    Create (if needed) + migrate the control-plane database
  account    create | delete   (provision/teardown a tenant; delete takes a final dump)
  token      create            (mint an account/database/mcp-scoped token)
  deploy     status | advance  (inspect / acknowledge boot fleet migrations)
  backup     run | status | list | restore
                               (run a pass; report freshness/stale tenants;
                                list a tenant's dumps; restore into a fresh DB)
```

`--control-url` is global; `serve` and `account` also take `--cluster-url`. Run
any subcommand with `--help` for the full flag set. Notable `serve` groups:

- **Routing & control plane**: `--base-domain`, `--port`, `--bind`, `--app-public-url`, `--trust-proxy-header` (per-IP limits behind a proxy), `--control-pool-max-connections` (control-plane pool size, default 25)
- **Email**: `--email-mode log|mailgun` (+ `--mailgun-*`)
- **Providers**: `--provisioning-mode`, `--managed-key-allowance-cents`, `--master-key-env`
- **Billing (Stripe, optional)**: `--stripe-secret-key-env`, `--stripe-webhook-secret-env`, `--stripe-price plan=price_…` (secret *values* are env-only, never argv)
- **Quotas & dunning**: `--period-rollover-interval-secs`, `--storage-recompute-interval-secs`, `--storage-warn-after-days`, `--storage-restrict-after-days`, `--dunning-read-only-days` (3), `--dunning-suspended-days` (14)
- **Dispatcher**: `--dispatcher`, `--dispatcher-tick-ms`, the four `--*-pool-total`/`--*-pool-per-tenant` caps, `--reports-per-tenant-cap`
- **Backpressure**: `--breaker-*`, `--retry-after-cap-secs`, `--chat-streams-per-account`
- **Deploy gating**: `--fleet-migration-*`, `--deploy-ready-failure-rate`, `--deploy-review-failure-rate`
- **Backups**: `--backup-store local|s3` (+ `--backup-base-dir`, `--backup-bucket`, `--backup-region`, `--backup-endpoint`, `--backup-prefix` for a shared-bucket key prefix; S3 access-key/secret are env-only `AWS_*`), `--backup-interval-secs`, `--max-backups-per-pass`, `--backup-staleness-secs`, `--backup-timeout-secs` (per-`pg_dump`/`pg_restore` kill budget; default 30m)

Every flag has an `ATOMIC_CLOUD_*` env fallback. Secrets (master key, provisioning
key) are **only** read from the environment — never argv — to keep them out of
process listings.

## Migrations

Control-plane migrations live in [`migrations/`](migrations) (`001`–`016`) and
run through the hardened runner in `control_plane.rs` (schema-version table,
advisory lock on a detached connection, errors propagated). Tenant databases run
atomic-core's own migrations via `initialize()`.

Migrations are **additive-only** — no `DROP COLUMN`, `ALTER COLUMN ... TYPE`,
`RENAME`, `SET NOT NULL`, or validated-at-add constraints. This is what makes
rolling deploys safe (old code tolerates new columns) and is enforced by
[`tests/migration_lint.rs`](tests/migration_lint.rs), which scans both this
crate's and atomic-core's migration directories. Drops happen N+1 deploys later.

## Backups & the restore runbook

Backups are **nightly logical dumps** (`pg_dump -Fc`, custom format) per tenant
database plus the control plane, written to a [`BackupStore`](src/backup_store.rs)
(local filesystem in dev/tests; S3-compatible in production via `object_store`).
Object keys are deterministic and date-prefixed so **retention is bucket
lifecycle policy, not code** — nothing in this crate ever deletes a backup:

| Key | What | Retention (bucket lifecycle) |
|---|---|---|
| `backups/<YYYY-MM-DD>/acct_<base32>.dump` | one tenant's nightly dump | 14 daily + 8 weekly |
| `backups/<YYYY-MM-DD>/control.dump` | the control-plane nightly dump | 14 daily + 8 weekly |
| `backups/final/<account-id>-<ts>.dump` | the **final dump taken before an account deletion** | 30 days |

The nightly pass ([`backups.rs`](src/backups.rs)) dumps each active tenant under
the reaper's **per-account advisory lock** (two pods never dump the same tenant
at once), stamps `account_databases.last_backup_at`, records a `backup_runs`
ledger row, and runs the **staleness monitor** (an error-level alert when any
active tenant's last successful backup is >36h old). The credential password is
passed to `pg_dump`/`pg_restore` via `PGPASSWORD` **in the child environment,
never argv** (a unit test pins this), and every database name is shape-validated
by `is_tenant_db_name` before any DDL.

Three robustness properties the runner guarantees:

- **Each `pg_dump`/`pg_restore` is killed if it overruns `--backup-timeout-secs`**
  (default 30m). A hung child — a tenant holding a lock that blocks `pg_dump`'s
  `ACCESS SHARE`, a network stall, a wedged process — is killed and recorded as
  a typed timeout failure rather than hanging the serial pass forever. One
  tenant's timeout never aborts the pass; the whole-pass worst case is bounded
  by `backup_timeout × max_backups_per_pass`.
- **Most-overdue-first ordering by last *attempt***, not last *success*: a
  tenant whose dump keeps failing stamps `last_backup_attempt_at` each pass, so
  it sinks behind a healthy-but-due tenant instead of starving it at the front
  of every capped pass. A never-attempted tenant still sorts first.
- **Stale in-flight `backup_runs` rows are finalized `'abandoned'`** (a pod
  killed mid-pass leaves a row never finished) — at pass start and on `backup
  status`, mirroring `deploy_runs`.

The pass and **every active-account deletion path** take the *same* per-account
advisory lock, so a backup mid-`pg_dump` of tenant X and a `DROP DATABASE` of X
are genuinely mutually exclusive — for the reaper's interrupted-deletion arm
(which holds the lock and calls `delete_account` in `DeleteLock::AlreadyHeld`
mode), the HTTP route, and the CLI (both `DeleteLock::Acquire`). A delete that
finds the lock held by a backup waits briefly, then returns a typed `Busy`
(HTTP 503, retry) rather than racing.

**Final dump on deletion is fail-closed and mandatory.** `delete_account` takes
the final `backups/final/` dump **before** the `DROP DATABASE`; if the dump (or
its upload) fails, the deletion **aborts** and drops nothing — under hard-delete
v1 the final dump is the operator's only undo, so destroying un-backed-up data is
never allowed. This applies **only to the active-account deletion path** (the
HTTP route, the CLI, and the reaper's *interrupted-deletion* completion arm). The
reaper's stuck-provision rollback and orphan-database reclaim drop tenants that
**never activated** (no real user data, possibly not even dumpable) and
deliberately take **no** final dump.

The backup decision is an **explicit `BackupPolicy`**, never a fail-open absent
store: `Required(store)` takes the dump; `DisabledAcknowledged` drops without one
after a loud `warn!` (dev, or the never-activated reaper paths). A composition
that *forgets* to wire the store on an enabled deployment is a type error, not a
silent unrecoverable drop — the route holds `Enabled(store)` vs
`DisabledAcknowledged`, with no third "forgot it" state.

### Restore runbook (rehearse before launch)

A restore is **per-tenant** and never reads or writes another tenant's data,
control rows, or store keys — its keys are named by *that* tenant's `db_name` and
`account_id`. The procedure, end to end:

```bash
# 1. Restore the dump into a FRESH tenant database (a new acct_<base32> name).
#    Restore refuses to clobber an existing database — it never overwrites a
#    live tenant. The old database (if any) is left intact until step 4.
atomic-cloud --control-url $CTL backup restore \
  --cluster-url $CLUSTER \
  --backup-store s3 --backup-bucket <bucket> \
  --key backups/final/<account-id>-<ts>.dump \
  --target-db acct_<new-base32>
```

Then, **manually** (the CLI prints these and intentionally does not do them — a
CLI invocation can't reach a running pod's in-memory cache, and silently
repointing while a pod serves the old database would split-brain reads):

2. **Repoint the mapping**, recording the schema version the dump carries (the
   running binary's compiled target) so CloudAuth's straggler gate doesn't 503
   the restored tenant as forever-`account_upgrading` — a real trap the
   end-to-end rehearsal (`tests/e2e_backup.rs`) caught:

   ```sql
   UPDATE account_databases
      SET db_name = 'acct_<new-base32>',
          last_migrated_version = <target>,   -- this binary's tenant_schema_target()
          last_migrated_at = NOW()
    WHERE account_id = '<account-id>';
   ```

3. **Evict the running serve process's `AccountCache` entry** for that account —
   otherwise a serving pod keeps a `DatabaseManager` pointing at the OLD
   `db_name` and serves the stale database. A CLI restore cannot reach another
   process's cache, so until an admin evict endpoint exists, restart the pod or
   let the idle TTL reclaim the entry. In-process (e.g. the HTTP deletion route)
   this is `AccountCache::evict`.

4. **Drop the old database** once you've confirmed the restored tenant serves.

The whole sequence — final dump → restore into a fresh DB → repoint (with the
schema version) → evict → verify — is **rehearsed as a test** so the runbook
stays honest:
[`tests/backup.rs::final_dump_restore_runbook_roundtrip`](tests/backup.rs) at the
library level and [`tests/e2e_backup.rs`](tests/e2e_backup.rs) through the
composed cloud server (two tenants, isolation asserted at every step). **PITR via
WAL archiving is deferred** — recovery granularity is one day.

## Testing

~410 test functions across [`tests/`](tests) and inline `#[cfg(test)]` modules.
Tests are **Postgres-gated**: they skip cleanly when `ATOMIC_TEST_DATABASE_URL`
is unset, and create + drop their own uniquely-named databases (control plane and
tenant) with guard-based cleanup.

```bash
# Unit + integration, no DB (PG-gated tests skip; the migration lint still runs):
cargo test -p atomic-cloud

# Full suite against the dev cluster — MUST be single-threaded
# (all PG tests share one physical cluster):
CARGO_INCREMENTAL=0 \
ATOMIC_TEST_DATABASE_URL=postgres://atomic:atomic_test@localhost:5433/atomic_test \
  cargo test -p atomic-cloud -- --test-threads=1
```

Test doubles keep suites hermetic and offline: `atomic_test_support::MockAiServer`
(wiremock, can inject 429/402/401/latency), a capturing `EmailSender`, and a
`RecordingProvisioning` for the OpenRouter provisioning API. No test hits a real
provider or sends real email.

## Plan tiers, quotas & billing

- **Plan-tier resource limits** — `plans` catalogue + `accounts.plan_id`,
  live atom/KB enforcement (402 `quota_exceeded`); free-tier defaults (100
  atoms, 1 KB, 100 MB, $0.50/mo AI), `pro` placeholder (unlimited atoms/KBs,
  10 GB, frontier-models flag).
- **Anti-abuse rate limits** — per-IP/email signup-surface limiters plus the
  per-account data-plane rows (600 req/min, 60 atom-creates/min, 30 URL
  ingestions/min), 429 with `Retry-After`.
- **Stripe billing** — customer-portal + checkout redirects (account-scope
  gated), the signed webhook on the app host (HMAC-SHA256 verification with a
  replay-tolerance window + event-id idempotency ledger), and the full
  subscription lifecycle projection (`created/updated/deleted`,
  `payment_{succeeded,failed}`). The Stripe HTTP client is behind a
  `BillingProvider` trait; the real client's request shape is wiremock-pinned
  and the webhook scheme is unit-tested over a known-secret fixture. No test
  hits real Stripe.
- **Dunning** — `past_due → read_only (3d) → suspended (14d)`, time-driven by
  `advance_dunning`; thresholds are `--dunning-*` flags. **Data is always
  retained, never auto-deleted.**
- **Trials** — 14-day paid trial at signup (no card), auto-downgraded to free
  by the sweep (read-only if over the free limits, data retained).
- **Period rollover** — a 1-hour `quota_usage` rollover for the non-AI metrics
  (idempotent, cross-pod safe); AI allowances reset natively at OpenRouter.
- **Storage enforcement** — a periodic `pg_database_size` recompute into
  `quota_usage` with week-1-warn / week-2-restrict serving states (the
  `account_storage_restricted` write-block), **never auto-deleting**.
