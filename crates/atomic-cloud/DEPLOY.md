# Atomic Cloud — Production Deployment Runbook

This is the operator's checklist for standing up a production Atomic Cloud pod.
It documents the host-split routing model, the reverse-proxy and DNS/TLS
requirements, the single-pod constraint v1 ships with, the pod image
(`cloud.dockerfile`), and the full env/secret checklist `serve` expects.

`README.md` is the source of truth for *what the crate is and how it's wired*;
this file is the source of truth for *what an operator must provide to run it
in production safely*. Where a setting depends on infrastructure that lives
outside this repo (DNS provider, k8s manifests, S3 bucket policy), it is marked
**[CONFIRM]** — fill it in for your environment before launch.

> **Read this in full before standing up the first production pod.** Several
> of the items below (the tenant marker, `--trust-proxy-header`, the backup
> store, the single-pod constraint) silently void a correctness or data-safety
> guarantee if skipped — they fail open, not closed.

---

## 1. Topology at a glance

```
                       ┌──────────────────────────────────────┐
   wildcard DNS        │            Reverse proxy             │
   *.<base>  ────────► │  - TLS termination (wildcard cert)   │
   <base>, app.<base>  │  - sets X-Forwarded-For              │
                       │  - host-split: app vs tenant         │
                       └───────────────┬──────────────────────┘
                                       │  (HTTP, peer = proxy)
                          ┌────────────▼─────────────┐
                          │     atomic-cloud serve   │  (single pod, v1)
                          │  --trust-proxy-header     │
                          └──────┬─────────────┬──────┘
                                 │             │
                    control plane (PG)   tenant cluster (PG, budgeted — §6)
                    atomic_cloud_control   acct_<base32(uuid)> × N
```

Routing is split by the `Host` header (see `README.md` → "Request lifecycle"):

- **App host** — the bare base domain (`<base>`) and `app.<base>` — serves the
  unauthenticated account plane: signup, login, the Stripe webhook
  (`app.<base>/billing/webhook`), and the authenticated `/account/*` dashboard
  SPA.
- **Tenant subdomains** (`<slug>.<base>`) serve the data plane: the full
  atomic-server API wrapped in `CloudAuth`, the per-tenant `/mcp` and `/ws`
  endpoints, and (in production) the product knowledge-base app at the tenant
  root.

The server reads the raw `Host` header to decide which plane a request hits, so
**the proxy MUST forward the client's `Host` unmodified** and MUST NOT let a
client spoof it past the trust boundary. This is the hinge of the whole
isolation model — get it wrong and a tenant request can be misrouted.

---

## 2. DNS, TLS, and encryption at rest

You need two DNS records and a TLS cert that covers both the app host and every
tenant subdomain:

- An A/AAAA (or CNAME) record for the **app host**: `app.<base>` (and,
  optionally, the bare `<base>` apex if you serve a marketing root there).
- A **wildcard** record `*.<base>` so every provisioned tenant subdomain
  resolves without a per-signup DNS write. Tenant subdomains are created
  on-demand at signup; there is no hook that mints DNS records, so the wildcard
  is mandatory.
- A **wildcard TLS certificate** covering `*.<base>` **and** `app.<base>`. A
  bare `*.<base>` wildcard does **not** cover the apex or a second-level host in
  most issuers' interpretation, so request a SAN cert listing both, or use a
  cert that explicitly includes `app.<base>`.

The session cookie is scoped to `.<base>` so it is presented on every tenant
subdomain (this is what lets a dashboard login authenticate the same-origin
product app); the cookie is `Secure` in production, which is **why TLS on
every tenant subdomain is non-negotiable** — without it the session cookie is
never sent and login appears to "do nothing".

**[CONFIRM]** DNS provider + wildcard record config, and the ACME/issuer flow
that produces the `*.<base>` + `app.<base>` cert (e.g. cert-manager with a
DNS-01 solver, or a managed LB cert). Wildcard certs generally require DNS-01,
not HTTP-01.

### App ↔ Postgres TLS (required)

The cert above protects the **client ↔ proxy** leg. The **app ↔ Postgres** leg
carries tenant content *and* the encrypted-credential ciphertexts, so it must be
TLS too. The server negotiates TLS from each connection URL's `sslmode`; set
`sslmode=require` — or `verify-full`, which additionally checks the server
certificate — on **both** `--control-url` and `--cluster-url`:

```
postgres://USER:PASS@HOST:5432/atomic_cloud_control?sslmode=verify-full
postgres://USER:PASS@HOST:5432/postgres?sslmode=verify-full          # cluster
```

`serve` emits a boot **warning** when a non-localhost deployment's URL doesn't
ask for a TLS-negotiating mode. It's a warning, not a hard failure, because some
setups terminate TLS out of band — e.g. the Cloud SQL Auth Proxy / a sidecar
over a local socket. If that's you, the link is already encrypted and the
warning is expected; otherwise, treat it as a must-fix.

### Encryption at rest

Atomic's at-rest posture is **per-account database isolation + infrastructure
volume encryption + app-encrypted secrets** — *not* application-level content
encryption. Two layers, and the infrastructure one is yours to enable:

- **Secrets** — provider API keys (managed runtime keys + BYOK) are encrypted by
  the app with **AES-256-GCM**, each ciphertext AEAD-bound to its
  `(account, provider, origin)` row, under `ATOMIC_CLOUD_MASTER_KEY` (env-only;
  see [`keyvault.rs`](src/keyvault.rs)). A stolen database dump alone cannot
  decrypt them — the key lives out of band.
- **Tenant content** — atoms, embeddings, wiki, chat live as ordinary rows in
  each tenant database; their at-rest encryption is the **cluster's volume
  encryption**, which is an infra setting you must turn on:
  - **[CONFIRM] Enable volume/disk encryption** on the Postgres cluster (every
    managed PG offers AES-256 at the storage layer).
  - **[CONFIRM] Enable bucket encryption (SSE)** on the backup bucket —
    `pg_dump` artifacts hold the same plaintext content.
- **Master-key custody** — back `ATOMIC_CLOUD_MASTER_KEY` up **out of band,
  separate from the database backups** (a bundle holding both the ciphertexts
  and the key is plaintext with extra steps). Losing it makes every stored
  provider credential unrecoverable.

> Because tenant content is not app-encrypted, an operator with database access
> can read it. Describe the product as **isolated + encrypted at rest (infra)**,
> never as zero-knowledge or end-to-end encrypted.

---

## 3. The product-app tenant marker (OPS-2 — do not skip)

The product knowledge-base app decides whether it is running as a cloud tenant
by reading a meta tag:

```html
<meta name="atomic-cloud-tenant" content="__ATOMIC_CLOUD_TENANT__" />
```

The frontend's `isCloudTenant()` returns `true` **only** when that `content`
attribute has been rewritten to the literal string `true`. When it is `true`,
the app authenticates by the same-origin session cookie (set by the dashboard
login) and shows no "enter a server URL + token" setup screen. When it is left
as the placeholder (or the meta tag is missing entirely), `isCloudTenant()` is
`false` and **every tenant lands on the self-hosted setup screen — login is
silently broken.**

There are exactly two supported ways to serve a product bundle that carries the
marker:

1. **Let the cloud server serve it.** Point `serve --product-dir` at the built
   product bundle (`npm run build:web` → `dist-web`). The server rewrites the
   placeholder to `true` once at boot when it loads the product
   `index.html`. This is the simplest correct option and is recommended for the
   single-pod v1 topology — and it is what the pod image (§7) wires up out of
   the box.
2. **Have the reverse proxy / build inject it.** If the proxy serves the
   product bundle directly at the tenant root (the classic prod topology), the
   bundle it serves **must** already have the marker rewritten to
   `content="true"` — either baked at build time or rewritten by the proxy
   (e.g. an nginx `sub_filter __ATOMIC_CLOUD_TENANT__ true;` on the tenant-root
   HTML response). **[CONFIRM]** the exact proxy rewrite rule for your proxy.

**Safety net:** when the cloud server serves the product app via
`--product-dir`, it emits a loud `warn!` at boot if the loaded `index.html` is
missing the `__ATOMIC_CLOUD_TENANT__` placeholder (which means the `true`
injection would no-op and tenant auth would break). Treat that warning as a
launch blocker. The proxy-served path (option 2) has no such automatic check —
verify by hand that `curl https://<slug>.<base>/ | grep atomic-cloud-tenant`
shows `content="true"` before going live.

---

## 4. Behind a reverse proxy: `--trust-proxy-header` (OPS-4)

The pod binds `127.0.0.1` by default (or a pod-internal address) and sits
behind the proxy, so the connection peer address is **always the proxy**, not
the client. The per-IP rate limiters (anti-abuse on signup, magic-link issue,
etc.) therefore collapse to a single bucket — the proxy's IP — unless you tell
the server to read the client IP from `X-Forwarded-For`:

```
--trust-proxy-header           # or ATOMIC_CLOUD_TRUST_PROXY_HEADER=true
```

Enable this **if and only if** a trusted proxy fronts the process and that
proxy appends (does not pass through) the client IP as the **rightmost**
`X-Forwarded-For` entry. With the flag on and no such proxy, clients can spoof
the header and sidestep per-IP limits; with the flag off behind a proxy, every
client shares the proxy's bucket. (Per-email and per-account limiters still
apply either way, which is why OPS-4 is an accepted risk rather than a blocker —
see §10.)

**[CONFIRM]** that your proxy strips any client-supplied `X-Forwarded-For` and
appends the real peer, so the rightmost entry is trustworthy.

---

## 5. Single-pod constraint (v1): no cross-pod WS relay

**Run a single `serve` pod in v1.** Durable state is always correct across
pods — ledger claims, atom statuses, backups, and migrations all serialize
safely on Postgres advisory locks / unique indexes, so multiple pods will not
corrupt data. The limitation is **live WebSocket progress events**:

- Background workers publish pipeline/ingestion progress into the executing
  pod's in-memory per-account event channel (`AccountCache` entry's
  `event_tx`).
- A WebSocket session subscribed on a *different* pod receives none of that
  execution's progress events. The frontend self-heals on its next fetch (the
  durable state is correct), but live progress is faithful **only when the
  executing pod and the subscribed pod are the same** — i.e. a single pod.
- MCP tool calls that create atoms broadcast onto the transport's own channel,
  not the per-account WS channel, so their events are likewise not relayed
  (a pre-existing v1 limitation).

A cross-pod event relay (Postgres `LISTEN/NOTIFY` fan-out) is a planned
follow-up. Until it lands, keep the dispatcher/serve pod count at **one**. If
you must scale request-serving capacity before the relay exists, that is a
design change, not a config change — do not run two `serve` pods and expect
correct live progress.

---

## 6. The cluster connection budget (REL-3)

Each cached active tenant holds its own Postgres pool against the **shared**
tenant cluster (`--tenant-pool-max-connections`, default 5). At fleet scale the
fan-out is roughly:

```
max worst-case backend connections ≈ account_cache_max_entries
                                      × tenant_pool_max_connections
                                      × pods
```

A single pod with ~1000 cached tenants × 5 connections is ~5000 backend
connections — far past a bare Postgres `max_connections`. At fleet scale
that demands a pooler; at launch scale it demands a sized budget (see the
limitation box below for why the pooler can't be transaction-mode yet).
Whichever you run, size the cluster's real `max_connections` against your
launch fleet, plus headroom for:

- the control-plane pool (`--control-pool-max-connections`, default 25),
- the nightly backup pass (`pg_dump` opens its own connections),
- the reaper and other background loops.

> **Verified limitation (2026-07): transaction pooling does not work today.**
> The provisioning, migration, and backup paths hold **session-scoped advisory
> locks**, and transaction pooling migrates a client between server connections
> across statements — provisioning hangs within seconds (reproduced against
> pgbouncer 1.24 in `pool_mode = transaction`). Until the advisory-lock usage
> is made transaction-scoped, the realistic options are:
>
> - **Direct connections + a sized connection budget** — what the single-box
>   [`deploy/`](../../deploy) stack does: `max_connections = 200` on the
>   cluster, bounded by the app's per-tenant pool caps and idle TTLs. Fine
>   through soft-launch scale; the fan-out math above says when it stops
>   being fine.
> - **Session-pooling mode** — safe, but sqlx holds its pool connections open,
>   so multiplexing gains are minimal; it mostly buys connection-storm
>   shielding.
>
> Treat "make the pool paths transaction-pooling-safe" as the prerequisite for
> the fleet-scale pgbouncer topology this section describes.

**[CONFIRM]** the connection budget: either a sized direct `max_connections`
(single-box) or a session-mode pooler — and if you ever switch to transaction
pooling, re-verify provisioning end-to-end first (see the limitation box).

`pg_dump`/`pg_restore` must be on `PATH` in the pod image — backups and account
deletion's final dump shell out to them. The shipped image (§7) bakes them in;
match its client major to the cluster's.

---

## 7. The pod image (`cloud.dockerfile`)

The repo ships the pod as a single multi-stage image. Build it from the repo
root:

```bash
docker build -f cloud.dockerfile -t atomic-cloud .
```

One image carries everything §1's pod needs:

- **The `atomic-cloud` binary.** The server and the operator CLI are the same
  binary, so `docker run … atomic-cloud migrate|account|token|backup …` runs
  operator commands against the same image the pod serves from.
- **Both frontends, pre-wired.** The account-plane SPA is baked at `/srv/spa`
  and the product knowledge-base app at `/srv/product`, with
  `ATOMIC_CLOUD_SPA_DIR`/`ATOMIC_CLOUD_PRODUCT_DIR` pre-set. The §3 tenant
  marker is handled by the server's boot-time rewrite (option 1) — and the
  image build fails outright if the `__ATOMIC_CLOUD_TENANT__` placeholder ever
  goes missing from the product bundle, so a marker regression can't reach a
  running pod.
- **`pg_dump`/`pg_restore`** (PGDG `postgresql-client`), for the nightly
  backups and the fail-closed final dump (§6).

Two operational notes:

- **Match the client major to the cluster.** `pg_dump` refuses to dump from a
  server newer than itself. The image defaults to PostgreSQL 17 client tools;
  build with `--build-arg PG_CLIENT_MAJOR=<major>` at least your tenant
  cluster's major.
- **No separate migrate step is required.** `serve` connects to (creating if
  absent) and migrates the control plane at boot, then runs the boot fleet
  migration over lagging tenants behind `/ready`. `atomic-cloud migrate`
  exists to run the control-plane step explicitly ahead of a deploy.

The container listens on `0.0.0.0:8080` (override via the CMD's
`--bind`/`--port`), runs as a non-root user, and health-checks `GET /health`
(liveness — pair it with `GET /ready` for deploy gating). All configuration is
env-only (§8). The §5 constraint applies to this image like any other
packaging: exactly one `serve` replica.

---

## 8. Required env / secret checklist

`serve` takes the **NAME** of an env var on argv for every secret and reads the
VALUE from the environment, so secrets never appear in `ps` / `/proc/<pid>/cmdline`.
Keep that discipline: pass secrets via env, not flags.

### Always required (the pod will not serve without these)

| Setting | Env var | Notes |
|---|---|---|
| Control-plane DB URL | `ATOMIC_CLOUD_CONTROL_URL` | Postgres URL of `atomic_cloud_control`. Direct or session-pooled — never transaction-pooled (§6). |
| Tenant cluster URL | `ATOMIC_CLOUD_CLUSTER_URL` | Shared cluster; the role must be able to `CREATE`/`DROP DATABASE`. Direct or session-pooled — never transaction-pooled (§6). |
| Base domain | `ATOMIC_CLOUD_BASE_DOMAIN` | e.g. `atomic.cloud`. Drives host-split routing, cookie scope, redirect URLs. |
| Master key | `ATOMIC_CLOUD_MASTER_KEY` (name overridable via `--master-key-env`) | 32 bytes, hex or base64. Encrypts provider credentials at rest. **Loss of this key = unrecoverable tenant credentials** (see `keyvault` custody runbook). `serve` refuses to boot without a valid key. |

### Required for the AI experience (managed keys)

| Setting | Env var | Notes |
|---|---|---|
| Provisioning mode | `ATOMIC_CLOUD_PROVISIONING_MODE` | Set to `openrouter` to mint a managed key per account at signup. `disabled` (default) = keyless accounts (dev only). |
| OpenRouter provisioning key | `ATOMIC_CLOUD_OPENROUTER_PROVISIONING_KEY` (name overridable via `--openrouter-provisioning-key-env`) | Mints runtime keys against the master OpenRouter account's balance — crown-jewel custody. Required with `provisioning-mode=openrouter`. |

### Required for billing (Stripe) — omit to run billing-disabled

| Setting | Env var | Notes |
|---|---|---|
| Stripe secret key | `STRIPE_SECRET_KEY` (name overridable via `--stripe-secret-key-env`) | `sk_…`. Billing is enabled iff this is set non-empty; unset/empty ⇒ billing routes return a structured 503 and dunning has nothing to advance. |
| Stripe webhook secret | `STRIPE_WEBHOOK_SECRET` (name overridable via `--stripe-webhook-secret-env`) | `whsec_…`. Required for the webhook to accept anything. |
| Plan→price map | `ATOMIC_CLOUD_STRIPE_PRICES` | `plan_id=price_id` pairs, comma-separated (or repeat `--stripe-price`). |

### Required for email delivery (production) — defaults to `log` (dev only)

| Setting | Env var | Notes |
|---|---|---|
| Email mode | `ATOMIC_CLOUD_EMAIL_MODE` | Set to `mailgun` in prod; `log` writes magic links to the log (dev delivery channel — **never** prod). |
| Mailgun API key | `ATOMIC_CLOUD_MAILGUN_API_KEY` | Required with `mailgun`. Prefer env over argv (SEC-2). |
| Mailgun domain | `ATOMIC_CLOUD_MAILGUN_DOMAIN` | e.g. `mg.atomic.cloud`. |
| Mailgun from | `ATOMIC_CLOUD_MAILGUN_FROM` | e.g. `Atomic <no-reply@mg.atomic.cloud>`. |

### Backups (REL-3 / MIG-1 — see Accepted-Risks)

| Setting | Env var | Notes |
|---|---|---|
| Backup store | `ATOMIC_CLOUD_BACKUP_STORE` | **Set to `s3` in production.** The default `local` writes dumps to local disk — ephemeral on most pods, so a tenant hard-delete's only undo evaporates on restart. |
| S3 bucket | `ATOMIC_CLOUD_BACKUP_BUCKET` | Required with `--backup-store s3`. |
| S3 region / endpoint / prefix | `ATOMIC_CLOUD_BACKUP_REGION`, `ATOMIC_CLOUD_BACKUP_ENDPOINT`, `ATOMIC_CLOUD_BACKUP_PREFIX` | Endpoint for R2/MinIO; prefix when sharing a bucket. |
| S3 credentials | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` (standard `AWS_*`) | Read from the environment by `object_store`, never argv. |

**[CONFIRM]** the S3 bucket lifecycle/retention policy and IAM/bucket policy
(the code only *writes* dumps; retention/expiry/versioning is yours to set).

### Behind a proxy / dev safety flags

| Setting | Env var | Notes |
|---|---|---|
| Trust proxy header | `ATOMIC_CLOUD_TRUST_PROXY_HEADER` | **Required behind a proxy** (§4). |
| App public URL | `ATOMIC_CLOUD_APP_PUBLIC_URL` | Defaults to `https://app.<base>`; override for non-standard scheme/port. |
| Dangerously insecure cookies | `ATOMIC_CLOUD_DANGEROUSLY_INSECURE_COOKIES` | **NEVER set in prod** — drops `Secure` from the session cookie. Boot warns loudly when set. |
| Product app dir | `ATOMIC_CLOUD_PRODUCT_DIR` | Point at `dist-web` to have the server serve + mark the product app (§3). |
| Account SPA dir | `ATOMIC_CLOUD_SPA_DIR` | Account dashboard bundle; defaults to `crates/atomic-cloud/frontend/dist`. |

### Tuning knobs (sensible defaults; revisit under load)

`ATOMIC_CLOUD_CONTROL_POOL_MAX_CONNECTIONS` (25), `ATOMIC_CLOUD_TENANT_POOL_MAX_CONNECTIONS` (5),
`ATOMIC_CLOUD_TENANT_POOL_IDLE_TIMEOUT_SECS` (300), `ATOMIC_CLOUD_REAPER_INTERVAL_SECS` (60),
`ATOMIC_CLOUD_MAX_CONCURRENT_PROVISIONS`, `ATOMIC_CLOUD_CHAT_STREAMS_PER_ACCOUNT`,
`ATOMIC_CLOUD_BACKUP_INTERVAL_SECS` (24h), `ATOMIC_CLOUD_MAX_BACKUPS_PER_PASS`,
`ATOMIC_CLOUD_BACKUP_STALENESS_SECS`, `ATOMIC_CLOUD_BACKUP_TIMEOUT_SECS`,
`ATOMIC_CLOUD_PERIOD_ROLLOVER_INTERVAL_SECS` (1h), `ATOMIC_CLOUD_STORAGE_RECOMPUTE_INTERVAL_SECS` (1h),
`ATOMIC_CLOUD_STORAGE_WARN_AFTER_DAYS` (0), `ATOMIC_CLOUD_STORAGE_RESTRICT_AFTER_DAYS` (7),
`ATOMIC_CLOUD_DUNNING_READ_ONLY_DAYS`, `ATOMIC_CLOUD_DUNNING_SUSPENDED_DAYS`,
`--port` (8080) and `--bind` (flag-only — bind a pod-internal address behind the proxy).

> **Reaper warning:** `ATOMIC_CLOUD_REAPER_INTERVAL_SECS=0` disables the reaper.
> The reaper's orphan-DB reclaim **drops** any `acct_*` database the control
> plane doesn't reference. **Never point a production pod's `--control-url` at
> an empty/wrong control plane with the reaper enabled** — a misdirected
> control URL after a failover/restore can drop live tenant databases within a
> reaper interval. Verify the control URL before every deploy.

---

## 9. Pre-launch checklist

- [ ] Wildcard DNS `*.<base>` + `app.<base>` resolve to the proxy. **[CONFIRM]**
- [ ] Wildcard TLS cert covers `*.<base>` **and** `app.<base>`. **[CONFIRM]**
- [ ] Proxy forwards `Host` unmodified and appends a trustworthy
      `X-Forwarded-For`; `--trust-proxy-header` is set. **[CONFIRM]**
- [ ] Product bundle served at the tenant root carries
      `content="true"` for the tenant marker (curl-verified, or
      `--product-dir` with no boot warning). (§3)
- [ ] Exactly **one** `serve`/dispatcher pod is running (no cross-pod WS relay
      in v1). (§5)
- [ ] The connection budget covers the launch fleet: sized `max_connections`
      for direct connections (single-box), or a session-mode pooler —
      **transaction pooling is currently incompatible**. **[CONFIRM]** (§6)
- [ ] `pg_dump`/`pg_restore` are on the pod image `PATH` (baked into
      `cloud.dockerfile`; `PG_CLIENT_MAJOR` ≥ the cluster's major). (§7)
- [ ] `--backup-store s3` with a durable bucket; `AWS_*` creds present; no
      `local` store in prod. (§8, MIG-1)
- [ ] `ATOMIC_CLOUD_MASTER_KEY` present, valid, and backed up off-pod **separate
      from the database backups** (loss = unrecoverable credentials). (§2)
- [ ] `--control-url` and `--cluster-url` carry `sslmode=require`/`verify-full`
      (or TLS is terminated by a trusted proxy/socket) — no app↔Postgres TLS
      boot warning. (§2)
- [ ] Volume/disk encryption is enabled on the Postgres cluster. **[CONFIRM]** (§2)
- [ ] Server-side encryption (SSE) is enabled on the backup bucket. **[CONFIRM]** (§2)
- [ ] Stripe keys + webhook secret + price map set (if a paid tier is live);
      webhook endpoint reachable at `app.<base>/billing/webhook`.
- [ ] Mailgun configured (`--email-mode mailgun`); `log` mode is **not** in use.
- [ ] `--dangerously-insecure-cookies` is **not** set (no boot warning for it).
- [ ] `ATOMIC_CLOUD_ALLOW_PRIVATE_PROVIDER_URLS` is **not** set (no boot warning;
      the BYOK SSRF gate is active).

---

## 10. Accepted Risks (residual, accepted for soft launch)

These are the items the ship-readiness review explicitly accepted for the soft
launch. They are documented here so an operator knows the edge each one lives
on and what the follow-up is. None block soft launch on its own; track them as
fast-follows.

| ID | Risk | Why accepted for soft launch | Follow-up |
|---|---|---|---|
| **REL-3** | Tenant pool fan-out (`max_entries × pool × pods`) can exceed the cluster's connection budget at fleet scale. | §6 documents the budget math; single-box launch runs direct with sized `max_connections` (transaction-mode pgbouncer is currently incompatible — session advisory locks). | Make the pool paths transaction-pooling-safe, then add a boot sanity check relating fan-out to the budget. |
| **OPS-3** | Logs are human-text only — no JSON, no request-correlation id. | `account_id` is logged in hot paths, so an operator can grep by tenant + timestamp. | Add `--log-format json` + an `X-Request-Id` correlation id as the observability floor. |
| **OPS-4** | `--trust-proxy-header` is off by default with no boot warning → per-IP limits collapse to the proxy IP behind a proxy. | Per-email and per-account limiters still apply. §4 documents it as required-behind-proxy. | Emit a boot `warn!` when no proxy-trust is configured but the bind looks proxied. |
| **OPS-5** | `panic = "abort"` in the server profile → any panic aborts the whole pod (all tenants), not just the failing request. | Defensible fail-fast: durable state always recovers and the orchestrator restarts the pod. | Consider `panic = "unwind"` for the server profile later, accepting the larger binary. |
| **ISO-6** | `AccountCache` hard-cap can be exceeded when every entry has a live WS subscriber. | Won't bite below ~1000 connected tenants/pod. | Add a per-pod WS cap + a cache-length gauge. |
| **ISO-7** | Pre-auth account state (provisioning/upgrading/suspended) is distinguishable by status code on a tenant subdomain. | The subdomain is already semi-public; this is a coarse lifecycle leak, no secret exposed. | Optionally gate the `suspended` 402 behind a valid credential later. |
| **AUTH-2** (dropped) | Session cookie scoped to `.<base>` is sent to every tenant subdomain. | Unreachable today: gated behind `HttpOnly` **and** the no-tenant-active-content invariant (no `rehype-raw` / `dangerouslySetInnerHTML` on `*.<base>`). | Keep "no XSS / no tenant active content on `*.<base>`" a hard, tested invariant + add CSP. |
| **REL-5** | Last-writer-wins `provider_pause_kind` can briefly re-open AI routes for an out-of-credits tenant after a rate-limit trip. | Self-heals on the next provider call; brief window. | Split the column, or preserve the stronger pause kind. |
| **MAI-2 / BILL-4** | First interactive 402 isn't pre-empted — a raw provider error shows until a background job trips the credits pause. | Self-heals in seconds-to-minutes; CRUD is unaffected. | Have interactive handlers classify their own 402. |
| **BILL-3** | Obsidian import counts as a 1-atom delta → bounded `atom_limit` overshoot. | Only reachable if a cloud route exposes `vault_path` to tenants (the current product app does not). | Confirm and document; gate if a vault-path route is ever exposed. |
| **SEC-2** | Mailgun API key can be passed on argv (`ps` / `/proc` leak). | Avoidable by env deployment — pass it via `ATOMIC_CLOUD_MAILGUN_API_KEY`, never the flag. | Switch to a `--mailgun-api-key-env` (name-only) flag to match the other secrets. |
| **DEL-2** | A deleted user's email/IP persists in `magic_links` for up to ~24h. | Inert, not cross-tenant. | Add a `DELETE FROM magic_links WHERE lower(email)=…` to `delete_account`. |
| **DEL-3 / PROV-2** | CLI `account create`/`delete` can't mint/revoke the managed key → keyless accounts / orphaned keys, traced only by a log line. | Operator-facing only; key allowance is capped low. Prefer the HTTP routes (which do both properly). | Add loud notices + a durable audit surface; document the CLI-vs-HTTP asymmetry. |
| **PROV-1** | Reaper-resumed signups never receive the 14-day trial. | Small fraction; recoverable by upgrade, data correct. | Call idempotent `start_trial` from the reaper resume arm. |
| **PROV-3** | The provision permit is held across a multi-second provision, serializing signups at the concurrency cap. | Tunable: raise `--max-concurrent-provisions` and scale pods for launch spikes. | — |
| **MIG-2** | Restore stamps `last_migrated_version` at the binary target, not the dump's version → can skip migrations. | Operator-mediated and rare (nightly backups are near-current). | Stamp the dump's actual version and let the reaper re-migrate. |
| **MIG-3** | Tenant migrations 003/005/008 omit their own `schema_version` insert. | Benign today (022 writes its row; all three are idempotent). | Add the missing inserts + a lint assertion. |
| **DASH-2** | The managed-models LLM list is a hand-maintained mirror of the server constant. | Lists match today. | Add a CI drift assertion, or render the picker from the API. |
| **DASH-3** | The suspended hold screen is a dead-end when `upgrade_url` is null. | Confirm the server always populates `upgrade_url`. | Add a `/account/billing`/support fallback. |
| **PRODCLOUD-2** | An onboarding-completion write is blocked by `billing_write_guard` for a returning `read_only` tenant → the wizard re-loops. | Not reachable at first run (trial = active). | Exempt `onboarding_completed` from the write guard. |

> The **non-accepted** majors (BILL-1 MCP guard bypass, MAI-1/DEL-1/DISP-1
> billing, MIG-1 local backup store, REL-4 orphan reaper, the data-safety
> footguns) are tracked in the ship-readiness report and are launch gates, not
> accepted risks. MIG-1 and REL-4 are partially mitigated here by the §8/§9
> checklist items (S3 backup store; verify the control URL before deploy) —
> follow those to neutralize the operator footgun until the code guards land.
