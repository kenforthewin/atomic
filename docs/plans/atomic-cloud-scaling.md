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

**Signal.** `sum(pg_stat_activity_count)` vs 200 via postgres-exporter
(the `sum()` is load-bearing — the metric is per-(datname, state), and
database-per-tenant spreads the budget so no single series ever nears the
ceiling); `atomic_cloud_tenant_pool_connections` + control-pool gauges on
the pod's /metrics; sqlx acquire-timeout errors in pod logs. Alert #4 in
deploy/MONITORING.md.

**Remediation ladder.** (a) Lower per-tenant pool to 3 — most tenant work
is serial. (b) Raise `max_connections` with the droplet's RAM (each
connection ~5–10MB worst case). (c) Split the *data plane* onto pgbouncer
while keeping advisory-lock paths (provision/backup/delete/reaper) on
direct connections — the constraint is per-path, not global. (d) Second
cluster (the `cluster_id` column exists for exactly this).

## 2. Dispatcher slow scan × AccountCache (the cross-tenant ledger scan)

**Shape.** The fast path is fine: ticks every 2s but polls only *hinted*
tenants (`dispatch_hints`), so it scales with active mutation, not tenant
count. The load driver is the **slow scan**: every `slow_scan_interval`
(default 900s since 2026-07; was 300s) a tick polls **every active
account** — the recovery bound for lost hints and the only driver for
purely time-based work (cron reports, feed polls) on tenants nobody is
touching (`dispatcher.rs`, `SCAN_CONCURRENCY = 16`). Each poll goes
through `AccountCache::get_for_dispatch`, so the scan **faults every
tenant into the cache**. The second compounding effect — scans-as-touches
defeating idle eviction, converging on all-tenants-resident — is fixed in
two layers. Dispatch reads never renew the serving idle TTL
(`get_for_dispatch` is a no-touch hit), and the entries a dispatch *miss*
builds start as **background faults** on a short TTL of their own
(`AccountCacheConfig::background_idle_ttl`, default 60s; the periodic
sweep runs at least that often), so a scan's faults drain by the next
sweep no matter how `slow_scan_interval` compares to `idle_ttl` — the two
knobs are independent, with no ordering constraint to tune or validate.
What promotes an entry to the full serving TTL (`AccountCache::touch`) is
evidence the tenant isn't idle: a poll that finds **live ledger state**
(pending rows, due or backed off — either way the hint keeps the fast
path faulting the handle every tick, so eviction would reclaim nothing),
and a claim that actually **executed**, on settle. Net: a scan-only
tenant is resident for ~a background TTL per interval; a tenant with
flowing — or deferred, waiting out a provider Retry-After or exponential
backoff — work rides the window on one entry instead of an evict/refault
cycle per TTL. Pinned by
`tests/account_cache.rs::dispatch_hits_do_not_renew_idle_ttl_but_serving_hits_do`,
`tests/account_cache.rs::background_faults_live_on_short_ttl_until_promoted`,
`tests/dispatcher.rs::scan_evicts_idle_tenant_but_executed_work_keeps_tenant_resident`,
and `tests/dispatcher.rs::deferred_ledger_rows_keep_hinted_tenant_resident_across_ttl`.

**Bites at.** Gradually: N × (pool fault + ledger queries) per 900s is
~1 poll/s at 1000 tenants — fine as query load. The scan still faults
every tenant once per interval, so a per-interval rebuild pass (pool open
+ provider decrypt per tenant per 900s) remains; but residency is now a
short pulse (out by the first sweep past the 60s background TTL) instead
of a floor, and the hard-cap pass evicts by idle *deadline*, so scan
artifacts are sacrificed before serving entries when the cap bites. Call
it: rebuild churn noticeable in the **high hundreds**, structural fix
warranted at **~1000+**.

**Signal.** Exported live since 2026-07-13:
`atomic_cloud_dispatcher_last_full_scan_seconds` /
`_last_full_scan_tenants` (scan cost growing with N),
`atomic_cloud_account_cache_entries{kind}` (serving vs background-fault
residency), `_account_cache_evictions_total`, plus
`_dispatcher_last_tick_age_seconds` / `_last_full_scan_age_seconds` for
loop liveness (+Inf until first event — a dead dispatcher reads as
unbounded age, never as frozen-healthy). Alert #5 in deploy/MONITORING.md.
Cross-check: `pg_stat_statements` calls on the ledger-poll query shape.

**Remediation ladder.** (a) ~~Stretch `slow_scan_interval`~~ — applied:
default 900s; the cost is a 15-minute worst-case pickup for cron/feed work
on unhinted tenants, documented on the `--dispatcher-slow-scan-secs` flag
and `DispatcherConfig::slow_scan_interval`. The flag is now safe to tune
in either direction — scan residency is bounded by the background TTL,
not the idle TTL, so tightening it back toward 300s costs query load and
rebuild churn, never fleet-wide cache residency. (b) Move
time-driven work into the control plane: a `next_run_at` column per
(account, schedule) written at schedule-save time, so cron/feed due-ness
becomes one indexed control-plane query and the full scan exists only for
lost-hint recovery. (c) Peek ledgers without faulting the full tenant
handle (a bare one-shot connection, no cache entry) — rejected for now:
per-scan connection open/close per tenant trades cache memory for cluster
connection churn, which concern #1 says is the scarcer resource. (d) The
outbox/LISTEN-NOTIFY pattern the plan deferred "until N+1 hurts" — this
section is the definition of "hurts."

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

**Shape.** Every deploy boots in migrating mode and walks the *lagging*
tenant databases before `/ready` flips (deploy-gating policy: >30 min wall
time = timeout). Since the 2026-07-13 stamp short-circuit work, tenants
whose `account_databases.last_migrated_version` already equals the
compiled target are never connected to — a no-op deploy's fleet gate is
one control-plane query, O(changed tenants), and the run reports
`skipped_current` (in logs, the `deploy_runs` ledger via migration 022,
and `deploy status`, disambiguating "fleet all current" from "gate saw no
fleet"). The trust chain is documented on `count_skipped_current`: stamps
are written only post-migration, and the one writer that could lie — the
restore runbook, which used to stamp the compiled target onto a
possibly-older dump — now stamps the restored database's own
`schema_version`. E2e-pinned by
`tests/e2e_deploy.rs::fully_stamped_fleet_short_circuits_without_tenant_connections`
(tenant DBs dropped before a fully-stamped run; only a true skip stays
green).

**Bites at.** No-op deploys no longer scale with N. What remains O(N) is a
deploy that actually raises the schema target — every tenant is then
lagging by definition and must be walked (~1s per no-op check, more for
real DDL; 1000 tenants ≈ 17+ min serial, against the 30-min policy
window).

**Signal.** `run_fleet_gate` duration in boot logs and
`skipped_current`/total in `deploy status` — record wall time per
schema-raising deploy starting now.

**Remediation ladder.** (a) ~~Version-stamp short-circuit~~ — applied (see
Shape). (b) Concurrency in the fleet runner for schema-raising deploys
(advisory locks already make it safe). (c) Lazy migration — the straggler
path (503 `account_upgrading` + reaper retry) already works; flip the
default so deploys gate only on the control plane + a canary subset, and
the fleet migrates in the background. That converts even DDL deploys from
O(N) to O(1) and is the eventual end state.

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

**Signal.** Host disk gauges via the monitoring profile's node exporter
(alert at 70/85% — recipe #3 in deploy/MONITORING.md),
`atomic_cloud_export_jobs_active` (artifacts share the volume), the
storage rollup the quota system already computes per tenant, DO volume
metrics.

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

**Signal.** `atomic_cloud_backup_last_success_age_seconds` (+Inf until
the first clean pass — a loop dead from boot is alertable at first
scrape), `_backup_stale_tenants`, `_backup_staleness_check_age_seconds`
(the checker watching the checker); alert #1 in deploy/MONITORING.md.
Also the `backup_runs` ledger (pass duration = finished_at −
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

**Signal.** Host CPU/load/RAM/disk via the monitoring profile's node
exporter; `atomic_cloud_worker_pool_in_flight{class}` vs `_cap` for
work-side pressure. Request-path latency histograms are deliberately not
exported yet (noted in `metrics.rs`); p95 latency remains a gap until
that or an edge-side measurement exists.

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
- **The recurring remediation is monitoring — code side shipped
  2026-07-13.** The pod exports 22 `atomic_cloud_*` families on an
  internal-only /metrics listener, and the compose `monitoring` profile
  (grafana-alloy + postgres-exporter + host metrics) ships them to Grafana
  Cloud; `deploy/MONITORING.md` is the runbook, including the first five
  alerts keyed to this document's signal columns. The remaining step is
  operator-side: create the Grafana Cloud stack, fill the three env vars,
  enable the profile. Until then the gauges exist but nothing watches
  them.
- Revisit this doc at every ~10× tenant milestone (10 → 100 → 1000) and on
  every topology change (second pod, second cluster, pgbouncer).
