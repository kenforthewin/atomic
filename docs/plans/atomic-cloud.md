# Atomic Cloud — Postgres-backed Multi-Tenant Hosting

## Status

Living plan, iterating section by section. Sections marked **[drafted]** have
been worked through; sections marked **[stub]** are placeholders for future
deep dives. Decisions made so far are recorded in the "Decisions log" at the
bottom — when you change one, update the log so future-us knows what shifted.

Implementation began 2026-06-10. Slice 1 (control plane, provisioning core,
CloudAuth/AccountCache, composed cloud server + multi-tenant e2e suite) is on
branch `cloud-control-plane`; see the **Implementation log** section for what
landed, where it deviates from this plan, and what each slice deferred.

## Context

The earlier exploration of "Atomic Cloud on SQLite + per-customer mounted
volumes" was working around the wrong constraint. Cloud users are by
definition trading privacy for ease of access; given that, the natural
storage choice is the Postgres engine we already built (see
`crates/atomic-core/src/storage/postgres/`). It ships with pgvector,
native-async sqlx, advisory-locked migrations, and a `db_id` column that
scopes per-tenant data — enough that the storage layer is essentially
feature-complete for cloud already. The work is the layer *above* it:
accounts, auth, tenant routing, provisioning, worker fairness, billing.

Self-hosted Atomic stays on SQLite. Cloud is purely Postgres. The storage
trait abstraction earns its keep here — same `atomic-core`, two deployment
shapes.

## Goals

- Multi-tenant hosting on a shared Postgres cluster, one database per
  account.
- Account-scoped auth, OAuth, sessions, and MCP integration.
- Cloud code lives in its own crate; `atomic-core` is untouched and
  `atomic-server` accepts only cloud-unaware generality refactors.
- Each account gets a subdomain (`<slug>.atomic.cloud`) which doubles as the
  primary tenant-routing input.
- Self-hosted Atomic continues to work exactly as before, on SQLite, with no
  feature flags or cloud-aware code paths.

## Non-goals (v1)

- Bring-your-own-domain (custom domains pointed at an account). Paid add-on
  later.
- Cross-account features (shared knowledge bases, team workspaces). One
  account = one user for v1.
- Cluster sharding / region selection. One cluster, one region. Capacity
  ceiling around 2-3k accounts before we need to split; `account_databases`
  carries `cluster_id` from day one so the split is mechanical.
- Migration of existing self-hosted databases into cloud. Cloud signups are
  fresh accounts; users can import via the existing export/import flow but
  tokens are always reissued.

## Architectural principles

**Isolation directive.** Cloud code must be as separate as possible from
`atomic-core` and `atomic-server`. Concretely:

- New `crates/atomic-cloud/` holds everything cloud-specific.
- Dependency arrow is one-way: `atomic-cloud → atomic-server → atomic-core`.
- No `#[cfg(feature = "cloud")]` gates in `atomic-core` or `atomic-server`.
- Grepping the string `cloud` in those crates should find nothing.
- Refactors to `atomic-server` are acceptable *only* when they're justifiable
  as pure generality improvements (route registration as a library function,
  request-extension-based resolution). Cloud-driven, but cloud-unaware.

**Two tiers of "database."** Don't conflate them.

- **Tenant database** (`acct_<uuid>`) — one Postgres database per account,
  on the shared cluster. Runs the existing 18 migrations. The boundary for
  isolation, backup, and (eventually) sharding.
- **User-facing knowledge base** — the existing `db_id` column inside a
  tenant database. A single account can still have work-kb, personal-kb,
  etc. The boundary for user-level organization, *not* tenancy.

The administrative boundary is the Postgres database; `db_id` is the user's
organizational tool inside their tenant.

**Subdomain as primary tenant key.** Each account gets a subdomain. The
`Host` header is the first thing the auth middleware reads — host → account →
token. This collapses several MCP/OAuth questions into a single primitive
and gives us free browser-level cross-tenant isolation (cookies, localStorage,
CORS all bound to origin).

## Crate layout

```
crates/
  atomic-core/         unchanged
  atomic-server/       three small generality refactors (see below); cloud-unaware
  atomic-cloud/        new; binary + library; depends on atomic-server
```

The `atomic-cloud` binary composes `atomic-server`'s route registration with
its own middleware, control-plane handle, and account-management routes. The
self-hosted `atomic-server` binary keeps working exactly as before.

An earlier, never-shipped management-plane prototype lives at
`crates/atomic-cloud` on main (Fly machine-per-customer model). This plan
supersedes its architecture wholesale: the Fly provisioning dies, but the
magic-link flow, Mailgun and Stripe clients, and signup frontend are
salvageable. Treat it as a parts bin, not a base.

## Tenant model

- Each account owns one Postgres database on the shared cluster, named
  `acct_<uuid>` (UUID, not the subdomain — subdomains are renameable, UUIDs
  are not).
- The cluster also hosts the **control plane database** (`atomic_cloud_control`),
  which is separate from any tenant DB.
- Subdomain → account_id is looked up in control plane and cached.
- pgbouncer sits in front of the cluster in transaction-pooling mode so the
  per-account `sqlx::PgPool` instances can be small (e.g. 5 conns max) without
  blowing the cluster's `max_connections`.

### Subdomain rules

- Users pick a vanity slug at signup (3–32 chars, `[a-z0-9-]`).
- Reserved blocklist: `www`, `app`, `api`, `mcp`, `admin`, `support`,
  `status`, `docs`, `blog`, `auth`, `login`, `signup`, plus the usual ~50
  others. Maintain in `atomic-cloud/src/reserved_subdomains.rs`.
- Public enumeration via DNS is accepted as the norm (Slack, Notion, Linear
  all do this).
- Wildcard cert (`*.atomic.cloud`) via Let's Encrypt DNS-01.
- Wildcard A-record (`*.atomic.cloud` → load balancer).
- `app.atomic.cloud` for the marketing site / signup; per-account subdomains
  for the actual product.

## Auth & tenant routing **[drafted]**

### Control plane schema (first cut)

```
accounts            (id, subdomain UNIQUE, email, status, plan,
                     last_active_db_id?, created_at, deleted_at)
account_databases   (account_id, cluster_id, db_name, status, created_at)
cloud_tokens        (hash, account_id, scope, allowed_db_id?, name,
                     created_at, last_used_at, expires_at?, revoked_at?)
sessions            (hash, account_id, created_at, expires_at,
                     ip_first_seen, ua_first_seen)
oauth_clients       (account_id, client_id, client_secret_hash,
                     client_name, redirect_uris, created_at)
oauth_codes         (code_hash, account_id, client_id, code_challenge,
                     redirect_uri, created_at, expires_at, used, token_id)
provider_credentials (account_id, provider, origin, external_key_id?,
                      encrypted_key, model_config, created_at, rotated_at)
```

Notes:

- `account_databases.cluster_id` from day one for future shard split.
- `oauth_clients` and `oauth_codes` are per-account in cloud (vs. server-wide
  in self-hosted) — each subdomain has its own OAuth identity.
- `cloud_tokens` is the single source of truth for all tokens (account-scope,
  KB-scope, MCP-scope). No per-tenant `api_tokens` table in cloud.
- `provider_credentials.encrypted_key` — encrypted at rest via the
  `KeyVault` trait; see the provider management section.

### Token model

- All tokens live in `cloud_tokens` (option A from the deep dive).
- Format: opaque `atm_<random>` with SHA-256 hash stored. The subdomain
  provides account context, so the token itself doesn't need account-encoding.
- Scope enum: `account` (full access), `database` (one `db_id`), `mcp`
  (MCP-issued, typically database-scoped, OAuth-tied).
- Sessions are separate from tokens — different table because their
  lifetimes and revocation UX differ.

### CloudAuth middleware

Order of operations:

1. Read `Host` header → strip base domain → subdomain.
2. Look up `accounts WHERE subdomain = ?` → 404 if not found.
3. Extract bearer token OR session cookie.
4. Verify against `cloud_tokens WHERE account_id = ? AND hash = ?`
   (or `sessions` for cookie path).
5. Build `AuthPrincipal { account_id, scope, allowed_db_id?, source }`.
6. Resolve `Arc<DatabaseManager>` for the account via `AccountCache`.
7. Insert `ResolvedTenant { principal, manager, event_tx }` into request
   extensions.

The middleware is the entire authorization layer. Route handlers see a
`ResolvedTenant`, never a raw token.

### AccountCache

`HashMap<AccountId, Entry>` with idle TTL eviction and a hard cap.

```rust
struct Entry {
    manager: Arc<DatabaseManager>,    // pointing at acct_<uuid>
    event_tx: broadcast::Sender<ServerEvent>,
    last_touched: Instant,
}
```

On miss: look up `account_databases` → connect a fresh `PostgresStorage` to
the tenant's database → wrap in `DatabaseManager` → insert.

Idle TTL number is TBD; rough target 10–30 minutes for v1. Tune from
production data.

Eviction must not orphan live WebSocket subscribers — `event_tx` lives in
the entry, and a quiet-but-connected WS client would otherwise keep
listening on a channel nothing publishes to. Skip entries with
`event_tx.receiver_count() > 0`, or count WS activity as a touch.

### Db extractor change

Today's `Db` extractor (in `atomic-server`) reads from `AppState.manager`.
After the refactor, it reads from request extensions, with `AppState.manager`
as fallback. The refactor is cloud-unaware — it just makes the extractor
generic over where the manager comes from.

The chokepoint check: if `AuthPrincipal.allowed_db_id` is set, the resolved
`db_id` (from `X-Atomic-Database` header or `last_active_db_id`) must
match. Single test asserts this. Without it, a database-scoped MCP token
could read another KB via header override.

A sibling chokepoint test covers the cross-tenant case: a valid session or
token for account A presented on account B's subdomain must fail with
401/404. The `.atomic.cloud` cookie crosses subdomains by design, so this
check — middleware step 4's `WHERE account_id = ?` — is what actually
enforces browser-level tenant isolation. The middleware provides it; the
test pins it.

### "Active database" concept

Survives but moves into the control plane: `accounts.last_active_db_id`.
Behaviorally identical to today from the user's POV — the frontend can omit
`X-Atomic-Database` and the server picks the user's last-selected KB. Updated
*only* when the user explicitly switches, not on every request, to avoid
making it a hot row.

### Web sessions

Server-stored sessions in `sessions` table; opaque cookie holds the session
hash. Cookie domain is `.atomic.cloud` (note leading dot) so it works across
all subdomains the user visits — needed for cross-account dashboards, account
switcher, etc. `Secure; HttpOnly; SameSite=Lax`.

Login page lives at `app.atomic.cloud/login`. After auth, redirects to
`<chosen_subdomain>.atomic.cloud/`.

### OAuth

Cloud has its own OAuth flow in `atomic-cloud`. We do **not** extend
`atomic-server`'s OAuth handlers with pluggable storage. The flow is
structurally the same (Dynamic Client Registration + Authorization Code +
PKCE) but each endpoint resolves `account_id` from the host before doing
anything. `atomic-server`'s existing OAuth implementation remains untouched
and continues to serve self-hosted.

### MCP token UX

With subdomains, the MCP setup is one piece of information per account:
`https://<slug>.atomic.cloud/mcp`. Claude Desktop's OAuth flow against that
URL produces an MCP-scoped token automatically. Users don't paste tokens
manually.

Open question: do MCP tokens default to account-scope or per-KB? Tracked in
**Open questions** below.

## atomic-server refactors required

Three changes, all cloud-unaware:

1. **Route registration as a library function.** Extract the actix `App`
   wiring into `pub fn configure_routes(cfg: &mut web::ServiceConfig)` that
   `atomic-cloud` can call after wrapping the scope in its own middleware.

2. **`Db` extractor reads from request extensions, falls back to
   `AppState.manager`.** Self-hosted gets a tiny default middleware that
   populates the extension from `AppState.manager` (no behavior change);
   cloud installs its own middleware that populates from `AccountCache`.

3. **`event_tx` becomes injectable via request extensions** with
   `AppState.event_tx` as fallback. Same pattern as #2 for per-account WS
   channels.

None of these mention cloud. They're each defensible as "make atomic-server
more reusable" on their own merits.

## atomic-core changes required

**None**, given the decisions below.

- Provider config moves to an explicit `Option<ProviderConfig>` parameter on
  `AtomicCore::open*` constructors (option a from the deep dive). When
  `Some`, used directly. When `None`, falls back to today's
  "read from settings" behavior. Self-hosted always `None`; cloud always
  `Some`. atomic-core has no idea why.
- Live config update: a single `update_provider_config` method on
  `AtomicCore` that both modes call. Self-hosted writes to settings then
  reloads; cloud reloads from control-plane state.
- The registry-vs-storage settings split (already in `lib.rs`) accommodates
  registry-less mode — cloud's `AtomicCore` simply has no registry attached.
  No change needed.

## Provisioning lifecycle **[drafted]**

### Signup

Synchronous, inline with the HTTP request, capped at 4–8 concurrent in-flight
provisions per process. Happy path ~2–5 seconds. Steps:

1. Validate (email format, subdomain regex `[a-z0-9-]{3,32}`, not reserved).
2. Magic link sent. User clicks → token consumed → flow continues.
3. Atomically claim the subdomain via UNIQUE constraint:
   `INSERT INTO accounts (id, subdomain, email, status='provisioning', ...)`.
   The UNIQUE failure path is what makes "subdomain taken" a race-free check.
4. `CREATE DATABASE acct_<base32(uuid)>` on the cluster.
5. Connect a fresh `PostgresStorage`; call `initialize()` (runs migrations).
6. Seed `databases` row inside the tenant DB: `(id='default', name='Default',
   is_default=true)`.
7. Seed per-DB default settings (wiki prompt template, etc.). Do **not** seed
   provider config — that lives in the control plane (see provider
   management).
8. Seed the default Report (per the reports plan).
9. Provision the managed OpenRouter key: create a runtime key via the
   provisioning API with the plan's monthly credit allowance and monthly
   reset, encrypt via `KeyVault`, insert `provider_credentials`
   (`origin='managed'`).
10. Insert `account_databases (account_id, cluster_id, db_name, status='active')`.
11. Flip `accounts.status='active'`.
12. Create session, set cookie, redirect to `<slug>.atomic.cloud/`.

**Idempotency** — each step is independently idempotent so a crashed signup
can be retried or reaped:

- `SELECT FROM pg_database WHERE datname = ?` before CREATE.
- Migrations are idempotent via `schema_version` + advisory lock.
- Seed inserts use `ON CONFLICT DO NOTHING`.
- Managed-key provisioning checks for an existing `provider_credentials`
  row first (OpenRouter key creation itself is not idempotent); rollback
  deletes any key that was created before the crash, using
  `external_key_id`.

**No starter atoms** — render an empty-state UI explaining how to capture a
first atom rather than seeding fake content.

**Safety-net reaper** picks up rows stuck in `status='provisioning'` for >5
minutes and either retries or rolls back (DROP DATABASE WITH FORCE if the
database exists, delete any provisioned OpenRouter key, mark
`accounts.status='failed'`, free the subdomain).

### Account deletion

Hard delete v1 (no grace period, no soft-delete). User confirms → everything
gone.

1. Revoke all `cloud_tokens` (set `revoked_at`).
2. Invalidate all `sessions`.
3. Delete the managed OpenRouter key via the provisioning API
   (`origin='managed'` rows).
4. Take a final logical dump to the backup bucket (`backups/final/`,
   30-day retention) — the operator's only undo for a fat-fingered
   confirmation or a deletion-path bug. See Backups & DR.
5. Evict `AccountCache` entry, drain pool.
6. Terminate stragglers: `SELECT pg_terminate_backend(pid) FROM
   pg_stat_activity WHERE datname = ?` (or rely on `DROP DATABASE WITH FORCE`).
7. `DROP DATABASE ... WITH (FORCE)`.
8. Delete `account_databases` row.
9. Hard-delete `accounts` row.
10. Reserve the subdomain in `subdomains_reserved (subdomain, expires_at =
    now() + 90 days)` to prevent confusion if external clients (RSS readers,
    MCP configs) still point at the old name.

### Schema migration on deploy

The new binary boots in **migrating mode** and doesn't pass readiness until
fleet migration completes. One mechanism, one policy, in one place.

Compile-time `TARGET_SCHEMA_VERSION = N`. On boot:

1. Enumerate `account_databases WHERE status='active' AND
   last_migrated_version < N`.
2. Fan out with concurrency cap (start at 16, tune from production).
3. Per tenant: connect, call `storage.initialize()`, record outcome
   (`last_migrated_version`, `last_migrated_at`, or `migration_failed_at` +
   `last_migration_error`).
4. While migrating: liveness ready, readiness NOT ready.
5. On completion, compute failure rate and apply policy:

| Failure rate | Action |
|---|---|
| 0% | Flip readiness ready. |
| 0 < x < 1% | Flip ready. Stragglers get hold-message; reaper retries. |
| 1% ≤ x < 10% | Stay not-ready. `deploy_status='awaiting_review'`. Operator inspects and either advances or rolls back. |
| x ≥ 10% | Stay not-ready. `deploy_status='rollback_required'`. Migration is broken. |
| Migration runs > 30 min | Stay not-ready. `deploy_status='migration_timeout'`. |

**Rolling deploys** work without coordination because migrations are
**additive-only**: ADD COLUMN, CREATE TABLE, CREATE INDEX, deferred/not-validated
constraints. No DROP COLUMN, no ALTER COLUMN TYPE, no rename. Drops happen
N+1 deploys later, after all referring code is out of the fleet. Enforced by
a custom lint in atomic-cloud's CI that scans migration SQL.

**Stragglers** — when CloudAuth resolves an account with
`last_migrated_version < TARGET_VERSION`, it returns 503:

```json
{ "error": "account_upgrading",
  "message": "Your account is being upgraded. Try again shortly.",
  "retry_after_seconds": 60 }
```

Frontend renders a friendly upgrade screen. MCP clients back off and retry.
The always-running reaper retries failed migrations on a backoff schedule and
alerts when `retry_count > 5`.

**Rollback** is structurally safe with additive-only migrations: rolling
back the binary to version M while some tenants are on schema M+1 means old
code reads extra columns it doesn't know about (ignored). The forward-roll
later is a no-op for already-migrated tenants and a retry for the rest.

**Multi-pod boot**: every pod boots in migrating mode and races over the
fleet. Per-tenant advisory locks make this safe, merely wasteful. If deploy
times start to hurt, have one pod claim the migration run via a
control-plane lock — deferred until it hurts.

### Failure recovery & the reaper

One periodic job, runs every ~60s, takes a control-plane advisory lock keyed
on `account_id` for each row it processes (multiple atomic-cloud processes
can run reapers concurrently):

- Stuck provisioning: `accounts WHERE status='provisioning' AND created_at <
  now() - interval '5 minutes'`.
- Failed migrations: `account_databases WHERE migration_failed_at IS NOT
  NULL AND (migration_retry_after IS NULL OR migration_retry_after <= now())`.
- Anything else: same shape.

Probably the same job runner handles reapers, feed polling, scheduler — see
the worker-fairness deep dive.

## Worker fairness & job queue **[drafted]**

### Shape

A central **dispatcher** in `atomic-cloud`, fed by the existing durable
ledgers (`atom_pipeline_jobs`, `task_runs`) inside tenant DBs, dispatching to
**bounded worker pools per work class** with **per-tenant fairness**. No new
storage primitive; the ledgers stay where they are.

```
DURABLE LEDGERS (inside each tenant DB)
  atom_pipeline_jobs   (per db_id)
  task_runs            (per db_id; reports, scheduled tasks, feed-polls, wiki regen)
        ↓
DISPATCHER (one per atomic-cloud pod, no leader election)
  poll → round-robin per tenant → submit
        ↓
WORKER POOLS (in-memory, per-pod, per class)
  embedding   32 total / 4 per-tenant
  llm         16 total / 2 per-tenant
  ingestion   16 total / 4 per-tenant
  maintenance  8 total / 1 per-tenant
        ↓
Provider calls + tenant DB writes
```

Initial cap numbers are guesses calibrated to ~50 active tenants per pod. Real
numbers come from load testing; ship conservative, raise from metrics.

Caps are **per-pod**: a noisy tenant's effective fleet-wide concurrency is
`per-tenant cap × pod count`. Fine at small pod counts; remember this when
scaling out, because adding pods loosens fairness without any config change.

### Selection algorithm

Plain **round-robin per tenant** within each pool's ready-queue: deque of
per-tenant deques. Pop a tenant, take one job, push the tenant back. Skip
tenants over their per-tenant cap. Drop tenants with empty deques.

Plan-tier weighted fairness is deferred — uniform weights v1, switch to
weighted/DRR when plan tiers exist. Data model needs no preparation; weights
derive from `accounts.plan` when added.

### Cross-tenant ledger scan

Dispatcher uses **N+1 polling with a pending-work hint bit**.

- Application code that enqueues a ledger row also writes to a control-plane
  `dispatch_hints (account_id, last_enqueued_at)` table.
- Dispatcher reads `dispatch_hints` first — only polls tenant DBs that have
  the hint set. Idle tenants are skipped entirely.
- When a tenant's ledger comes back empty, dispatcher clears the hint.
- If a hint write fails (dual-write inconsistency), the work sits in the
  ledger until the next time *someone* enqueues for that tenant; not great
  but bounded. A slow-path full scan every N minutes catches orphans.

Pgbouncer transaction-pooling absorbs the per-tenant connection cost. At
scale (thousands of active tenants per pod), revisit and consider moving to
the full outbox pattern.

### Per-pod, no leader election

Each `atomic-cloud` pod runs its own dispatcher. `FOR UPDATE SKIP LOCKED` on
ledger claims guarantees no double-dispatch. Jittered polling intervals
across pods reduce thundering-herd cost on `dispatch_hints`. Leader election
is the optimization-when-it-hurts answer, not the v1 answer.

### Streaming chat (not in a pool)

Request-driven, user-facing, latency-critical. Per-tenant semaphore at the
route handler (cap = 3 concurrent streams). Provider rate limits do the
actual throttling downstream. No queue involvement.

### Provider rate-limit handling

Two layers:

1. **Local retry with backoff** — worker that hits 429 records the
   rate-limit-reset header into `task_runs.next_attempt_at` or
   `atom_pipeline_jobs.not_before`, releases the lease. Ledger handles
   re-dispatch.
2. **Per-tenant circuit breaker** — 3 consecutive 429s in 60s pauses that
   tenant's dispatch for a cool-down (60s, doubling). State lives in
   `accounts.provider_paused_until`. Also handles "BYOK key expired" and
   managed-key credit exhaustion (OpenRouter 402) — the breaker stays open
   until the key is fixed, the allowance resets, or the user upgrades.

### How each work-type lands

| Work type | Today | Cloud |
|---|---|---|
| Embedding/tagging | `atom_pipeline_jobs` ledger + spawn | Same ledger; dispatcher → embedding pool |
| Wiki regen | Fire-and-forget on tag change | New `task_runs` entry `wiki.regenerate`; LLM pool |
| Reports | `task_runs` (already) | Same; LLM pool, per-tenant cap 1 |
| Feeds | 60s loop, special-case | **Move to `task_runs`**; ingestion pool |
| DraftPipelineTask, GraphMaintenanceTask | 15s loop with lock map | **Move to `task_runs`**; maintenance pool |
| Streaming chat | Handler streams provider | Same, with per-tenant route-handler semaphore (cap 3) |
| Canvas warmup | One-shot on boot | Lazy; "all-tenants boot" doesn't apply in multi-tenant |

The "move to `task_runs`" rows are a separate workstream (see below).

### Restart semantics

Standard. Pod restart drops in-memory ready-queues. Durable ledgers re-claim
expired-lease jobs. In-flight streaming chats terminate; frontend retries.

## `task_runs` unification (cross-cutting workstream)

Moving feed polling, `DraftPipelineTask`, `GraphMaintenanceTask`, and wiki
regen into `task_runs` is a refactor of `atomic-core` (where task definitions
live) and `atomic-server` (which dispatches them today). It is **not
cloud-specific** — `task_runs` was designed for this from the start
(see comment in migration 015 referring to phase 1.5's dormant-helper
ship). Self-hosted benefits from the unification too: one durable ledger
with the existing claim/lease/crash-recovery semantics replaces several
ad-hoc loops.

The plan for this already exists: `docs/plans/durable-task-runs.md`. As of
2026-06-09, its phase 1 (ledger schema + claim/lease helpers) is landed and
reports already dispatch through it; the remaining work is retrofitting
`DraftPipelineTask`/`GraphMaintenanceTask`, folding in feed polling, adding
wiki regen (added to scope by this plan), and the retention GC. The work
lives in atomic-core/atomic-server, cleanly. Atomic-cloud just relies on
`task_runs` being the single source of pending work.

Sequencing-wise: unification can land first, before atomic-cloud exists, and
ride to production in self-hosted. By the time atomic-cloud's dispatcher is
built, all background work is already going through one ledger.

## Provider management **[drafted]**

**Managed by default, BYOK as the escape hatch.** Every account gets a
platform-provisioned OpenRouter key at signup, created via OpenRouter's
provisioning API with a hard per-key credit limit and native monthly reset
(midnight UTC). Users who want to exceed platform allowances — or just
prefer their own billing — can switch to a BYOK OpenRouter or
OpenAI-compatible key in settings. Platform-proxy with per-call metering
stays v2. Ollama is **not supported in cloud** — local-only by definition.

This supersedes the earlier "BYOK only for v1" decision. Rationale: BYOK at
the front door puts "go create an OpenRouter account and paste a key"
before the product's first magic moment — embedding, tagging, and chat are
all dead until then, which is exactly backwards for a growth-first launch.
And the provisioning API removes the original margin-risk argument: each
tenant key carries a credit limit that *OpenRouter* enforces, with
automatic monthly resets, so worst-case spend per account is the allowance
we set, regardless of bugs in our own metering. Signup becomes "magic link
→ it works."

### Managed key lifecycle

| Event | Action |
|---|---|
| Signup | Create runtime key (`POST /api/v1/keys`) with the plan's monthly credit allowance + monthly reset; encrypt, store with `origin='managed'` |
| Plan change | PATCH the key's credit limit |
| Allowance exhausted | OpenRouter 402 → jobs sit in ledger as `blocked_on_credits`; chat/wiki/reports return a structured "out of AI credits" error with reset date + upgrade link |
| Switch to BYOK | New `origin='user'` row, flip `accounts.active_provider`; managed key kept (switching back is a column flip) |
| Account deletion | DELETE the key via provisioning API |

Two pieces of shared infrastructure come with this:

- The **master OpenRouter account** funds every managed tenant. Its prepaid
  balance needs monitoring and auto-top-up, and it is a single point of
  failure — an empty balance is an all-tenants outage.
- The **provisioning key** can mint runtime keys against our balance.
  Crown-jewel custody, same as the KeyVault master key: sealed-secret at
  deploy, never stored in the control plane.

### Model curation (managed mode)

We pay for managed inference, so we pick the models:

- **The embedding model is pinned fleet-wide.** Not user-changeable:
  switching embedding models invalidates every stored vector and triggers a
  full re-embed billed to the platform.
- Tagging, wiki, and chat run on a curated list of 2–3 cost-effective
  models; users pick within the list.
- Frontier-model access is a paid-tier feature flag (`plans.feature_flags`),
  not a free-tier option.

BYOK accounts choose models freely — their key, their bill. (An
embedding-model switch still forces a full re-embed; warn loudly.)

### Storage schema

```sql
provider_credentials (
    account_id              TEXT NOT NULL,
    provider                TEXT NOT NULL,   -- 'openrouter' | 'openai_compat'
    origin                  TEXT NOT NULL,   -- 'managed' | 'user'
    external_key_id         TEXT,            -- OpenRouter key id; managed rows only
    encrypted_key           BYTEA NOT NULL,
    nonce                   BYTEA NOT NULL,  -- 96-bit, fresh per encryption
    encryption_version      INT  NOT NULL,   -- master-key generation
    model_config            JSONB NOT NULL,  -- { embedding_model, llm_model, ... }
    created_at              TIMESTAMPTZ NOT NULL,
    rotated_at              TIMESTAMPTZ,
    last_used_at            TIMESTAMPTZ,
    last_validated_at       TIMESTAMPTZ,
    last_validation_error   TEXT,
    PRIMARY KEY (account_id, provider, origin)
)
```

`origin` distinguishes platform-provisioned from user-provided keys;
`external_key_id` is the OpenRouter identifier needed to PATCH/DELETE
managed keys. `accounts.active_provider` selects which row is the active
config. The composite PK lets managed and BYOK rows coexist, so switching
between them is a column flip, not a re-provision.

Model selection (`model_config`) lives **with the key** in control plane, not
in per-DB settings. Rationale: provider config is account-level — different
KBs sharing one account using different models is more flexibility than users
want. Per-DB override remains an optional future feature.

### Encryption at rest

Wrapped behind a `KeyVault` trait in atomic-cloud with two methods:

```rust
trait KeyVault {
    fn encrypt(&self, account_id: &str, provider: &str, plaintext: &[u8])
        -> Result<(Vec<u8>, Vec<u8>, i32)>;  // ciphertext, nonce, version
    fn decrypt(&self, account_id: &str, provider: &str, ct: &[u8], nonce: &[u8],
        version: i32) -> Result<Vec<u8>>;
}
```

**v1 implementation `EnvMasterKeyVault`**: AES-256-GCM with 32-byte master key
loaded from env at process start. Fresh nonce per row. AAD = `account_id ||
provider`, binding ciphertext to its row.

**v2 implementation `KmsEnvelopeVault`**: per-account DEKs encrypted by a KMS
master key. DEK ciphertext stored alongside `encrypted_key`. Cached in
AccountCache so KMS calls amortize across requests. Same schema; swap is
contained.

Master key rotation in v1: bump `encryption_version`, lazy re-encrypt on
next access. Master key custody: sealed-secret at deploy, backed up
out-of-band. **Loss of master key = unrecoverable keys.** Document
explicitly in operator runbook.

### BYOK entry & validation

Signup never asks for a key — the managed key covers onboarding. BYOK is a
settings-page feature:

- Settings page at `<slug>.atomic.cloud/settings/provider` for key
  entry/rotation and switching between managed and BYOK.
- **Existing key is never displayed.** Status only ("configured ✓, last
  validated 3h ago"). Rotation = replace.
- **Validation on save** — test call against the provider before storing:
  OpenRouter `GET /api/v1/auth/key`; OpenAI-compat minimal embedding call.
  Failure surfaces provider's error verbatim, rejects the save.
- **Periodic re-validation** — deferred. See Open questions.

### Blocked states

With managed keys there is no "no provider configured" state for new
accounts. Two blocked states remain, both reusing the same ledger-hold
pattern as `account_upgrading`:

- **`blocked_on_credits`** (managed) — monthly allowance exhausted.
  Background jobs sit in the ledger until the allowance resets or the user
  upgrades; interactive features return the structured "out of AI credits"
  error. Atoms still create/update fine.
- **`blocked_on_provider`** (BYOK) — key expired, revoked, or out of the
  user's own credits. Jobs sit until the user fixes the key; frontend
  banner directs to the settings page.

### Plumbing — control plane → AtomicCore

On AccountCache miss:

1. Load `provider_credentials` row for the account's active provider.
2. `KeyVault::decrypt(...)` → plaintext key.
3. Build `ProviderConfig` from row + decrypted key.
4. `AtomicCore::open_postgres(cluster_url, "acct_<id>", "default",
   Some(provider_config))`.

Cloud always passes `Some(ProviderConfig)` — never `None` (which would route
through atomic-core's settings-table fallback). If no row exists, pass a
`ProviderConfig` with `*_api_key: None` — atomic-core builds providers in
"missing key" state that reject calls with a structured error.

### Live rotation

Same path for both origins; for managed keys, "new key" means provisioning
a replacement via the API and deleting the old one after the swap.

1. Validate new key.
2. UPSERT `provider_credentials` (bump `rotated_at`).
3. Build fresh `ProviderConfig`.
4. Look up the AccountCache entry, call `core.update_provider_config(new)`.
5. In-flight requests using the old config complete; new requests use the new.
6. **Clear `accounts.provider_paused_until`** if circuit-breaker was open —
   new key deserves a fresh chance.

### In-process hygiene

- **Custom `Debug` impl on `ProviderConfig` redacts `*_api_key` fields.**
  This change lives in `atomic-core`, not atomic-cloud — it's pure hygiene,
  useful for self-hosted logging too, and not cloud-aware.
- Never include the key in error messages, traces, or logs. Audit
  instrumentation around provider calls.
- No "zeroize on drop" — overkill for our threat model. Standard Rust drop
  semantics free the key.

### Audit / visibility in settings UI

- Provider, managed vs BYOK, configured ✓ (no key value, ever).
- Managed: allowance usage ("62% of monthly AI credits used, resets June 1")
  — from the OpenRouter key-usage endpoint, cached, with the local advisory
  counter as fallback.
- Last validated, last used.
- Current status (Healthy / Paused / Out of credits / Failing).
- Recent errors (timestamp + redacted message).

### Trust-building docs (launch task)

Customer-facing "where does my key go?" page explaining encryption,
in-process decryption, no-logging discipline (managed keys get the same
treatment as BYOK keys). B2B norm. Not architectural, but write it for
launch.

## Observability, quotas, billing **[drafted]**

### Observability

Three audiences (operator, user, support); four data kinds.

**Metrics.** Split per-tenant (small set, operationally critical) from
per-cluster (everything else) to bound cardinality.

| Metric | Cardinality |
|---|---|
| `requests_per_account_total{account_id}` | Per-tenant |
| `provider_errors_per_account_total{account_id, provider, code}` | Per-tenant |
| `queue_depth_per_account{account_id, pool}` | Per-tenant |
| `http_request_duration_seconds{route, status}` | Per-cluster |
| `worker_pool_in_flight{pool}` | Per-cluster |
| `account_cache_size` | Per-cluster |

Higher-cardinality detail (per-tenant latency) defers to traces.

**Structured logs.** Every route, worker job, and provider call emits
JSON with `account_id`, `db_id`, `request_id`. Discipline: route handlers
never call `tracing::info!()` directly — they use a helper that injects
account context from the request. Clippy lint enforces.

**Tracing.** OpenTelemetry with `account_id` as a root-span attribute
propagated down. Head sampling (1–5%) baseline; tail sampling for errors
if our backend supports.

**Per-account event log (user-facing)** — distinct from system logs. Schema
in tenant DB:

```sql
account_events (
    id BIGSERIAL PRIMARY KEY,
    db_id TEXT NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL,
    event_type TEXT NOT NULL,
    subject_id TEXT,
    metadata JSONB
)
```

Rows only for discrete, named outcomes the user cares about (atom creation,
report run, wiki regen, provider failure). High-volume operations
(embedding chunks) stay in logs and roll up into daily counters in
`quota_usage`. TTL via partitioned tables (90 days default).

### Quotas

Two categories with very different consistency requirements:

**Anti-abuse rate limits** — sliding-window per-pod counters via `governor`,
keyed by account_id. Approximate consistency is fine. Defaults:

| Limit | Window | Default |
|---|---|---|
| API requests | per min | 600 |
| Signup attempts | per IP per hour | 5 |
| Magic link requests | per email per hour | 3 |
| URL ingestion | per min | 30 |
| Atom creates | per min | 60 |

**Plan-tier resource limits** — strong consistency via Postgres UPSERT.

```sql
plans (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    monthly_price_cents INT,
    atom_limit INT,                       -- NULL = unlimited
    ai_credits_monthly_cents INT,         -- managed-key allowance; OpenRouter enforces
    kb_limit INT,
    storage_bytes_limit BIGINT,
    feature_flags JSONB
)

quota_usage (
    account_id TEXT NOT NULL,
    period_start DATE NOT NULL,
    metric TEXT NOT NULL,
    value BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (account_id, period_start, metric)
)
```

Plan-tier configuration in code or a small seeded table. `accounts.plan_id`
references it.

**Enforcement points:**

| Where | Check | Action on hit |
|---|---|---|
| CloudAuth middleware | Rate limit | 429 with `Retry-After` |
| Atom create | `atoms_count < limit` | 402 with quota error |
| KB create | `kb_count < limit` | 402 with quota error |
| Provider call (managed) | OpenRouter per-key credit limit (hard stop) | 402 → jobs **block** as `blocked_on_credits` |
| Periodic reaper | Storage bytes recompute | Week 1 warn; week 2 restrict writes; **no auto-delete** |

AI spend is the one quota we do **not** enforce ourselves: the managed
key's credit limit is the hard stop, enforced by OpenRouter with native
monthly reset. The internal `quota_usage` AI counter is advisory UX ("80%
of allowance used") — a bug in it can mislead a progress bar but can't run
up a bill. BYOK accounts have no platform AI limit at all.

Quota-exceeded response shape:

```json
{ "error": "quota_exceeded",
  "metric": "ai_credits",
  "current": 50,
  "limit": 50,
  "resets_at": "2026-07-01T00:00:00Z",
  "upgrade_url": "https://app.atomic.cloud/billing" }
```

Background jobs that exhaust the AI allowance **sit in the ledger** (not
fail) until it resets or the user upgrades. Same hold-message pattern as
account-upgrading.

Period rollover: AI allowances reset natively at OpenRouter (monthly,
midnight UTC) — no rollover code needed for them. A 1-hour-cadence job
inserts new `period_start` rows for the remaining metrics. Old rows kept
for billing/audit.

### Billing

**v1 model:** subscription with included AI credits. Each plan's monthly
price includes a managed-key allowance (`ai_credits_monthly_cents`) that
OpenRouter enforces per key — no per-call metering, no usage-based
invoicing, and the margin math is just "allowance < price." BYOK accounts
take the AI cost off our books entirely. Platform-proxy with metered
passthrough is v2.

```sql
stripe_customers (
    account_id TEXT PRIMARY KEY,
    stripe_customer_id TEXT UNIQUE NOT NULL,
    default_payment_method_id TEXT,
    created_at TIMESTAMPTZ
)

stripe_subscriptions (
    account_id TEXT PRIMARY KEY,
    stripe_subscription_id TEXT UNIQUE NOT NULL,
    plan_id TEXT NOT NULL,
    status TEXT NOT NULL,
    current_period_start TIMESTAMPTZ NOT NULL,
    current_period_end TIMESTAMPTZ NOT NULL,
    cancel_at_period_end BOOLEAN NOT NULL DEFAULT false,
    updated_at TIMESTAMPTZ NOT NULL
)
```

- "Manage billing" → Stripe Customer Portal (Stripe owns the UI for invoices,
  payment methods, plan changes).
- Webhook at `app.atomic.cloud/billing/webhook` (single URL, not
  per-subdomain). Verifies Stripe signature, updates rows.
- Key events: `customer.subscription.{created,updated,deleted}`,
  `invoice.payment_{succeeded,failed}`.

**Plan transitions:**

| Trigger | Effect |
|---|---|
| Checkout success | `accounts.plan_id` updated, quotas widen |
| Downgrade | Plan updated. Over-limit usage retained but writes blocked until under. No auto-deletion. |
| Upgrade | Plan updated immediately, Stripe handles proration |
| Payment fail (Stripe dunning x3 over 1 week) | Status → `past_due` |
| 3 days past_due | Read-only mode |
| 14 days past_due | Suspended (login blocked), data retained |
| Subscription deleted | Drops to free plan; if over free limits, read-only until under |

**Never auto-delete data for payment failure.** Hard-delete only on explicit
user action. Right ethically and commercially (re-conversion is real revenue).

**Free tier (defaults, product-tunable):** 100 atoms, $0.50/mo AI credits
(managed key, cheap curated models — embedding and tagging a normal user's
notes costs pennies), 1 KB, 100 MB storage. All features available — no
feature-gated free tier. Chat gets a per-message output-token cap on free;
arbitrary generation is the free-inference abuse vector, embeddings are not.

**Trials:** 14 days of paid tier on signup, **no card required**. Auto-
downgrade to free after. Accepts signup-spam risk for friction-free
onboarding; magic-link + rate-limited signup bounds the abuse vector.

## Backups & disaster recovery **[drafted]**

Database-per-tenant makes per-tenant backup natural; hard-delete v1 makes it
mandatory. Without this section, a reaper bug, a bad migration, or a
fat-fingered delete confirmation is unrecoverable customer data loss.

### v1: nightly logical dumps

- Nightly `pg_dump -Fc` per tenant database **plus the control plane**,
  streamed to object storage (`backups/<date>/acct_<uuid>.dump`). Driven by
  the same reaper/job-runner machinery, with a concurrency cap.
- Retention: 14 daily + 8 weekly. Bucket lifecycle rules, not custom code.
- **Final dump on account deletion** — written to `backups/final/` with
  30-day retention before `DROP DATABASE` (step 4 of the deletion
  sequence). Hard delete stays the product behavior; this is the operator's
  undo, not a user feature.
- **Restore runbook** — write and *rehearse* before launch: restore dump
  into a fresh database → repoint `account_databases.db_name` → evict the
  AccountCache entry. Per-tenant restore never touches other tenants —
  that's the payoff of database-per-tenant.
- **Monitoring** — alert when any tenant's last successful backup is >36h
  old. An unmonitored backup job is a placebo.

### Deferred

- **PITR via WAL archiving** (wal-g / pgBackRest) — cluster-wide
  point-in-time recovery. Restore lands on a side cluster (all tenants at
  once), then per-tenant dump/restore from there. Add when nightly
  granularity stops being acceptable.
- Cross-region replicas, per-tenant continuous streaming — not v1.

## Implementation log

### Slice 1 — control plane, provisioning, CloudAuth, composed server (2026-06-10, branch `cloud-control-plane`)

The Fly-era prototype at `crates/atomic-cloud` was removed (its last commit,
`4b44c51`, is the parts bin for the magic-link/Mailgun/Stripe salvage).
Landed: control-plane database (`atomic_cloud_control`, hardened migration
runner mirroring atomic-core's advisory-lock pattern; migration 001 with the
slice-1 tables only), `provision_account`/`delete_account` as idempotent
library functions, token/session issuance (`atm_`/`ats_` prefixes, SHA-256
hashes), `AccountCache` (idle TTL + hard cap, WS-receiver eviction pinning,
coalesced loads, periodic sweep at `idle_ttl/4`), `CloudAuth` middleware with
both auth chokepoints enforced and e2e-pinned, and a composed `serve` binary
(atomic-server's `api_scope()` under CloudAuth) plus operator CLI
(`migrate`, `account create/delete`, `token create`).

**Deviations from this plan (deliberate, review-vetted):**

- `account_databases` got `PRIMARY KEY (account_id, db_name)` +
  `UNIQUE (cluster_id, db_name)`; account-owned tables carry
  `ON DELETE CASCADE` FKs as a safety net under hard-delete (the deletion
  sequence still deletes explicitly).
- Deletion **reserves the subdomain before deleting the accounts row**; this
  ordering is load-bearing — provisioning re-checks `subdomains_reserved`
  after its claim and rolls back if a reservation landed in the window.
- Stragglers/non-active accounts return a 503 variant `account_provisioning`
  (sibling of the planned `account_upgrading`, which arrives with deploy
  gating).
- `ResolvedTenant` carries the principal only; manager/event channel travel
  via atomic-server's `RequestDatabaseManager`/`RequestEventChannel`
  extensions (better than the plan's combined struct — one injection
  mechanism, not two).
- The Db extractor's AppState requirement is satisfied by an **inert
  `FallbackAppState`** plus a `cloud_plane_guard` that fails closed: requests
  lacking the tenant extension 404 before any handler runs (e2e-pinned).
- Per-tenant pools: 5 connections + 5-minute idle timeout by default
  (configurable), via a cloud-unaware `new_postgres_with_pool` generality
  constructor in atomic-core. `new_postgres` behavior is unchanged.
- Reserved-subdomain blocklist is ~80 names (plan said ~60; over-reserving
  is the safe direction).
- **No auth caching in v1, by choice**: subdomain→account and credential
  verification hit the control plane every request (plus a `last_used_at`
  write). This is what makes deletion/revocation immediate. Revisit with
  real load data; the plan's "looked up … and cached" applies only to the
  manager/event-channel resolution (AccountCache).

**Unrouted planes (fail-closed 404 under cloud, pending a per-tenant
story):** `/api/auth/*` (token CRUD is control-plane business),
`/api/exports/*` + `/api/databases/{id}/exports/*` (atomic-server's export
jobs bind process-global state — would be a cross-tenant job/artifact
namespace), `/api/logs` (process-global log buffer), OAuth/MCP (own slice).

**Deferred to later slices:** signup HTTP flow + magic links + email,
managed OpenRouter keys / `provider_credentials`, OAuth + MCP for cloud,
billing, quotas, dispatcher/worker pools, backups (including
final-dump-before-delete), deploy gating / fleet migration, the reaper,
`accounts.last_active_db_id` wiring, an HTTP deletion route (CLI deletion
today cannot evict the serve process's AccountCache — the stale entry is
harmless but lingers; the deletion route evicts in-process).

**Follow-ups for the reaper/signup slices:** a deletion that crashes between
reserving the subdomain and deleting the accounts row can be revived by a
resumed provision, leaving an active account whose own subdomain holds a
90-day reservation (benign; reaper should clear self-reservations); orphaned
`acct_*` databases from failed 23503 cleanup are logged loudly for the
reaper to reclaim. *(Both closed by slice 2.)*

### Slice 2 — app-host plane, magic-link signup/login, HTTP deletion, reaper (2026-06-10, branch `cloud-signup`)

Landed: host-based plane split (the bare apex **and** `app.<base>` serve the
account plane; tenant subdomains and the app plane 404 each other's routes,
e2e-pinned both directions); `magic_links` (migration 002, hash-only `aml_`
tokens, 15-min TTL — our choice, not plan-specified); `EmailSender` trait
with Mailgun (salvaged from the parts bin), log-mode, and a capturing test
impl; request-link + completion routes for signup and login; synchronous
provisioning behind a semaphore (default 4, `try_acquire` so saturation
503s without consuming the token; shape-check → read-only peek → permit →
atomic consume, so junk can't starve permits); session cookie
`Domain=.<base>; Secure; HttpOnly; SameSite=Lax`, 302 to the new subdomain;
`DELETE /api/account` (account-scope credentials only, confirmation body,
unconditional cache evict + WS severing, cancellation-proof via a detached
task); a five-arm reaper (stuck-provision resume-or-rollback, interrupted-
deletion recovery, orphan reclaim, self-reservation cleanup, link/session/
reservation expiry hygiene) under per-account advisory locks with an
observable per-pass summary.

**Deviations and choices (review-vetted):**

- Rate limiting is a hand-rolled sliding log, not `governor`: exact window
  semantics and a directly computable `Retry-After`, no new dependency.
  Implemented rows: request-link 5/IP/hour (one bucket shared across signup
  and login, charged before validation) and 3/email/hour (shared across
  both purposes, lowercased email). The other table rows (API req/min, atom
  creates, URL ingestion) belong to the quotas slice.
- Subdomain availability failures are honest 400s (subdomains are public
  via DNS); only email-axis outcomes get the neutral 200. The login path's
  issue+send is **fire-and-forget** so the exists/not-exists branches are
  timing-uniform — byte-identical bodies alone were a timing oracle.
- Failed-provision rollback **hard-deletes** the accounts row (loudly
  logged) instead of a `status='failed'` tombstone — consistent with v1
  hard-delete philosophy and avoids a non-additive UNIQUE-constraint
  migration.
- Deletion order remains revoke-credentials-first, delete-then-evict
  (inverting plan steps 5/7); the response can't claim "retry with the same
  credential," so recovery is automatic: an active account with no
  `account_databases` row is *provably* an interrupted deletion (the
  mapping INSERT precedes activation, and only deletion/CASCADE removes
  it), and the reaper's recovery arm completes it after a grace period.
- `provision_account` treats a zero-row activation UPDATE as the typed
  not-provisioning error (fourth race guard — a concurrent rollback can no
  longer yield a false-positive success).
- A stuck `'provisioning'` claim is exempt from `subdomain_taken` for the
  same (lowercased) email, so user-driven resume via a fresh link works.
- CSRF posture of the cookie-authenticated DELETE: `SameSite=Lax` blocks
  cross-site non-navigation requests and the confirmation body is required;
  no CSRF token in v1. `Referrer-Policy: no-referrer` on the account plane
  (completion URLs carry live tokens).

**Deferred:** remaining rate-limit rows (quotas slice), failed-migrations
reaper arm (deploy gating slice), folding the reaper into the shared job
runner, email plus-addressing canonicalization (accepted residue: variants
get separate rate-limit buckets), HTTP token CRUD for cloud, signup/login
frontend pages (parts-bin salvage later — API + redirects only today).

### Slice 3 — provider management (2026-06-11, branch `cloud-provider`)

Landed: the sanctioned atomic-core plumbing (explicit `Option<ProviderConfig>`
injection with byte-identical `None` fallback, live `update_provider_config`,
redacting `Debug`, `openrouter_base_url` override — implemented as a
settings-map overlay at one chokepoint, `settings_for_ai`, since providers
are rebuilt per-operation rather than cached); migration 004
(`provider_credentials` + the active-provider pointer) and 005
(`accounts.provider_generation`); `KeyVault` with AES-256-GCM
`EnvMasterKeyVault` (length-prefixed AAD over account/provider/**origin** —
stronger than the plan's plain concatenation); the OpenRouter provisioning
client behind a trait (API shape verified against openrouter.ai docs);
managed-key minting at signup step 9 with race-safe conditional insert and
external-key cleanup on every rollback/deletion path; BYOK save with
validate-before-store, per-account write locks, model curation
(pinned embedding model + curated LLM list), and live rotation.

**Deviations and choices (review-vetted):**

- **Explicit mode pins per-task models**: `apply_to_settings` pins
  `wiki_model`/`chat_model` to the config's `llm_model`, and the re-embed
  machinery sources both settings maps from `settings_for_ai` — so tenant
  settings writes are inert for model routing AND for embedding-space
  changes (no platform-billed frontier inference, no destructive vector
  index recreation). Consequence: one LLM selection governs all tasks; BYOK
  per-task models deferred until someone asks.
- **BYOK dimension changes are rejected**, not warned (plan said "warn
  loudly"): the warning was unfulfillable — the tenant vector column is
  pinned at `PINNED_EMBEDDING_DIMENSION` (1536) and no cloud mechanism can
  recreate it. Structured `embedding_dimension_unsupported` 400. Revisit
  with a dimension-migration story.
- **Rotation convergence via `provider_generation`**: every provider
  mutation bumps it; CloudAuth's existing per-request account lookup
  observes it and lagging cache entries refresh in place — bounded
  staleness across rebuild races and pods (the no-auth-caching decision
  paying off). In-place swap remains the fast path.
- `accounts.active_provider` became two columns `(active_provider,
  active_origin)` with a paired-NULL CHECK; flip is still one UPDATE.
- Deletion step 3 and all rollback paths delete external keys best-effort
  with loud logging — deletion never wedges on a provider outage; the
  master-account key listing is the ops fallback. The create-key→row-insert
  orphan window is accepted and documented.
- Reaper stuck-provision rollback is outage-classified: provisioning-API
  failures defer rollback (summary-visible) up to a 60-min ceiling.
- Managed `model_config` writes merge over platform-owned keys (base-URL
  overrides survive); BYOK writes are vocabulary-checked (a misplaced
  `api_key` inside `model_config` is rejected, never stored plaintext).
- BYOK validation errors are bounded (500 chars) and key-scrubbed before
  truncation on both provider arms.
- `last_used_at` stamps at cache-entry build, not per provider call.
- CLI stays keyless (never holds master/provisioning keys): `account
  create` provisions without a managed key; HTTP routes are the real path.
  Provisioning mode `disabled` is the dev default.

**Deferred:** plan-change → PATCH credit limit (billing slice);
`blocked_on_credits`/`blocked_on_provider` ledger holds + circuit breaker +
rotation step 6 (dispatcher slice); managed-key re-provisioning/rotation
route; synthesized health status + recent-errors list (needs breaker +
`account_events`); advisory usage counter fallback (needs `quota_usage`);
frontier-model feature flags (needs `plans`); master-key lazy re-encrypt
(nothing to rotate to at generation 1); periodic BYOK re-validation (open
question, unchanged); signup steps 7-8 (still deferred from slice 2).

**Accepted residue (verifier-noted):** the cross-pod rotation test asserts
the refreshed config rather than driving a second-pod embed (the in-process
twin does drive one); a rotation landing inside `set_setting_with_reembed`'s
two config reads can queue one spurious same-dimension re-embed (cost
noise, not correctness; dimension mismatch impossible under the pin).

### Slice 4 — dispatcher, worker pools, backpressure (2026-06-12, branch `cloud-dispatcher`)

Landed: migration 006 (`dispatch_hints` with clear-if-older mid-scan
survival) and 007 (`provider_paused_until` + streak); the per-pod
dispatcher (hint fast-path + slow-path full scan, N+1 poll over tenant
ledgers, one-job-per-pop round-robin, per-tenant poll timeout); four
bounded worker pools at the plan's caps with per-work-type overrides
(reports cap 1 counts report in-flight, not the whole llm class); provider
backpressure — 429 Retry-After into ledger horizons (clamped, 15-min cap),
the per-tenant circuit breaker (3 provider-classified failures in 60s,
doubling cooldown, only provider-touching work participates), 402 →
credits pause with jobs sitting in ledger and the structured
`out_of_ai_credits` error on interactive routes, 401/403 → provider-kind
pause; provider mutations clear pauses AND re-arm backed-off rows in both
ledgers; per-tenant chat-stream semaphore (cap 3, permit tied to the
response stream's lifetime).

**Key design facts (study findings worth keeping):**

- atomic-core itself spawns pipeline work on the request path (every save
  path claims + spawns in-process). The cloud-unaware `inline_pipeline`
  knob gates **before the claim** — gating after would lease-lock jobs
  away from the dispatcher for 30 minutes. Default true; self-hosted
  byte-identical.
- Hints are marked AFTER the handler completes (ledger-write-then-hint
  makes the conditional clear sound) by a coarse mutating-method
  middleware — a false-positive hint costs one empty poll. The slow-path
  scan (5 min) bounds hint loss.
- `RunHandle::defer_until` in the scheduler ledger: environmental
  (provider-classified) failures release the lease, set `next_attempt_at`,
  and **refund the attempt** — the lease-reclaim precedent extended to
  classifiable failures. No policy installed → `fail()` unchanged.
  Cloud installs the classification policy; self-hosted untouched.

**Deviations and choices (review-vetted):**

- The breaker detection window is per-pod in-memory (the pause itself is
  control-plane, honored by all pods); "3 consecutive" is implemented as
  3-in-60s among provider-touching outcomes only.
- Rate-limit pauses gate background dispatch only; credits pauses also gate
  interactive AI routes (atom CRUD always works). Interactive routes do
  not trip the breaker themselves (asymmetry documented in code).
- Feeds on idle tenants ride the slow scan (up to ~5 min latency vs 60s
  self-hosted) — acceptable for v1, noted.

**Known limitation (deliberate, v1):** dispatcher worker events publish to
the executing pod's in-process channel — in a multi-pod deployment, WS
subscribers on another pod miss that execution's progress events. Durable
state is always correct and the frontend self-heals on fetch. Cross-pod
event relay (e.g. Postgres LISTEN/NOTIFY) is the planned follow-up; do it
before running >1 pod in production.

**Deferred:** plan-tier weighted fairness (uniform v1), outbox pattern,
leader election, `upgrade_url` content in the credits error (billing
slice), the failed-migrations reaper arm (deploy gating).

### Slice 5 — deploy gating & fleet migration (2026-06-12, branch `cloud-deploy-gating`)

Landed: migration 008 (per-tenant migration tracking on `account_databases`,
backfill stamping active rows at the frozen authoring-time target — provably
at-or-below, the safe direction) and 009 (`deploy_runs` per-boot history);
`PostgresStorage::target_schema_version()` (the one core change — a const
accessor); the boot fleet runner (enumerate → fan out, cap 16 → per-tenant
`initialize()` under the existing advisory lock, which IS the multi-pod
story); `/ready` gated by the plan's failure-rate policy table verbatim;
the `account_upgrading` 503 (body verbatim, WS included) on CloudAuth's
per-request lookup; `deploy status` / `deploy advance` CLI (no override for
`rollback_required`, deliberately); the failed-migrations reaper arm; and
the additive-only migration lint as a test scanning both migration dirs
with fixture probes (forbids DROP/ALTER TYPE/RENAME, SET NOT NULL, and
validated-at-add constraints; one justified frozen-history exemption:
core 020's REAL→f64 widening, which predates the additive-only decision).

**Design correction (review-driven, the slice's big lesson):** the original
recovery split — "boot runner owns unattempted lagging tenants, reaper owns
recorded failures" — was wrong, because the boot runner enumerates once per
pod lifetime. Old-pod signups mid-rolling-deploy, failed record writes, and
panicked migration tasks all produced lagging tenants nobody owned, 503ing
forever. The reaper's arm now owns **all** lagging rows (lagging-ness, not
failure state, drives retry); per-tenant advisory locks make racing the
boot runner safe. This also defuses the mixed-fleet race where an old
binary's no-op success-record erases a new-target failure record. Both
regression-pinned, including an e2e proving a late-stamped straggler heals
without any pod reboot.

**Other review-vetted hardening:** `start_deploy_run`/`finish_deploy_run`
retry within the wall-clock budget (a transient control-plane blip can no
longer wedge a pod not-ready with green liveness); stale `migrating`
deploy_runs rows finalize as `abandoned` and cannot shadow `deploy
advance`; timed-out fleet runs persist partial counts; monotone
GREATEST stamping everywhere (an old binary can never regress a newer
stamp).

**Notes:** readiness gates the load balancer, not the process — the data
plane serves per-request during `awaiting_review` (e2e-pinned). Small
fleets hit `rollback_required` easily (1 broken of 2 = 50%); that is the
policy working as designed — the operator path is fix/delete the tenant
and redeploy.

### Slice 6 — plans, quotas, Stripe billing & dunning (2026-06-13, branch `cloud-billing`)

Landed: migration 010 (additive) — the `plans` (seeded `free` + placeholder
`pro`) and `quota_usage` tables, `accounts.plan_id` (FK with `ON DELETE
RESTRICT`, backfilled alongside the retained legacy bare `plan` column),
`accounts.billing_state`/`past_due_since`, and the `stripe_customers` /
`stripe_subscriptions` / `plan_transitions` tables. An in-memory
[`PlanRegistry`](../../crates/atomic-cloud/src/plans.rs) (eager-loaded at
boot; `provision_account` stamps `plan_id='free'`). The plan-tier resource
guard ([`quota.rs`](../../crates/atomic-cloud/src/quota.rs)) on `api_scope`:
`POST /api/atoms`, `/api/atoms/bulk` (batch-delta accounted by a buffer-and-
replay body peek), and `POST /api/databases` → the plan's exact
`quota_exceeded` 402 shape (`resets_at: null`, derived `upgrade_url`); NULL
limit = unlimited never blocks; atom/KB counts read **live** from the tenant
DB (cheap, strongly consistent), `quota_usage` reserved for storage/rollups,
the AI-credits counter advisory only (OpenRouter enforces it). The three
remaining anti-abuse rate-limit rows (API 600/min, atom-creates 60/min,
URL-ingestion 30/min) as per-account sliding windows + a data-plane guard
(429 + `Retry-After`, per-pod approximate). The full Stripe integration:
`BillingProvider` trait + `StripeClient` (salvaged from `4b44c51`, adapted to
`CloudError` + constant-time webhook verify), the customer-portal/checkout
redirects (tenant plane) and the signed webhook (app host, single URL),
subscription/payment event projection. The dunning state machine
([`billing/dunning.rs`](../../crates/atomic-cloud/src/billing/dunning.rs)):
`past_due → read_only` (3d) `→ suspended` (14d), data always retained —
suspended gated in `CloudAuth`, read_only by a write-guard, advanced by an
injectable-clock `advance_dunning` sweep (hourly loop in `serve`). **Trials**
(migration 012, `accounts.trial_ends_at` + the `trialing` `billing_state`):
signup completion grants the 14-day paid tier with no card (`start_trial`,
first-time-only/idempotent), `CloudAuth` serves a trial as full access, and
the same hourly sweep auto-downgrades expired trials to `free` —
`read_only` when the now-free account is over the free limits (over-limit
data retained, never deleted), `active` otherwise (`advance_expired_trials`,
the over-limit decision read live from the tenant DB via the account cache).

**Period rollover & storage enforcement** ([`quota_usage.rs`](../../crates/atomic-cloud/src/quota_usage.rs),
migration 013): an hourly `serve` loop rolls `quota_usage` period rows
forward (idempotent, cross-pod safe) — AI allowances reset natively at
OpenRouter so they need no rollover — and recomputes per-tenant storage
bytes into a warn→restrict ladder (`storage_state`), restricting writes via
the same path as `read_only`, never deleting data.

**Review-driven hardening (the atom-ceiling was the slice's sharpest gap):**
The quota guard is an HTTP middleware, so it only sees request-path atom
creation. The adversarial review found three evasions of the account-wide
atom ceiling it therefore missed, all now closed: (1) **background** atom
creation — feed polls and report-finding writes go through the dispatcher,
which now defers atom-creating work for an at-ceiling tenant (the
"blocks, sits in the ledger" pattern, sharing the data plane's
`PlanRegistry`); (2) the request-time gate counted only the targeted KB
while the limit is **account-wide** — it now sums across all the tenant's
KBs, matching the downgrade sweep; (3) the **manual-trigger** routes
(`POST /api/import/obsidian`, `/api/reports/{id}/run`, `/api/feeds/{id}/poll`)
were ungated aliases of the atom-create surface — now in `quota_target`.
Also: the Stripe webhook's claim-and-apply run in **one transaction**, so a
crash between claiming an event id and applying it rolls the claim back
(Stripe's redelivery reprocesses rather than being dedup'd into a no-op);
the signature is still verified before the transaction opens.

**Deviations from this plan (deliberate):**

- **`accounts.billing_state` is a new column, orthogonal to `status`.** The
  plan describes dunning as moving "Status → past_due", but `accounts.status`
  is CloudAuth's provisioning/active gate; overloading it would conflate
  "being set up" with "behind on payment". A delinquent account stays
  `status='active'` with a separate `billing_state` (active/past_due/
  read_only/suspended) — cleaner serving logic, and the audit trail
  (`plan_transitions`) records every move.
- **Billing is optional.** No `--stripe-secret-key` ⇒ the billing routes 503
  and the dunning sweep finds nothing to advance, so dev clusters and
  self-hosted-style deployments run unchanged. The plan assumes Stripe is
  always present; making it optional cost nothing and unblocks local testing.
- **Webhook verify uses constant-time MAC comparison** (`Mac::verify_slice`),
  not the parts-bin's `==` on hex strings — a timing oracle on the MAC would
  let a forger reconstruct a valid signature byte by byte.
- **Resource counts are read live, not from `quota_usage`.** The plan's
  schema implies a counter; for atoms/KBs a live `count_atoms()` /
  `list_databases()` is cheaper than maintaining a counter and can't drift.
  `quota_usage` stays for the metrics that genuinely need it (storage,
  rollups) and the advisory AI counter.

**Deferred:** observability metrics/tracing; the user-facing `account_events`
log (rides with the frontend slice); plan-change → PATCH managed-key credit
limit (the managed-keys seam exists); the signup/billing **frontend** (API +
redirects only); paid-tier pricing (the `pro` numbers are documented
placeholders).

### Slice 7 — cloud OAuth flow & per-tenant MCP (2026-06-13, branch `cloud-oauth`)

Landed: cloud's **own** per-account OAuth 2.0 flow
([`oauth_routes.rs`](../../crates/atomic-cloud/src/oauth_routes.rs)) on the
tenant subdomain — Dynamic Client Registration + Authorization Code + PKCE
(S256-only, `client_secret_post`) — backed by the slice-prepped control-plane
store ([`oauth_store.rs`](../../crates/atomic-cloud/src/oauth_store.rs),
migration 014, every row `account_id`-scoped). The discovery / register /
token endpoints are **public** (an MCP client bootstraps before any token
exists), each resolving the account from `Host` itself with CloudAuth's exact
subdomain rules; the authorize-**approve** step authenticates the **session
cookie** (the logged-in user on their own subdomain), not a pasted token —
the structural difference from atomic-server's self-hosted flow, which is left
**untouched** (it stores in atomic-core's registry, which cloud's Postgres
mode lacks). A logged-out authorize bounces to `app.<base>/login?return_to=…`.
The minted token lands in `cloud_tokens` (`scope='mcp'`) and CloudAuth then
accepts it on `/api/*` and `/mcp` (full flow e2e-pinned).

Per-tenant **MCP**: atomic-server's `mcp_scope` now mounts under CloudAuth +
`cloud_plane_guard` ([`server.rs`](../../crates/atomic-cloud/src/server.rs)),
so the tenant's injected `RequestDatabaseManager` drives tool resolution.
Under cloud, **CloudAuth is the MCP auth layer** — self-hosted's `McpAuth` is
not in the cloud composition — so CloudAuth also produces the MCP-compliant
401: an unauthenticated `/mcp` request gets a `WWW-Authenticate: Bearer
resource_metadata="{scheme}://{Host}/.well-known/oauth-protected-resource"`
header pointing at the **tenant's own** discovery document (the same origin
`oauth_routes` serves it from), so Claude Desktop discovers this account's
OAuth flow. The decoration fires only for a 401 on the `/mcp` path
([`auth.rs`](../../crates/atomic-cloud/src/auth.rs) `decorate_mcp_unauthorized`,
fed the public scheme via `CloudAuth::with_public_scheme`); `/api/*` 401s stay
plain. The MCP path is governed by the **same** `allowed_db_id` chokepoint as
the data plane — a db-pinned MCP token can't select another KB on `/mcp` via
`X-Atomic-Database` (e2e-pinned, alongside cross-tenant rejection: account A's
MCP token on B's `/mcp` → 401).

**The one atomic-server change (cloud-unaware, self-hosted byte-identical):**
the MCP transport now resolves its `DatabaseManager` **per request** — a new
`RequestManager` rmcp extension copied from the actix `RequestDatabaseManager`
when a composing layer installed one, with the baked-in manager as fallback —
mirroring the data plane's `db_extractor::request_manager`. With no such
middleware (the standalone server) the baked-in manager is used exactly as
before; both sides pinned by new unit tests in `mcp/transport.rs`. Nothing
else in atomic-server changed; `routes/oauth.rs` was not touched.

**MCP-token-default-scope decision (resolves the open question):** consent
mints **account-scope** tokens (`allowed_db_id = NULL`) — one MCP URL per
account, one account = one user in v1. The db-pinned path still works: a code
carrying an `allowed_db_id` mints an `mcp`-scoped token pinned to that KB, and
the slice-1 `allowed_db_id` chokepoint enforces it (e2e-pinned: a pinned token
can't reach another KB via `X-Atomic-Database`).

**Review-driven hardening (security-sensitive, post-e2e):** the adversarial
review found two real holes, both closed: (1) the server-rendered OAuth
**consent page** had no anti-framing header — an attacker who self-registers a
client could iframe a victim tenant's `/oauth/authorize` and clickjack consent
into minting a token (the `SameSite=Lax` cookie doesn't defend a same-origin
submit inside a cross-origin frame). Every OAuth HTML response now carries
`X-Frame-Options: DENY` + `Content-Security-Policy: frame-ancestors 'none'`
via a single hardened-response helper. (2) The db-pin chokepoint was enforced
only for an *explicit* different-KB selection — on the **default no-selection**
path the MCP transport read only `?db=` (never the `X-Atomic-Database` header
CloudAuth injects for a pinned request), so a KB-pinned MCP token fell through
to the tenant's *active* KB (a within-account isolation hole). The MCP
transport now resolves its target DB with the same `header → ?db= → active`
precedence as the `Db` extractor, so an injected pin is honored — pinned by a
positive two-KB test (token pinned to the non-active KB resolves to the pinned
one). Also: OAuth redirects are now RFC-correct for `redirect_uri`s carrying a
pre-existing query string, and the MCP `WWW-Authenticate` challenge covers the
trailing-slash path.

**Deviations from this plan (deliberate):**

- **OAuth routes are individual exact-path resources, not a `web::scope("")`.**
  An empty-prefix scope would prefix-match (and swallow) `/api/*`; exact
  resources keep the discovery/flow paths isolated.
- **Issuer/endpoint URLs are built from the request `Host` + a configured
  scheme**, not a second configured hostname — discovery points back at the
  exact origin the client already addressed, so the routing host and the
  advertised host can't drift apart.
- **The consent page authenticates via the session cookie only** (no pasted-
  token fallback, unlike self-hosted): cloud users are already logged in on
  their subdomain, and a pasted-token field would be a worse, phishable UX.

**Deferred:** the OAuth consent/login **SPA** (this slice is API + a minimal
server-rendered approve form only); per-KB-MCP-by-default (awaits multi-user-
per-account); wiring `purge_expired_oauth_codes` into the reaper loop (the
function exists; codes are single-use + short-TTL, so stale rows are inert).

### Slice 8 — backups & disaster recovery (2026-06-13, branch `cloud-backups`)

Landed the v1 backup story — mandatory before real user data exists, because
hard-delete v1 makes a reaper bug or a fat-fingered delete unrecoverable
without it. Everything is in `atomic-cloud`; `atomic-core`/`atomic-server` are
untouched (backups are a control-plane + subprocess + object-storage concern).

- **`BackupStore` seam** ([`backup_store.rs`](../../crates/atomic-cloud/src/backup_store.rs))
  — the `EmailSender`/`BillingProvider`/`ProvisioningApi` shape: a trait
  (`put`/`get`/`list`/`exists`), a `LocalFileSystemStore` (dev + **every**
  test; pure `tokio::fs`, path-traversal-guarded, never network), and an
  `S3Store` over the `object_store` crate (S3 + any S3-compatible endpoint;
  SigV4 not hand-rolled — one well-maintained dep). S3 credentials env-only.
- **Dump/restore runner** ([`backup.rs`](../../crates/atomic-cloud/src/backup.rs))
  — `pg_dump -Fc` / `pg_restore` via `tokio::process`. **Credential hygiene is
  load-bearing**: the password rides in `PGPASSWORD` in the child env, *never*
  argv (a unit test asserts a sentinel password appears only in the spawned
  command's env, never its args); connection params are discrete
  `--host/--port/--username/--dbname` flags. Every db name passes
  `is_tenant_db_name` before reaching a flag or DDL. stderr is bounded into a
  typed `CloudError::Backup`. A real dump → restore → verify roundtrip is
  integration-tested (write an atom, dump, restore into a fresh DB, assert the
  atom rehydrated) — `pg_dump`-probed, skips-with-message when absent.
- **Nightly pass + final dump + staleness**
  ([`backups.rs`](../../crates/atomic-cloud/src/backups.rs), migration 015) —
  the pass dumps every active tenant (each under the reaper's **same**
  per-account advisory lock, so a dump can never race a `DROP DATABASE`) plus
  the control plane, recording `account_databases.last_backup_at` /
  `last_backup_error` and a `backup_runs` ledger row; a jittered `serve` loop
  mirrors the reaper. The staleness monitor alerts (error-level) when any
  tenant's last successful backup is >36h old. The **final dump** hooks into
  `delete_account` *before* the drop, **fail-closed** (a dump failure aborts
  the deletion), scoped to the active-account path (HTTP route, CLI, reaper
  interrupted-deletion arm) — the never-activated rollback/orphan paths drop
  no-data databases and take none. Retention is bucket lifecycle, not code.
- **Restore CLI + runbook** — `atomic-cloud backup restore` restores into a
  *fresh* DB and prints the remaining manual steps; repointing
  `account_databases.db_name` and evicting a running pod's `AccountCache` are
  deliberate human runbook steps (the CLI can't reach another process's cache;
  an admin evict endpoint is a later slice — the slice-2 deletion-gap shape).

**Deviations from this plan (deliberate):** `delete_account` takes an explicit
`BackupPolicy` (`Required(store)` | `DisabledAcknowledged`) — *not* an
`Option` — so a composition that forgets to wire a store is a **type error**,
not a silent fail-open drop; dev (backups off) is the loud `DisabledAcknowledged`
path. The reaper and backup pass share `reaper_lock_key`, making backup/drop
mutually exclusive per account.

**Review-driven hardening (the slice whose whole purpose is preventing
unrecoverable loss got the closest read):**
- **Subprocess timeouts.** `pg_dump`/`pg_restore` now `spawn` under a
  `tokio::time::timeout` + `child.kill()` budget (`--backup-timeout-secs`,
  default 30m). A hung dump previously held the advisory lock and blocked the
  serial loop, so the pod took *no further nightly backups until restarted*;
  now a timed-out tenant is recorded failed and the pass continues.
- **The lock invariant made true.** The "a dump can never race a `DROP`"
  guarantee held only for the *reaper's* drop — the HTTP/CLI delete path took
  no lock. `delete_account` now acquires the same per-account advisory lock
  (`Acquire` mode → 503 `deletion_busy` if a backup holds it); the reaper's
  arm, which already holds the lock, passes `AlreadyHeld` to avoid a
  self-deadlock.
- **Fairness + observability.** Stale-first ordering now keys on
  `COALESCE(last_backup_at, last_backup_attempt_at)` so a persistently-failing
  tenant can't starve healthy-but-due ones; a `finalize_abandoned_backup_runs`
  sweep closes `backup_runs` rows orphaned by a mid-pass crash; `--backup-prefix`
  supports shared buckets.
- **Staleness uses the DB clock.** `stale_tenant_backups` computes its cutoff
  from `NOW()` in SQL rather than a caller timestamp — skew-immune across pods
  (also removed a clock-skew test flake at the source).

**Deferred (per plan):** PITR via WAL archiving, cross-region replicas, the
admin cache-evict endpoint, observability metrics — all later slices.

### Slice 9 — account-plane frontend (2026-06-14, branch `cloud-frontend`)

The cloud "front door" — a polished React SPA at
[`crates/atomic-cloud/frontend/`](../../crates/atomic-cloud/frontend) (Vite +
React 18 + TypeScript + Tailwind v4), styled to match the **marketing website**
(`atomic-website`): warm light-paper palette, Crimson Pro serif display + DM
Sans body (self-hosted via `@fontsource`, no runtime CDN), one purple accent,
the node-graph motif. The dark product app (`src/`) is a separate surface and
was **not** touched; atomic-core/atomic-server are untouched (all Rust changes
are in atomic-cloud: one read endpoint + the static-serving wiring).

- **One build, two host contexts** (switched by `Host`, read from a
  server-injected `<meta>` base-domain tag): the **app host** (bare base +
  `app.<base>`) serves the public pages — a focused landing, `/signup` (live
  `<slug>.<base>` preview, the real validation/`subdomain_taken`/429 handling),
  `/login` (account-existence-neutral, mirroring the backend); the **tenant
  subdomain** serves the authenticated dashboard at `/account/*`, same-origin
  and **session-cookie authed** (no bearer-token juggling), with a 401 →
  app-host-login bounce.
- **Dashboard**: Overview (plan/usage/KB count with `billing_state` banners —
  trialing/past_due/read_only/suspended/out-of-credits), Provider/BYOK settings
  (mirrors the product app's `AIProviderStep` against the cloud provider routes;
  managed status + usage, switch to BYOK with server-side validation surfaced
  verbatim, model selection within the curated list, the existing-key-never-shown
  rule), Billing (status + "Manage billing"/upgrade → Stripe via a fetch→redirect
  helper, in-app error on misconfig — **Stripe owns checkout**, no Elements), MCP
  setup (the `<origin>/mcp` URL + Claude-Desktop connect instructions), and a
  typed-subdomain-confirmation delete flow.
- **Rust (atomic-cloud only)**: a new `GET /api/account/overview` (account-scope,
  CloudAuth) assembling plan/billing/usage/provider summary from the control
  plane + the tenant manager (`count_atoms`, `list_databases`) — **never** key
  material; db/MCP-scoped tokens 403. The SPA is served by actix-files as the
  app's `default_service`, registered **last** so it never shadows a
  JSON/OAuth/MCP/WS route; `/account/*` is server-side auth-gated (unauth → 302
  to app login, API 401 stays JSON). `dist`/`node_modules` are gitignored, never
  committed; `dist` is produced by `npm run build`.

**Quality bar**: the design-fidelity review found the website match **exact, no
off-brand drift**; real loading/error/empty/disabled states throughout,
accessible, responsive; 40 vitest tests + Rust serving/overview integration
tests green. A first review fix closed a stuck-submit bug on the BYOK form, a
segmented-control focus-ring gap, and raw-JSON-on-checkout-misconfig.

**Note on polish**: this is a complete, faithfully-themed, fully-wired first
implementation; final *visual* polish is an iterative pass with a human running
it (automated review can't fully judge "looks great").

**Deferred**: the `account_events` user-facing activity log can ride a future
pass; per-page deep-linking polish and any marketing-grade landing content live
on the separate `atomic-website`.

## Open questions (carried across sections)

- **Free tier shape & abuse model.** Open free signup needs CAPTCHA +
  rate-limited token issuance — and with managed keys, free signups are
  platform-funded inference, so per-account allowances cap the blast radius
  but mass signup is the residual vector. Invite-only beta sidesteps all of
  this *and* defers the entire billing build; strongly consider it as the
  launch shape.
- **Email deliverability.** Magic-link-only auth makes it critical-path
  (decided 2026-05-25). Mailgun client exists in the prototype crate;
  domain warmup, SPF/DKIM, and a bounce strategy still need an owner.
- **MCP token default scope.** ~~Account-wide vs per-KB.~~ **Resolved (slice
  7): account scope.** Tokens minted by the cloud OAuth consent flow default
  to `account` scope (full access to the account's KBs, `allowed_db_id =
  NULL`), matching "one MCP URL per account" and "one account = one user" in
  v1. The db-pinned path still works end to end — an authorization carrying an
  `allowed_db_id` mints an `mcp`-scoped token pinned to that KB, and CloudAuth's
  `allowed_db_id` chokepoint enforces the pin (a db-pinned MCP token can't read
  another KB via header override; e2e-pinned). Per-KB-MCP-*by-default* is
  deferred until the multi-user-per-account story exists.
- **Email uniqueness.** `accounts.email` is not unique and signup never
  checks it — one email can own unlimited accounts (each rate-limited, each
  its own subdomain). Probably fine (it's also the multi-account story),
  but decide deliberately before billing ties subscriptions to accounts.
- **Event-stream scope for database-scoped tokens.** The cloud `/ws` route
  streams the whole account's event channel to any authenticated credential;
  the `allowed_db_id` chokepoint governs only the data plane, so a KB-pinned
  token sees other KBs' pipeline events (atom ids, etc.). Within-account
  exposure only, so fine while one account = one user — but decide before
  the MCP/OAuth slice mints db-scoped tokens for third-party clients.
- **AccountCache idle-TTL and hard-cap numbers.** Tune from real load; initial
  guess 10–30 min TTL, cap at 1000 entries.
- **Periodic BYOK re-validation.** Reaper-driven daily check
  (capped, skipped for active keys) catches quietly-expired keys before
  users hit them through failed work. Costs one test call per active key per
  day against the user's quota; adds reaper complexity. Decide once we see
  how often keys quietly expire in practice. (Managed keys don't quietly
  expire — their failure mode is allowance exhaustion, already handled.)
- **Master OpenRouter account ops.** Auto-top-up threshold, balance
  alerting, and what happens to tenants in the minutes after the balance
  hits zero (presumably 402s → circuit breakers → recovery on top-up; verify).
- **Included-credit sizing per tier.** Free placeholder is $0.50/mo; paid
  tiers need allowance numbers that keep "allowance < price" with margin.
- **Per-tenant metric cardinality strategy.** Top-N high-cardinality buckets
  + aggregate vs buy a high-cardinality TSDB (Mimir, VictoriaMetrics).
  Deferable until we have noisy tenants.
- **Plan tier structure beyond free.** Number of paid tiers, what features
  differ, pricing. Product/business call.
- **Free-tier limits (numbers).** Placeholder is 100 atoms / $0.50 AI
  credits / 1 KB / 100 MB.
- **Storage quota unit — bytes vs atoms.** Bytes is more accurate for cost,
  atoms is easier to communicate. Likely bytes for enforcement, atoms for
  marketing copy.
- **Trial length.** 14 days conventional; 7 or 30 also defensible.
- **Read-only / suspended UX details.** What the user sees, friendly upgrade
  prompts, "your data is safe" messaging.
- **account_events retention policy.** 90 days default? Per-event-type
  retention?
- **Tracing sample rate.** 1–5% baseline; tail sampling for errors.
- **Backup numbers.** 14 daily + 8 weekly is a placeholder; revisit
  alongside the PITR decision once tenant count and database sizes are real.

## Decisions log

Capture choices we've already made so we don't relitigate. Date each entry
and link the discussion if it lives in a memory file.

- **2026-05-25** — Cloud uses Postgres, not SQLite. Database-per-tenant on
  shared cluster. Supersedes the earlier SQLite + per-customer mounted
  volume plan.
- **2026-05-25** — Hard isolation directive: cloud code lives in
  `crates/atomic-cloud/`, no cloud-aware code in atomic-core or
  atomic-server.
- **2026-05-25** — Per-account subdomains as primary tenant routing input.
  Vanity slugs at signup, public enumeration via DNS accepted.
- **2026-05-25** — All tokens in control plane (option A). Token format is
  opaque `atm_<random>`; subdomain provides account context.
- **2026-05-25** — Server-stored web sessions, cookie scoped to
  `.atomic.cloud`.
- **2026-05-25** — Separate OAuth implementation in atomic-cloud; do not
  extend atomic-server's OAuth handlers with pluggable storage.
- **2026-05-25** — Provider config via explicit `Option<ProviderConfig>`
  parameter on `AtomicCore::open*` (option a). No traits; atomic-core gains
  no cloud awareness.
- **2026-05-25** — Three atomic-server generality refactors approved: route
  registration as library function, request-extension-based core resolution,
  request-extension-based event channel.
- **2026-05-25** — "Active database" concept moves to per-account state
  (`accounts.last_active_db_id`) rather than being killed.
- **2026-05-25** — `account_databases` carries `cluster_id` from day one so
  future shard split is mechanical.
- **2026-05-25** — Signup is synchronous (inline with HTTP request),
  capped at 4–8 concurrent provisions per process. Safety-net reaper for
  stuck rows.
- **2026-05-25** — Hard delete for v1. No grace period, no soft-delete.
  Freed subdomains reserved for 90 days before reuse.
- **2026-05-25** — Tenant database naming: `acct_<base32(uuid)>`. Opaque,
  fixed-length, doesn't leak tenant count.
- **2026-05-25** — Custom-domain support (BYOD) is the next subdomain-adjacent
  feature after v1 ships. Subdomain renaming is deferred indefinitely; we go
  straight from "subdomain only" to "subdomain + custom domain."
- **2026-05-25** — Seed defaults: default KB (`db_id='default'`), per-DB
  default settings, default Report (per reports plan). No starter atoms;
  empty-state UI instead.
- **2026-05-25** — ~~BYOK for provider keys in v1.~~ **Superseded
  2026-06-09**: managed keys by default, BYOK as opt-in (see below).
- **2026-05-25** — Authentication is **magic-link only**. No password
  infrastructure. Email verification falls out of signup naturally — clicking
  the link proves email ownership.
- **2026-05-25** — Deploy gating runs inside the new binary's boot sequence:
  fleet migration completes before readiness flips ready. Thresholds:
  <1% failures = proceed; 1–10% = await review; ≥10% = rollback required;
  >30min wall time = timeout. Stragglers get a 503 `account_upgrading`
  response; reaper retries them.
- **2026-05-25** — Migrations are **additive-only**. ADD COLUMN, CREATE
  TABLE, CREATE INDEX, deferred constraints. No DROP COLUMN, no ALTER COLUMN
  TYPE, no rename. Drops happen N+1 deploys after all referring code is gone.
  Enforced by a CI lint on migration SQL.
- **2026-05-25** — Central dispatcher in atomic-cloud, per-pod, no leader
  election. `FOR UPDATE SKIP LOCKED` on ledger claims. Round-robin per-tenant
  fairness (uniform weights v1; plan-tier weighting deferred).
- **2026-05-25** — Four worker pools per work class (embedding / llm /
  ingestion / maintenance) with total and per-tenant in-flight caps.
  Streaming chat is not in a pool — per-tenant semaphore at the route handler.
- **2026-05-25** — Cross-tenant ledger scan via N+1 polling + control-plane
  `dispatch_hints` bit. Outbox pattern is deferred until N+1 hurts.
- **2026-05-25** — Per-tenant provider circuit breaker (3×429 in 60s → 60s
  cool-down, doubling). State in `accounts.provider_paused_until`.
- **2026-05-25** — Unify feed polling, DraftPipelineTask, GraphMaintenanceTask,
  and wiki regen onto `task_runs`. Cloud-driven but **not cloud-specific** —
  the refactor lives in atomic-core/atomic-server and benefits self-hosted.
  Planned in `docs/plans/durable-task-runs.md` (phase 1 + reports already
  landed); a separate workstream that can finish before atomic-cloud exists.
- **2026-05-25** — Provider keys encrypted at rest via app-side AES-256-GCM
  with master key in env (v1). Wrapped behind a `KeyVault` trait so KMS
  envelope encryption is a localized swap (v2). Schema doesn't change.
- **2026-05-25** — `model_config` lives with the key in control plane, not in
  per-DB settings. Provider config is account-level v1; per-KB override is a
  future optional addition.
- **2026-05-25** — Cloud does **not** support Ollama. OpenRouter and
  OpenAI-compatible only.
- **2026-05-25** — Validate provider keys on save (test call against
  provider's auth-check endpoint). Periodic re-validation deferred — see
  Open questions.
- **2026-05-25** — Cloud always passes `Some(ProviderConfig)` to
  `AtomicCore::open*` — `None` would fall back to settings-table lookup,
  which is the registry-fallback path we explicitly avoid in cloud.
- **2026-05-25** — `ProviderConfig` gets a custom `Debug` impl that redacts
  `*_api_key` fields. Lives in atomic-core (not cloud-aware; pure hygiene).
- **2026-05-25** — Observability: per-tenant labels only on a small
  operationally-critical metric set; everything else per-cluster. Higher-
  cardinality detail goes through traces, not metrics.
- **2026-05-25** — `account_events` table in tenant DB for user-facing
  activity log. Discrete named outcomes only; high-volume operations stay
  in logs + rollups in `quota_usage`.
- **2026-05-25** — Two-tier quotas: anti-abuse rate limits (per-pod
  approximate counters via `governor`) and plan-tier resource limits
  (Postgres-backed strong consistency via UPSERT on `quota_usage`).
- **2026-05-25** — Background jobs that hit LLM quota limits **block**
  (sit in ledger) rather than fail. Same hold-message pattern as
  account-upgrading.
- **2026-05-25** — Billing v1 is BYOK + subscription (platform fees only,
  no AI-call metering). Platform-proxy with per-call metering is v2.
- **2026-05-25** — Stripe via Customer Portal. Webhook at
  `app.atomic.cloud/billing/webhook` (single URL, not per-subdomain).
- **2026-05-25** — **Never auto-delete data for payment failure.** Read-only
  after 3 days past_due, suspended after 14 days, data retained. Hard-delete
  only on explicit user action.
- **2026-05-25** — Trials: 14 days of paid tier on signup, no card required.
  Auto-downgrade to free after.
- **2026-06-09** — Provider keys are **managed by default**: per-tenant
  OpenRouter keys created via the provisioning API with a hard credit limit
  and native monthly reset. BYOK (OpenRouter or OpenAI-compatible) remains
  as an opt-in escape hatch. Supersedes "BYOK only for v1" — BYOK at the
  front door blocked the first magic moment, and per-key credit limits
  remove the margin-risk argument.
- **2026-06-09** — AI-spend enforcement is delegated to OpenRouter per-key
  credit limits; internal `quota_usage` AI counters are advisory UX only.
  AI allowances are denominated in credits (free placeholder $0.50/mo), not
  call counts.
- **2026-06-09** — Managed mode pins the embedding model fleet-wide and
  curates the tagging/wiki/chat model list; frontier models are a paid-tier
  feature flag. BYOK accounts choose freely (with a loud re-embed warning
  on embedding-model switches).
- **2026-06-09** — Billing v1 is subscription with included AI credits
  (managed-key allowance enforced by OpenRouter). No per-call metering.
  Replaces "BYOK + subscription" as the v1 billing model.
- **2026-06-09** — Backups: nightly per-tenant logical dumps to object
  storage (14 daily + 8 weekly), final dump to `backups/final/` (30-day
  retention) before account deletion, restore runbook rehearsed before
  launch, backup-staleness alerting. PITR via WAL archiving deferred.
- **2026-06-09** — Second auth chokepoint test: credentials for account A
  presented on account B's subdomain must fail. The `.atomic.cloud` cookie
  crosses subdomains by design, so this test is what pins browser-level
  tenant isolation.
- **2026-06-09** — AccountCache eviction skips entries with live WebSocket
  subscribers (`event_tx.receiver_count() > 0`), or WS activity counts as
  a touch.
- **2026-06-09** — The old `crates/atomic-cloud` prototype (Fly
  machine-per-customer; never shipped) is a parts bin: salvage magic-link,
  Mailgun, Stripe clients and signup frontend; the Fly provisioning dies.
- **2026-06-10** — Slice 1 landed (see Implementation log). Deviations
  ratified there: reserve-before-delete ordering + post-claim re-check,
  `account_provisioning` 503 variant, ResolvedTenant carries principal only,
  inert FallbackAppState + fail-closed cloud_plane_guard, CASCADE FKs as
  safety net.
- **2026-06-10** — Export jobs, `/api/logs`, and `/api/auth/*` are unrouted
  (404) in the cloud composition until each gets a per-tenant story — they
  bind process-global state in atomic-server and would otherwise be a
  cross-tenant namespace.
- **2026-06-10** — Per-tenant pools are bounded at the composition layer
  (default 5 connections, 5-min idle timeout) via a cloud-unaware
  pool-config constructor added to atomic-core. The plan's pgbouncer
  assumption stands, but the per-pool cap no longer depends on it.
- **2026-06-10** — No auth caching in v1: every request verifies
  subdomain + credential against the control plane, keeping revocation and
  deletion immediate. AccountCache caches only manager/event-channel
  resolution. Revisit under real load.
- **2026-06-10** — Slice 2 landed (see Implementation log). Magic-link-only
  auth implemented end to end; hash-only link storage; timing-uniform
  enumeration defense on login; provisioning semaphore that never consumes
  tokens on saturation.
- **2026-06-10** — Failed provisions are hard-deleted, never tombstoned;
  the reaper recovers interrupted deletions via the active-account-without-
  mapping-row predicate. Reaper rollback/resume runs under per-account
  advisory locks; multiple pods may reap concurrently.
- **2026-06-10** — Anti-abuse limits ship as per-pod hand-rolled sliding
  logs (request-link: 5/IP/hour shared across signup+login, 3/email/hour);
  the rest of the quota table waits for the quotas slice.
- **2026-06-11** — Slice 3 landed (see Implementation log). Explicit
  provider mode is authoritative end to end: tenant settings writes are
  inert for provider keys, model routing, and embedding-space changes.
  One LLM selection governs tagging/wiki/chat/reports in explicit mode.
- **2026-06-11** — BYOK embedding-dimension changes are rejected
  (`embedding_dimension_unsupported`), superseding the plan's "warn
  loudly" — the warning was unfulfillable without a dimension-migration
  mechanism. The embedding model/dimension pin is enforced fleet-wide at
  `PINNED_EMBEDDING_DIMENSION`.
- **2026-06-11** — Provider-config staleness is bounded by
  `accounts.provider_generation`, observed on CloudAuth's existing
  per-request lookup: rotations converge across pods and cache-rebuild
  races within one request, with no cross-pod invalidation infrastructure.
- **2026-06-11** — KeyVault AAD binds (account_id, provider, origin) with
  length-prefixed encoding; managed/BYOK ciphertexts are not swappable
  even with direct DB write access.
- **2026-06-12** — Slice 4 landed (see Implementation log). Dispatcher is
  per-pod with no leader election; claims ride the existing ledger lease
  machinery; pipeline execution moves behind the cloud-unaware
  `inline_pipeline` knob (gated before the claim).
- **2026-06-12** — Environmental failures defer, never fail:
  `RunHandle::defer_until` refunds the attempt (extending the
  lease-reclaim precedent); 402/429/401-classified background work sits in
  the ledger and provider mutations re-arm it immediately. Breaker
  accounting counts only provider-touching executions.
- **2026-06-12** — Multi-pod WS event delivery is a known v1 limitation:
  worker events are per-pod in-memory; build the cross-pod relay
  (Postgres LISTEN/NOTIFY) before running more than one pod in
  production. Durable state is unaffected.
- **2026-06-12** — Slice 5 landed (see Implementation log). Deploy gating
  per the plan's policy table; additive-only migrations enforced by a
  lint test over both migration dirs.
- **2026-06-12** — Migration recovery is keyed on lagging-ness, not
  recorded failure state: the reaper retries any active tenant below the
  compiled target. Supersedes the boot-runner/reaper ownership split,
  which left mid-deploy signups and lost record-writes permanently
  stragglered.
- **2026-06-13** — Slice 6 landed (see Implementation log): plans + quota
  enforcement, full Stripe billing, the dunning state machine, period
  rollover + storage enforcement. Billing is optional (no Stripe key →
  routes 503, dunning no-ops) so dev/self-hosted clusters are unaffected.
- **2026-06-13** — Dunning lives in `accounts.billing_state`, orthogonal to
  `status`: a delinquent account stays `status='active'`. Never auto-delete
  on downgrade/suspend/storage-restrict — writes block, data is retained.
- **2026-06-13** — The atom ceiling is account-wide and enforced on every
  creation surface, not just `POST /api/atoms`: the request-time guard sums
  across KBs; background atom-creating work (feeds, reports) defers in the
  dispatcher when at-ceiling; manual-trigger routes (import, run, poll) are
  gated. It is a soft ceiling (live count, no reservation) — bounded TOCTOU
  overshoot accepted; never affects money or managed-key credits.
- **2026-06-13** — Stripe webhook: signature verified before any effect;
  claim + apply in one transaction so a crash reprocesses on redelivery
  rather than dedup'ing into a no-op; constant-time MAC comparison.
- **2026-06-13** — Slice 7 landed (see Implementation log): cloud's own
  per-account OAuth 2.0 flow (DCR + auth code + PKCE, account resolved from
  Host, consent gated by the session cookie) and a per-tenant MCP endpoint.
  Resolves the open MCP-token-default-scope question: account scope by
  default, db-pin supported and chokepoint-enforced.
- **2026-06-13** — The one atomic-server change is cloud-unaware: the MCP
  transport resolves its DatabaseManager per request (a RequestManager
  extension mirroring the Db extractor), self-hosted byte-identical;
  routes/oauth.rs untouched, atomic-core untouched.
- **2026-06-13** — OAuth consent is clickjacking-hardened (X-Frame-Options +
  CSP frame-ancestors on every OAuth HTML path), and the db-pin chokepoint is
  enforced on the MCP default no-selection path (transport reads the injected
  X-Atomic-Database header, header → ?db= → active, matching the Db extractor).
- **2026-06-13** — Slice 8 landed (see Implementation log): per-tenant +
  control-plane nightly logical backups (pg_dump -Fc) to a BackupStore
  (local / S3 via object_store), the fail-closed final dump before account
  deletion, the restore CLI + rehearsed runbook, and >36h staleness alerting.
  Backups are opt-in (--backup-enabled / a configured store) so dev and
  self-hosted clusters are unaffected.
- **2026-06-13** — The final dump is the operator's only undo for hard-delete
  v1: it runs before the irreversible DROP and a dump failure ABORTS the
  deletion (no backup → no drop). delete_account takes an explicit
  BackupPolicy (type-enforced, never a silent fail-open) and acquires the
  per-account advisory lock so a backup and a delete of the same tenant can
  never race. pg_dump/pg_restore run under a kill-budget timeout. Credentials
  ride in PGPASSWORD env, never argv.
- **2026-06-13** — Backup retention (14 daily + 8 weekly; final 30-day) is
  object-store bucket-lifecycle policy keyed off the dated object layout, not
  application code. PITR/WAL archiving remains deferred.
- **2026-06-14** — Slice 9 landed (see Implementation log): the account-plane
  frontend — a polished React SPA (the cloud front door) styled to match the
  marketing website (light/paper, Crimson Pro + DM Sans, node-graph motif),
  not the dark product app. One build serves the public app-host pages
  (landing/signup/login) and the per-tenant authenticated dashboard
  (/account/*, same-origin session-cookie authed). Billing is Stripe-portal +
  status; the dashboard reads a new account-scope GET /api/account/overview.
  atomic-core/atomic-server untouched; the SPA is served as the cloud app's
  default_service, registered last so it never shadows an API/OAuth/MCP route.
- **2026-06-14** — With slice 9, the frontloaded functional build-out is
  complete: control plane, provisioning, auth, providers, dispatcher, deploy
  gating, billing/quotas, OAuth/MCP, backups, and the account frontend.
  Remaining work is observability and the iterative visual polish of the
  frontend (a human-in-the-loop pass), plus the deferred items each slice
  recorded.
