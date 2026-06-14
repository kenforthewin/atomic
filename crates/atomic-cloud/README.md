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
`/api/auth/*`, `/api/exports/*`, `/api/logs` all return 404 under cloud.

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
  OAuth flow mints). CloudAuth injects the tenant's `DatabaseManager` as a
  `RequestDatabaseManager` extension; atomic-server's MCP transport resolves
  its manager from that extension per-request (falling back to its baked-in
  manager when none is installed — exactly how self-hosted runs). An
  unauthenticated `/mcp` request gets a 401 with the MCP-compliant
  `WWW-Authenticate` challenge pointing at *this tenant's* protected-resource
  metadata, so the client discovers the right per-account OAuth flow.

**MCP-token default scope** (the plan's open question, resolved): OAuth-minted
tokens are classified `scope='mcp'` in `cloud_tokens` and default to
**account-level access** (`allowed_db_id = NULL`) — one MCP URL per account,
full access to all its KBs, matching "one account = one user" in v1. A db-pinned
authorization still mints a KB-pinned `mcp` token, and CloudAuth's
`allowed_db_id` chokepoint enforces the pin (a pinned MCP token can't reach
another KB via the `X-Atomic-Database` header). Per-KB-MCP-by-default is
deferred.

## Module map

**Composition & entry**
- [`lib.rs`](src/lib.rs) — crate doc + public re-exports
- [`main.rs`](src/main.rs) — the `atomic-cloud` binary: `serve`, `migrate`, `account`, `token`, `deploy`
- [`server.rs`](src/server.rs) — `configure_cloud_app`, the Host-split, `cloud_plane_guard`, the inert `FallbackAppState`

**Auth & routing**
- [`auth.rs`](src/auth.rs) — `CloudAuth` middleware, `AuthPrincipal`, `ResolvedTenant`
- [`account_cache.rs`](src/account_cache.rs) — per-account `DatabaseManager` cache (idle TTL, hard cap, WS-receiver eviction pinning, generation-checked refresh)
- [`tenant_plane.rs`](src/tenant_plane.rs) — cloud-owned tenant routes (`DELETE /api/account`, the provider routes)
- [`account_plane.rs`](src/account_plane.rs) — signup/login request-link + complete
- [`oauth_routes.rs`](src/oauth_routes.rs) — cloud's per-account OAuth flow (DCR + Auth Code + PKCE), public discovery/register/token + session-authenticated approve; the `/mcp` mount is wired in `server.rs`
- [`oauth_store.rs`](src/oauth_store.rs) — control-plane `oauth_clients`/`oauth_codes` storage (per-account, hash-only secrets, single-use codes)

**Control plane & provisioning**
- [`control_plane.rs`](src/control_plane.rs) — `ControlPlane` handle, connect-or-create, the hardened migration runner
- [`provision.rs`](src/provision.rs) — `provision_account` / `delete_account` (idempotent, race-guarded)
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
- [`backpressure.rs`](src/backpressure.rs) — provider 429/402/401 classification + per-tenant circuit breaker
- [`chat_streams.rs`](src/chat_streams.rs) — per-tenant streaming-chat semaphore (not pooled)

**Lifecycle & ops**
- [`reaper.rs`](src/reaper.rs) — periodic recovery: stuck provisions, orphan DBs, self-reservations, expiry, lagging migrations
- [`fleet_migration.rs`](src/fleet_migration.rs) — boot-time fleet migration over lagging tenants
- [`deploy.rs`](src/deploy.rs) — readiness state machine + failure-rate policy + `deploy_runs` history

- [`error.rs`](src/error.rs) — `CloudError`

## Running it locally

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
  `openai_compat` key pointed at any OpenAI-compatible endpoint (handy for local
  models). Validated before storage; takes effect live without a cache evict.
- **Managed** — `--provisioning-mode openrouter` + a provisioning key in
  `ATOMIC_CLOUD_OPENROUTER_PROVISIONING_KEY` mints a per-account runtime key at
  signup.

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

- **Routing**: `--base-domain`, `--port`, `--bind`, `--app-public-url`
- **Email**: `--email-mode log|mailgun` (+ `--mailgun-*`)
- **Providers**: `--provisioning-mode`, `--managed-key-allowance-cents`, `--master-key-env`
- **Billing (Stripe, optional)**: `--stripe-secret-key-env`, `--stripe-webhook-secret-env`, `--stripe-price plan=price_…` (secret *values* are env-only, never argv)
- **Quotas & dunning**: `--period-rollover-interval-secs`, `--storage-recompute-interval-secs`, `--storage-warn-after-days`, `--storage-restrict-after-days`, `--dunning-read-only-days` (3), `--dunning-suspended-days` (14)
- **Dispatcher**: `--dispatcher`, `--dispatcher-tick-ms`, the four `--*-pool-total`/`--*-pool-per-tenant` caps, `--reports-per-tenant-cap`
- **Backpressure**: `--breaker-*`, `--retry-after-cap-secs`, `--chat-streams-per-account`
- **Deploy gating**: `--fleet-migration-*`, `--deploy-ready-failure-rate`, `--deploy-review-failure-rate`
- **Backups**: `--backup-store local|s3` (+ `--backup-base-dir`, `--backup-bucket`, `--backup-region`, `--backup-endpoint`; S3 access-key/secret are env-only `AWS_*`), `--backup-interval-secs`, `--max-backups-per-pass`, `--backup-staleness-secs`

Every flag has an `ATOMIC_CLOUD_*` env fallback. Secrets (master key, provisioning
key) are **only** read from the environment — never argv — to keep them out of
process listings.

## Migrations

Control-plane migrations live in [`migrations/`](migrations) (`001`–`015`) and
run through the hardened runner in `control_plane.rs` (schema-version table,
advisory lock on a detached connection, errors propagated). Tenant databases run
atomic-core's own migrations via `initialize()`.

Migrations are **additive-only** — no `DROP COLUMN`, `ALTER COLUMN ... TYPE`,
`RENAME`, `SET NOT NULL`, or validated-at-add constraints. This is what makes
rolling deploys safe (old code tolerates new columns) and is enforced by
[`tests/migration_lint.rs`](tests/migration_lint.rs), which scans both this
crate's and atomic-core's migration directories. Drops happen N+1 deploys later.

## Testing

~198 test functions across [`tests/`](tests) and inline `#[cfg(test)]` modules.
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

## Known v1 limitations

- **Multi-pod WebSocket events**: worker events publish to the executing pod's
  in-memory channel, so in a multi-pod deployment a WS client on another pod
  misses that execution's progress events. Durable state is always correct;
  build the cross-pod relay (Postgres `LISTEN/NOTIFY`) before running >1 pod.
- Several capabilities are scoped to later slices — observability
  metrics/tracing, the user-facing `account_events` log, and the
  signup/billing SPA frontend. The OAuth flow is shipped as API + a minimal
  server-rendered consent/approve form (no SPA); a richer consent UI is later.
  See the plan doc's Implementation log for the current frontier.
- **Backup PITR is deferred**: backups are nightly logical dumps (`pg_dump
  -Fc`) per tenant + control plane, not point-in-time recovery via WAL
  archiving — recovery granularity is one day. The restore CLI restores into
  a *fresh* database; **repointing `account_databases.db_name` and evicting a
  running pod's `AccountCache` entry are deliberate manual runbook steps** (a
  CLI invocation can't reach another process's in-memory cache; an admin evict
  endpoint is a later slice).

## What's shipped (this slice: backups & disaster recovery)

- **`BackupStore` seam** — a trait (`put`/`get`/`list`/`exists`) with a
  `LocalFileSystemStore` (dev + every test; pure `tokio::fs`, never network)
  and an `S3Store` backed by the `object_store` crate (S3 + any S3-compatible
  endpoint; SigV4 not hand-rolled). `serve`/CLI select it via
  `--backup-store local|s3`; S3 credentials are env-only.
- **Logical dump/restore runner** — `pg_dump -Fc` / `pg_restore` via
  `tokio::process`, with the connection **password in `PGPASSWORD` in the
  child env, never argv** (a unit test asserts a sentinel password is only in
  the env), `is_tenant_db_name` shape-validation before any DDL, and bounded
  stderr capture. A real dump → restore → verify roundtrip is integration-
  tested (provision a tenant, write an atom, dump, restore into a fresh DB,
  assert the atom rehydrated) — gated on `pg_dump` being on PATH.
- **Nightly pass** (`backups.rs`) — dumps every active tenant (each under the
  reaper's per-account advisory lock, so two pods never dump one tenant at
  once) plus the control plane, records per-tenant `last_backup_at` /
  `last_backup_error` and a `backup_runs` ledger row (migration `015`), and
  runs the **staleness monitor** (error-level alert when a tenant's last
  successful backup is >36h old). Wired into `serve` with a jittered start +
  CLI knobs, mirroring the reaper loop.
- **Fail-closed final dump on deletion** — `delete_account` takes a final
  `backups/final/` dump **before** the `DROP DATABASE`, scoped to the
  active-account deletion path (HTTP route, CLI, and the reaper's
  interrupted-deletion arm; the never-activated rollback/orphan paths
  correctly take none). A dump failure aborts the deletion rather than destroy
  un-backed-up data — the operator's only undo under hard-delete v1. Retention
  (14 daily + 8 weekly; 30-day finals) is bucket lifecycle policy, not code.
- **Operator + restore CLI + runbook** — `atomic-cloud backup run` (one pass
  now), `backup status` (per-tenant `last_backup_at` + last error + the stale
  set + recent `backup_runs`), `backup list --subdomain` (one tenant's dumps,
  per-tenant by key construction), and `backup restore` — restore a dump into
  a fresh database, then print the remaining manual runbook steps (repoint
  `account_databases.db_name`, evict the running pod's `AccountCache`). The
  full final-dump → restore → repoint → verify runbook is rehearsed as a test
  (`tests/backup.rs::final_dump_restore_runbook_roundtrip`). PITR via WAL
  archiving is deferred.

## Previously shipped (OAuth & per-tenant MCP)

- **Per-account OAuth 2.0** — cloud's own Dynamic Client Registration +
  Authorization Code + PKCE (S256) flow on the tenant subdomain
  ([`oauth_routes.rs`](src/oauth_routes.rs)), storing clients/codes in the
  control plane scoped by `account_id` ([`oauth_store.rs`](src/oauth_store.rs),
  migration `014`). Discovery is per-tenant (issuer = the addressed origin);
  the approve step authenticates the session cookie, not a pasted token.
  Hash-only client secrets and codes, single-use 60s codes, S256 verification
  against an RFC 7636 fixture. atomic-server's self-hosted OAuth handlers are
  left untouched.
- **Per-tenant MCP** — `/mcp` mounts behind CloudAuth; atomic-server's MCP
  transport resolves its `DatabaseManager` from the per-request
  `RequestDatabaseManager` extension (a cloud-unaware generality mirroring the
  data plane's `Db` extractor — self-hosted runs byte-identical via the
  baked-in fallback), so each tenant's MCP tool calls hit its own KB. An
  unauthenticated `/mcp` returns the MCP `WWW-Authenticate` challenge pointing
  at *this tenant's* OAuth discovery.
- **MCP token default scope** — OAuth-minted tokens are `scope='mcp'` with
  account-level access (`allowed_db_id = NULL`); db-pinned tokens are still
  honored and chokepoint-enforced. The full Claude-Desktop journey
  (discovery → DCR → authorize → token → `initialize` + `tools/call`) and the
  cross-tenant isolation / PKCE-replay / expired-code / db-pin cases are driven
  over real HTTP in [`tests/e2e_oauth.rs`](tests/e2e_oauth.rs).

## Previously shipped (billing & quotas)

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
