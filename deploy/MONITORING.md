# Monitoring — Grafana Cloud wiring for the single-box stack

The scaling doc (`docs/plans/atomic-cloud-scaling.md`) lists ten concerns;
six of them name a "watch X" signal as the first remediation, and launch
week produced three faults nobody saw until a human went looking (the
silent backup stall, embedding upstream latency, the export disk gap).
This runbook turns the code side (the pod's internal `/metrics` listener,
the compose `monitoring` profile) into dashboards and alerts.

Everything here is opt-in and doubly gated: without
`COMPOSE_PROFILES=monitoring` the monitoring containers never start, and
without `ATOMIC_CLOUD_METRICS_BIND` the pod never binds its metrics
listener. A deployment that skips this file behaves exactly as before.

## Security model (read once)

`/metrics` is served by a **separate internal HTTP listener** inside the
pod, NOT a route on the public listener. It is deliberately not behind
CloudAuth on the tenant/app hosts — it isn't on those hosts at all, so no
route-table or auth mistake can expose it. The port is `expose:`d on the
compose network only (never `ports:`, never in the Caddyfile). The only
consumer is the `grafana-alloy` container on the same bridge network.

## Operator steps

1. **Create a Grafana Cloud stack** (free tier is ample at this scale):
   grafana.com → create account → create stack.
2. **Collect the OTLP gateway credentials**: stack → *Send Metrics* →
   OTLP card → note the gateway URL (the base ending in `/otlp`), the
   stack/instance id (the basic-auth username), and create an
   access-policy token with `metrics:write`. (Alloy converts the
   Prometheus scrapes to OTLP in-process; same stored metrics, and the
   OTLP card's credentials are the ones the integration flow hands out.)
3. **Fill `deploy/.env`** (template comments in `.env.example`):

   ```
   COMPOSE_PROFILES=monitoring
   ATOMIC_CLOUD_METRICS_BIND=0.0.0.0:9464
   GRAFANA_CLOUD_OTLP_URL=https://otlp-gateway-<region>.grafana.net/otlp
   GRAFANA_CLOUD_OTLP_USER=<stack id>
   GRAFANA_CLOUD_OTLP_TOKEN=<glc_… token>
   ```

4. **Redeploy**: `docker compose up -d` from `deploy/` (or the usual
   `scripts/deploy.sh`). Verify:

   ```
   docker compose exec grafana-alloy sh -c 'wget -qO- atomic-cloud:9464/metrics | head'
   docker compose logs grafana-alloy | tail          # no remote_write errors
   ```

   Then confirm series arrive in Grafana (Explore →
   `atomic_cloud_uptime_seconds`).

## First alerts (in this order)

These map 1:1 onto the scaling doc's signal column; create them before any
dashboards — alerts are the point.

0. **FD exhaustion** (the 2026-07-16 outage: a socket leak ate Docker's
   default 1024 soft limit in ~28h; at the limit `accept()` fails and the
   pod serves nothing while looking "up"):
   `atomic_cloud_process_open_fds / atomic_cloud_process_fd_limit > 0.5`.
   The compose file now sets nofile to 65536, so at the historical leak
   rate this alert fires WEEKS before impact. If it fires: snapshot
   `/proc/<pid>/fd` counts and classify sockets against `/proc/<pid>/net/*`
   BEFORE restarting — the evidence does not survive the restart.

1. **Backup staleness** (doc #6; the launch-week placebo lesson):
   `atomic_cloud_backup_last_success_age_seconds > 129600` (36h).
   The gauge is `+Inf` until the first successful pass after boot, so this
   alert also catches a pod whose backup loop never ran at all.
   Belt-and-braces: `atomic_cloud_backup_stale_tenants > 0` catches the
   clean-but-skipping pass (a due-filter bug backs up nobody yet still
   advances the success stamp), and
   `atomic_cloud_backup_staleness_check_age_seconds > 3600` catches the
   checker itself breaking — stale_tenants freezes at its last value when
   the check errors (the loop only logs, and logs are not shipped), while
   this age keeps growing. The check runs every backup tick (5 min), so
   an hour means twelve consecutive failures, not jitter.
2. **Disk** (doc #5 — the hard wall):
   `node_filesystem_avail_bytes / node_filesystem_size_bytes < 0.30` on the
   data volume mount (warn), `< 0.15` (page). DO volumes resize online —
   but only if someone is watching.
3. **/health synthetic**: a Grafana Cloud synthetic-monitoring HTTP check
   against `https://app.<base-domain>/health`. This is the only check that
   catches "the box is off" — every other signal is the droplet reporting
   on itself. Provisioned 2026-07-13: job `atomic-cloud-health`, Ohio +
   Zurich probes, 2-minute frequency (keeps the free execution budget at
   ~43k/month of ~100k), paired with the "Health endpoint unreachable"
   alert rule on `avg(probe_success) < 0.5` with noDataState=Alerting so
   Synthetic Monitoring itself breaking also fires. Note: the SM app needs
   one-time initialization in the UI before its API accepts checks, and
   modern stacks reject the legacy `alertSensitivity` field — alert via a
   normal rule over `probe_success` instead.
4. **Connection budget** (doc #1):
   `sum(pg_stat_activity_count) > 160` (80% of max_connections=200).
   The `sum()` is load-bearing: postgres-exporter emits one series per
   (datname, state), and with database-per-tenant the budget is spread
   across many small series — the cluster can sit at 195/200 while no
   single series comes near the threshold. Confirm against
   `atomic_cloud_tenant_pool_connections` +
   `atomic_cloud_control_pool_connections` to see whether it's the pod.
5. **Dispatcher liveness and scan time** (doc #2):
   `atomic_cloud_dispatcher_last_tick_age_seconds > 300` — the tick loop
   is a spawned task; a panic kills it silently while its duration gauges
   freeze at their last healthy value, so only this age (which is `+Inf`
   until the first tick, catching died-before-first-tick too) can see the
   loop stop. Ticks run every ~2s; five minutes is unambiguous death.
   `atomic_cloud_dispatcher_last_full_scan_age_seconds > 3600` — a
   dispatcher whose candidate scan errors every tick still ticks fast;
   only a COMPLETED full scan (every ~15 min) advances this stamp, so an
   hour means the scan itself is broken.
   `atomic_cloud_dispatcher_last_full_scan_seconds > 30` — the slow scan
   outgrowing its budget is the "structural fix warranted" tripwire.
   (A pod running `--dispatcher=false` keeps the age gauges at `+Inf` by
   design — scope these two alerts to the dispatcher-enabled pod.)

## First dashboard panels

- Backup: last-success age, stale tenants, last-pass backed-up/failed.
- Cache & pools: `atomic_cloud_account_cache_entries` by kind, evictions
  rate, `atomic_cloud_worker_pool_in_flight` vs `…_cap` per class.
- Dispatcher: last tick / full-scan durations, executed vs deferred rates
  (`rate(atomic_cloud_dispatcher_jobs_executed_total[5m])`).
- Host: CPU, RAM, disk on the data volume (doc #8's "the missing Grafana
  item").
- Postgres: connections vs max, per-database sizes (sums to the sold-GB
  overcommit picture, doc #5), autovacuum age (doc #7).

## What is deliberately absent

- **Request-path metrics** (per-route latency histograms): a later item;
  they cost hot-path work per request. Today's families cover the
  background/ops faults that were invisible during launch week.
- **pg_stat_statements dashboards** (doc #10): still read via psql; the
  postgres-exporter families here don't include per-query stats. When
  `pg_stat_statements_info.dealloc` starts climbing, raise
  `pg_stat_statements.max` (compose command) before trusting the view.
- **Logs**: not shipped. `docker compose logs` + journald remain the log
  surface; add Alloy's loki components later if grepping the box gets old.
