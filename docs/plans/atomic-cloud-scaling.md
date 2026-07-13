# Atomic Cloud — Single-Box Scaling Concerns

## Status

Living document, started 2026-07-13 (two days post-launch, 7 accounts).
Companion to `atomic-cloud.md`: that plan records what we built and why;
this one records **where the single-droplet topology bends as tenant count
grows**, so we widen bottlenecks deliberately instead of discovering them
as incidents. Launch week already produced the cautionary tales (silent
backup stall, pathological embedding upstream, the canvas full-scan) — the
lesson this doc encodes is *find the shape before the shape finds you*.

Each concern records four things:

- **Shape** — the O(·) behavior and where it lives in code.
- **Bites at** — an honest order-of-magnitude tenant count or load level.
  These are estimates from constants, not load tests; trust the signal
  column over the number.
- **Signal** — what to watch (`pg_stat_statements`, log lines, `df`) so the
  concern announces itself before it hurts.
- **Remediation ladder** — the cheap knob first, the structural fix last.
  Do not build the structural fix speculatively.

Constants cited below are greppable and may drift; the code is
authoritative.

## Summary (rough order of onset)

| # | Concern | Bites at (order) | Hard wall? |
|---|---------|------------------|------------|
| 1 | Concurrently-active tenant connections | ~35 simultaneously active tenants | no (config) |
| 2 | Dispatcher slow scan × AccountCache | ~100s of tenants (churn), ~1000 (thrash) | no |
| 3 | Per-request control-plane auth | high RPS, not tenant count | no |
| 4 | Deploy-time fleet migration wall time | ~1000s of tenants × migration cost | no (policy) |
| 5 | Disk: sold storage vs volume | sold-GB ≈ volume-GB (overcommit) | **yes** |
| 6 | Backup throughput & IO contention | ~1000s of daily dumps | no |
| 7 | Postgres catalogs & autovacuum across N DBs | ~1000s of databases | eventually |
| 8 | One box: CPU/RAM/fate sharing | load-dependent | **yes** (the topology) |
| 9 | Multi-pod blockers (WS events) | pod #2 | gate, not wall |
| 10 | Observability cardinality (pg_stat_statements) | ~100s of DBs | no |

## 1. Concurrently-active tenant connections

**Shape.** Postgres runs with `max_connections=200`
(`deploy/docker-compose.yml`; no pgbouncer — session advisory locks in the
provisioning/backup/deletion paths forbid transaction pooling, see
DEPLOY.md §6). Each cached tenant holds a pool capped at
`tenant_pool_max_connections = 5` (`account_cache.rs`), plus the control
pool, plus pg_dump/reaper/advisory-lock sessions.

**Bites at.** 200 ÷ 5 ≈ 40 pools saturated, minus control-plane and
operational headroom → **roughly 35 tenants running full-tilt
simultaneously** (interactive burst + pipeline work each). Idle tenants
cost ~0 connections (pool idle timeout 5 min), so this is a concurrency
ceiling, not a tenant-count ceiling — but a burst (launch spike, a popular
share) hits it as connection-acquire timeouts.

**Signal.** `pg_stat_activity` count vs 200; sqlx acquire-timeout errors in
pod logs.

**Remediation ladder.** (a) Lower per-tenant pool to 3 — most tenant work
is serial. (b) Raise `max_connections` with the droplet's RAM (each
connection ~5–10MB worst case). (c) Split the *data plane* onto pgbouncer
while keeping advisory-lock paths (provision/backup/delete/reaper) on
direct connections — the constraint is per-path, not global. (d) Second
cluster (the `cluster_id` column exists for exactly this).

## 2. Dispatcher slow scan × AccountCache (the cross-tenant ledger scan)

**Shape.** The fast path is fine: ticks every 2s but polls only *hinted*
tenants (`dispatch_hints`), so it scales with active mutation, not tenant
count. The problem is the **slow scan**: every `slow_scan_interval` (300s)
a tick polls **every active account** — the recovery bound for lost hints
and the only driver for purely time-based work (cron reports, feed polls)
on tenants nobody is touching (`dispatcher.rs`, `SCAN_CONCURRENCY = 16`).
Two compounding effects: each poll goes through
`AccountCache::get_for_dispatch`, so the scan **faults every tenant into
the cache**, and at `idle_ttl = 15 min` vs scans every 5 min, **no tenant
is ever idle-evicted** — the cache converges on all-tenants-resident.

**Bites at.** Gradually: N × (pool fault + ledger queries) per 300s is
~3 polls/s at 1000 tenants — fine as query load, but cache memory grows
with N (a resident entry holds core + pools + decrypted provider config)
until the `max_entries = 1000` cap, after which the scan **thrashes** the
cache every 5 minutes (evict → refault → evict). Call it: memory pressure
in the **hundreds**, thrash at **~1000**.

**Signal.** Tick-duration logs (`dispatcher` tick summary), RSS of the pod,
`pg_stat_statements` calls on the ledger-poll query shape climbing with N.

**Remediation ladder.** (a) Stretch `slow_scan_interval` (it's a CLI flag;
15–30 min is fine — hints cover the interactive path). (b) Move
time-driven work into the control plane: a `next_run_at` column per
(account, schedule) written at schedule-save time, so cron/feed due-ness
becomes one indexed control-plane query and the full scan exists only for
lost-hint recovery. (c) Peek ledgers without faulting the full tenant
handle (a bare one-shot connection, no cache entry). (d) The outbox/LISTEN-
NOTIFY pattern the plan deferred "until N+1 hurts" — this section is the
definition of "hurts."

## 3. Per-request control-plane auth

**Shape.** Decision 2026-06-10: no auth caching in v1 — every API request
does token-hash + account-row lookups against the control database
(CloudAuth). O(RPS), not O(tenants), and all of it lands on one hot table.

**Bites at.** Not soon at PKM request rates; becomes the top row of
`pg_stat_statements` by call count long before it's a latency problem. The
risk is coupling: a control-DB hiccup becomes every tenant's 500s.

**Signal.** It is already the #1 query by calls in `pg_stat_statements`;
watch its mean, not its count.

**Remediation ladder.** (a) Nothing, for a long time (it's one indexed
lookup). (b) Short-TTL (30–60s) in-process auth cache, accepting a bounded
revocation delay — document the delay in the token-revoke UX before
building it.

## 4. Deploy-time fleet migration

**Shape.** Every deploy boots in migrating mode and walks all N tenant
databases before `/ready` flips (deploy-gating policy: >30 min wall time =
timeout). O(N × migration cost) on every single deploy, even no-op ones
(a no-op `initialize()` still connects and version-checks each DB).

**Bites at.** At ~1s per no-op tenant check, 1000 tenants ≈ 17 min serial —
inside the window but uncomfortable; a real DDL migration across big
tenants blows it. We deploy many times per day; this is the first concern
that taxes *velocity* rather than capacity.

**Signal.** `run_fleet_gate` duration in boot logs — record it per deploy
starting now (it's in the journal; nobody looks yet).

**Remediation ladder.** (a) Concurrency in the fleet runner (advisory locks
already make it safe). (b) Version-stamp short-circuit: skip connecting to
tenants whose `last_migrated_version` already equals the target (the
mapping row exists; trust it, verify on drift). (c) Lazy migration — the
straggler path (503 `account_upgrading` + reaper retry) already works;
flip the default so deploys gate only on the control plane + a canary
subset, and the fleet migrates in the background. That converts deploy
time from O(N) to O(1) and is the eventual end state.

## 5. Disk: sold storage vs the volume (the hard wall)

**Shape.** Every tenant database lives on one encrypted DO volume. Pro
sells 10 GB/tenant; the volume is fixed-size. Overcommit is correct
(usage ≪ limits) but unmanaged overcommit on a single volume ends in
`ENOSPC`, and Postgres on a full disk is an incident, not a degradation.
Export artifacts (up to a tenant's full size, ≤24h retention) and the
migration-ingress staging share the same disk.

**Bites at.** `sum(actual tenant bytes) + WAL + exports` approaching the
volume. With today's tenants this is years away; with one viral thread it
is not. It is the only concern on this list that ends in data-loss-shaped
downtime rather than slowness.

**Signal.** `df` on the volume (alert at 70/85%), the storage rollup the
quota system already computes per tenant, DO volume metrics.

**Remediation ladder.** (a) DO volumes resize online — but only if someone
is watching; this is a monitoring item more than an engineering one.
(b) Move export staging off the data volume. (c) Per-plan storage
enforcement already exists (restricted state) — verify the rollup job's
cadence keeps overshoot bounded. (d) Second cluster / volume-per-shard.

## 6. Backup throughput & IO contention

**Shape.** Due-driven since 2026-07-12: every 5-min tick dumps tenants
past the 24h cadence, ≤ `max_backups_per_pass = 256` per pass, serial
`pg_dump -Fc` per tenant with a kill-budget timeout. Throughput ceiling:
one pass at a time, so ~N_daily dumps must fit in 288 ticks/day of serial
dump time; and pg_dump competes with live queries for CPU/IO on the same
box.

**Bites at.** Thousands of small tenants (fine) or dozens of *large* ones
(pg_dump minutes each, all sharing the box's IO). The due-driven design
self-spreads across the day (each tenant re-dumps ~24h after its last),
which is the saving grace.

**Signal.** `backup_runs` ledger (pass duration = finished_at −
started_at), tenant dump timeouts in logs, query-latency correlation with
pass times in `pg_stat_statements`.

**Remediation ladder.** (a) Lower per-pass cap so passes interleave with
serving (the due-driven loop makes this safe — deferred tenants are due
next tick). (b) `nice`/`ionice` the dump. (c) Dump from a replica
(requires the replica — see #8). (d) PITR/WAL archiving for the largest
tenants, already the plan's deferred item.

## 7. Postgres catalogs & autovacuum across N databases

**Shape.** Database-per-tenant means N × system catalogs, N × autovacuum
scheduling (workers cycle databases at `autovacuum_naptime` granularity),
N × stats. This is the known cost of the isolation model — the cluster
does more bookkeeping per tenant than schema-per-tenant would.

**Bites at.** Community experience says catalogs get noticeable in the
low thousands of databases and painful past ~5–10k; autovacuum starvation
(N databases ÷ naptime > worker throughput) can lag bloated tenants
earlier. Not a launch-year problem at current growth.

**Signal.** Autovacuum age/last-run per DB (queryable), catalog cache
memory in the postgres container, connection-establishment latency drift.

**Remediation ladder.** (a) Tune `autovacuum_naptime`/workers as N grows.
(b) Accept until sharding: the `cluster_id` column makes "new signups land
on cluster 2" a provisioning-time switch, and per-tenant restore/migrate
means rebalancing is per-tenant `pg_dump`/restore, not surgery.

## 8. One box: shared CPU, RAM, and fate

**Shape.** Embedding chunking, LLM streaming, zip exports, pg_dump,
Postgres, and Caddy share one droplet's cores and memory
(`shared_buffers=1GB`, `effective_cache_size=3GB` sizing). One tenant's
bulk import is every tenant's noisy neighbor (worker-pool caps bound the
*count* of concurrent work, not its CPU weight). And availability is
all-or-nothing: kernel panic, DO host maintenance, or a bad deploy takes
every tenant down together.

**Bites at.** Load-dependent, not count-dependent. The worker pools + plan
allowances keep AI work bounded; the uncapped shapes are import/export and
dump IO.

**Signal.** Droplet CPU/load/RAM (the missing Grafana item — every row of
this table keeps landing on the same remediation), p95 request latency.

**Remediation ladder.** (a) Bigger droplet — vertical headroom is cheap
and instant, take it before anything structural. (b) Move the pod off the
Postgres box (two droplets: compute vs data) — the compose file already
separates the services; this is mostly a Caddy/network change. (c) Second
pod (see #9), then replicas.

## 9. Multi-pod blockers

**Shape.** Nearly everything is already cross-pod safe (advisory locks,
claim-based sweeps, idempotent transitions — designed in from day one).
The known blocker: **WebSocket event delivery is per-pod in-memory**
(decision 2026-06-12) — pod A's pipeline events never reach a client
connected to pod B. Second order: per-pod rate limiters and chat-stream
caps become per-pod × N_pods, and the AccountCache duplicates residency
per pod (halving the effective per-pod thrash threshold in #2).

**Bites at.** The day we want pod #2 — which is also the remediation for
half this document, so it's a gate on the *escape hatch*, not on current
operation.

**Remediation ladder.** Postgres LISTEN/NOTIFY relay for the event
channels (the plan's named design), then revisit limiter scoping. Build it
*before* the droplet forces the move, not during the incident that does.

## 10. Observability cardinality

**Shape.** `pg_stat_statements` tracks per (userid, dbid, queryid): the
same ~50 app query shapes appear once *per tenant database*.
`pg_stat_statements.max = 10000` → silent LRU eviction of exactly the
rare-slow entries we want, at roughly **10000 ÷ 50 ≈ 200 databases**.

**Signal.** `pg_stat_statements_info.dealloc` counter climbing.

**Remediation ladder.** (a) Raise `max` (memory is ~few KB/entry).
(b) Aggregate by `queryid` across `dbid` in whatever dashboard reads it
(the per-tenant split is usually noise; the per-query shape is the
signal). (c) A real metrics pipeline (the Grafana item, again).

## Standing follow-ups

- **Measure, don't estimate**: record fleet-migration wall time per deploy
  and backup-pass durations now, while N is small — the trend line is the
  early warning this doc can't compute from constants.
- **The recurring remediation is monitoring.** Six of ten concerns list
  "watch X" as the first step and we currently watch nothing
  automatically. The Grafana Cloud + uptime item from the launch list is
  the prerequisite for operating this document rather than re-reading it
  after incidents.
- Revisit this doc at every ~10× tenant milestone (10 → 100 → 1000) and on
  every topology change (second pod, second cluster, pgbouncer).
