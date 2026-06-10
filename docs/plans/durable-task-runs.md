# Durable Task Runs

## Status (2026-06-09)

Phase 1 is **landed**: the `task_runs` table exists in both SQLite and
Postgres storage, and `scheduler::ledger` provides `claim_or_create` with
durable leases, heartbeating, and crash reclaim (race-tested). Reports
already dispatch through the ledger (the "phase 1.5" runner in
`atomic-server/src/main.rs`).

Phase 2 is **landed**: `draft_pipeline` and `graph_maintenance` dispatch
through the ledger via `scheduler::runner` (claim-and-record, extracted
from the 15s tick so tests can drive ticks directly). The in-memory
registry lock is demoted to a fast-path; `last_run` advances only on
terminal success; failures back off via `next_attempt_at` — the
retry-storm bug below is fixed. Known limitation surfaced by the
multi-DB e2e: the Postgres backend's `settings` table is global across
logical databases (no `db_id` scoping), so the `task.{id}.*` fast-path
keys are shared between DBs on Postgres. The ledger itself *is*
per-database on both backends; scoping PG settings is its own
follow-up work item.

Phase 3 is **landed**: every feed poll dispatches through the ledger as a
`task_id = "feed_poll"` run with `subject_id = <feed id>`
(`ingest::poller`, the sweep body the 60s loop drives; manual polls ride
the same path with `trigger = manual`). `feeds.last_polled_at` /
`last_error` are demoted to the fast-path cache: `last_polled_at`
advances only when a poll settles (success, or abandonment after the
retry budget — abandonment parks a persistently broken feed until its
next interval), so retryable failures leave the feed due and the row's
`next_attempt_at` drives backed-off retries. The hot due-feeds query is
unchanged.

Outstanding: phases 4–5 below, plus **wiki regen** (added to scope by
`docs/plans/atomic-cloud.md` — currently fire-and-forget on tag change).
Note `daily_briefing` no longer exists as a scheduled task — it collapsed
into a seeded report (see `reports-phase-3-briefing-collapse.md`).

This workstream is a prerequisite for Atomic Cloud's dispatcher, which
relies on `task_runs` being the single source of pending background work.
It should land and ride to production in self-hosted first.

## Context

Atomic has three half-overlapping patterns for background work:

1. **Scheduled tasks** (`daily_briefing`, `draft_pipeline`, `graph_maintenance`) — stateless cron ticks. Only persisted state is `task.{id}.last_run` in the per-DB `settings` table plus an in-memory `(task_id, db_id)` lock (`scheduler::TaskRegistry`). No run history, no attempt tracking, no backoff. On failure `last_run` is *not* advanced, so a failing task retries **every 15s tick forever with no backoff** — a real latent bug.
2. **Feed polling** — per-row state on the `feeds` table (`last_polled_at`, `last_error`). Independently scheduled per feed.
3. **`atom_pipeline_jobs`** (db.rs V13→V14) — already a durable job queue: `state` / `lease_until` / `attempts` / `not_before` / `last_error`. This is the durable idiom we want, but it's only applied to per-atom embed/tag work.

This plan introduces a single **occurrence-keyed run ledger**, `task_runs`, that unifies background-work execution under the durable idiom `atom_pipeline_jobs` already established — *without* changing `atom_pipeline_jobs` and *without* moving any trigger/definition logic.

### Conceptual model (Temporal vocabulary, for shared language)

- **Definition** = code + config. The `ScheduledTask` impls, trigger/scope settings, the `feeds` table rows. Never a ledger row. Unchanged by this plan.
- **`atom_pipeline_jobs`** = an *entity execution*: subject-keyed (`atom_id`), coalescing, mutated in place, converges to a desired state. Unchanged.
- **`task_runs`** = a *per-invocation execution ledger*: occurrence-keyed (uuid), one row per firing, history preserved.
- **No activity layer** — the internal steps of a task (LLM call, atom write) are deliberately *not* separately persisted. `task_runs.attempts` is task-level retry, not step-level replay. This is an intentional non-goal (see below).

## Goals

- One durable ledger for all per-invocation background work: system tasks now, feed polls next, automations/recipes later.
- Give the three existing system tasks retry, exponential backoff, and visible run history — fixing the silent-failure / retry-storm bug for free.
- Survive process restart (durable lease, crash recovery of stuck `running` rows).
- Bounded growth via a retention/GC task, with conservative defaults tuned for the unattended single-user desktop path.

## Non-goals

- **No changes to `atom_pipeline_jobs`.** It is correctly subject-keyed and coalescing; merging it into `task_runs` would destroy a property each table needs. They share a *vocabulary*, not a table.
- **No activity / step-replay layer.** The expensive step is a non-deterministic LLM call that replay can't help anyway; grain stops at "the run."
- **No automation/recipe builder.** The trigger→scope→action model and `automations` definition table are a follow-up. This plan only builds the ledger + retrofit + GC so that follow-up is a small addition, not a new subsystem.
- **No `VACUUM` / file-reclamation policy.** Flagged as an open question, deliberately out of scope.

## Schema

New per-DB table (data databases only — **never `registry.db`**, per the multi-DB gotcha in CLAUDE.md). Column vocabulary deliberately mirrors `atom_pipeline_jobs` so the two read as the same idiom.

```sql
CREATE TABLE IF NOT EXISTS task_runs (
    id              TEXT PRIMARY KEY,          -- uuid
    task_id         TEXT NOT NULL,             -- "daily_briefing", "feed_poll", or an automation id
    subject_id      TEXT,                      -- NULL for singleton system tasks; feed id for feed polls
    state           TEXT NOT NULL DEFAULT 'pending',
                       -- pending | running | succeeded | failed | abandoned
    trigger         TEXT NOT NULL,             -- "schedule" | "manual" | "threshold" | "event:..."
    attempts        INTEGER NOT NULL DEFAULT 0,
    max_attempts    INTEGER NOT NULL DEFAULT 3,
    lease_until     TEXT,                      -- durable lock; canonical (in-memory lock is a fast-path)
    next_attempt_at TEXT NOT NULL,             -- the not_before analog; backoff lives here
    scope           TEXT,                      -- resolved scope snapshot (e.g. tag id + atom count)
    result_id       TEXT,                      -- artifact produced (briefing / atom / wiki id)
    last_error      TEXT,
    started_at      TEXT,
    finished_at     TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- Find runnable rows cheaply (pending/retryable whose next_attempt_at has passed).
CREATE INDEX IF NOT EXISTS idx_task_runs_claim
    ON task_runs(state, next_attempt_at);
-- Reclaim crashed leases.
CREATE INDEX IF NOT EXISTS idx_task_runs_lease
    ON task_runs(state, lease_until);
-- History queries + retention scans, per (task, subject).
CREATE INDEX IF NOT EXISTS idx_task_runs_history
    ON task_runs(task_id, subject_id, created_at);
```

State machine:

- `pending` → `running` (claim: set `lease_until`, `started_at`, `state='running'`)
- `running` → `succeeded` (terminal; set `result_id`, `finished_at`; advance the definition's `last_run` fast-path)
- `running` → `failed` then either:
  - `attempts < max_attempts` → back to `pending`, `attempts += 1`, `next_attempt_at = now + backoff(attempts)`
  - `attempts >= max_attempts` → `abandoned` (terminal)
- Crash recovery: a `running` row with `lease_until < now` is reclaimable — treated as `pending` by the claim query (same shape as `atom_pipeline_jobs`'s lease index).

## Migration

New migration block `V17 → V18` in `crates/atomic-core/src/db.rs`, mirroring the V13→V14 `atom_pipeline_jobs` block.

**Gotcha:** the convention is that *only the final block* ends with `PRAGMA user_version = {Self::LATEST_VERSION}`. Currently that's the V16→V17 block (db.rs:861). Adding V18 requires two coordinated edits:

1. Bump `LATEST_VERSION` (db.rs:214) from `17` to `18`.
2. Change the V16→V17 block's final pragma (db.rs:861) from `{Self::LATEST_VERSION}` back to a **literal `17`**, since it is no longer the last block.
3. Add the new `if version < 18 { ... PRAGMA user_version = {Self::LATEST_VERSION}; }` block.

Data-DB only: `task_runs` holds no rows on a fresh DB; the "per-DB settings table starts empty" invariant is unaffected (we add no seed settings rows in the migration itself — see retention config below for defaults handled at read time).

## Integration points

### Scheduler loop (`crates/atomic-server/src/main.rs` ~447–506)

The 15s tick and per-DB fan-out (`manager.list_databases()` → `get_core(db_id)`) stay. The change is **spawn-and-forget → claim-and-record**:

- For each due task (existing `is_due()` predicate unchanged), instead of `spawn(task.run())` and forgetting: insert a `pending` `task_runs` row (or reclaim an existing retryable one), claim it (set lease), run it, then transition the row terminally.
- `lease_until` becomes the source of truth for "is this already running." The in-memory `(task_id, db_id)` lock in `TaskRegistry` demotes to a cheap optimization that avoids a DB round-trip every tick — it is no longer correctness-critical (and it didn't survive restart anyway).
- Backoff falls out of `next_attempt_at`: the claim query only returns rows whose `next_attempt_at <= now`, so a failing task naturally throttles instead of retrying every 15s.

The new claim/transition helpers belong in a `scheduler::runs` module alongside `scheduler::state` (which keeps owning the `task.{id}.*` definition/fast-path keys — **not moved**).

### Definition fast-path (unchanged rule)

`task.{id}.last_run` stays in the per-DB `settings` table, advanced **only on terminal success**. The hot `is_due` check (N tasks × N DBs every 15s) must not query `task_runs`. The ledger is the durable history + retry driver; `last_run` is the cheap "when did this last succeed" pointer. This is the same rule already applied to scheduled tasks — not a new pattern.

### Feed polling fold-in

- `feeds` stays as the **definition** table (url, interval, paused, title). Unchanged role.
- Each poll becomes a `task_runs` row with `task_id = "feed_poll"`, `subject_id = <feed id>`. The `subject_id` column exists precisely for this per-feed grain.
- `feeds.last_polled_at` / `feeds.last_error` demote to a **denormalized fast-path cache** on the definition row (exactly like `task.{id}.last_run`). The hot "which feeds are due" query keeps using the existing `idx_feeds_last_polled` index — no change to that path.
- **Why cleanup is safe:** because live scheduling state lives on the definition tables (`feeds`, settings), deleting old `task_runs` rows only discards *history*, never live state. The denormalized fast-path decision is what makes retention safe.

## Retention / cleanup

`task_runs` is append-per-invocation. With a 15s tick over 3 system tasks + N feeds (and later automations), unbounded growth is real — and the desktop path has no ops, a single user, potentially very long uptime, and a SQLite file on the user's disk. Cleanup is itself a `ScheduledTask` (`task_runs_gc`), dogfooding the same loop and getting its own bounded run history.

### Policy

- **Never delete non-terminal rows.** `pending`, `running`, and failed-but-retryable (`state='pending'` with `attempts < max_attempts`) rows are live execution state.
- **Per `(task_id, subject_id)`, keep the most recent `K` terminal rows.** Default `K = 50`. Per-subject keying means each feed retains its own recent history while the total stays bounded.
- **Always retain the most recent terminal *failure* per `(task_id, subject_id)`**, regardless of age, up to `retain_failed_days` (default 90). "Why did this feed stop syncing?" must remain answerable; failures are rare and high-value, successes are noise.
- **Hard age cap:** terminal rows older than `retain_days` (default 30) are eligible for deletion even within the last-`K` window, except the retained-failure above.

### Desktop-safe execution

- **Batched deletes.** SQLite is single-writer; never delete a large backlog in one statement holding the write lock while the user edits atoms. Delete in bounded batches (e.g. 500 rows) with the GC task yielding between batches.
- **Low frequency.** `task_runs_gc` runs hourly (default `interval_minutes = 60`), not on the 15s cadence.
- **Per-DB.** Like every background job here, GC iterates per data DB via the standard loop; `task_runs` is per-DB so GC is too.
- File-space reclamation (`VACUUM` / `incremental_vacuum` / WAL checkpoint) is **out of scope** — see open questions. Row payloads are small and the retention caps keep practical file growth modest; this can be a later refinement.

### Configurable defaults

Retention knobs follow the existing `task.{id}.*` settings convention so server/power-user deployments can raise them without code changes. Read with sane fallbacks (no migration seed rows, preserving the empty-settings invariant):

- `task.task_runs_gc.enabled` (default `true`)
- `task.task_runs_gc.interval_minutes` (default `60`)
- `task.task_runs_gc.keep_per_subject` (default `50`)
- `task.task_runs_gc.retain_days` (default `30`)
- `task.task_runs_gc.retain_failed_days` (default `90`)

## Phasing

Each phase is independently shippable and testable.

1. **Schema + helpers.** ✅ Landed (as `scheduler::ledger`; reports dispatch
   through it).
2. **Retrofit system tasks.** ✅ Landed (as `scheduler::runner`; see Status).
   Route `draft_pipeline` and `graph_maintenance` through `task_runs`
   (claim-and-record). Delivers retry + backoff + history to existing
   tasks; fixes the retry-storm bug. Keep `last_run` fast-path. In-memory
   lock demoted to optimization. (`daily_briefing` is gone — it's a seeded
   report now and already rides the ledger.)
3. **Fold in feed polling.** ✅ Landed (as `ingest::poller`; see Status).
   `feed_poll` runs with `subject_id`; demote
   `feeds.last_polled_at`/`last_error` to fast-path cache; poll loop
   claims/records.
4. **Wiki regen.** Replace fire-and-forget regen on tag change with a
   `task_runs` row (`task_id = "wiki.regenerate"`, `subject_id = <tag id>`).
   Subject-keying gives natural per-tag dedup via the live-lease check;
   retry/backoff replaces silent loss on LLM failure.
5. **Retention GC.** `task_runs_gc` scheduled task with the policy + batched deletes above.
6. *(Follow-up, out of scope here)* Automations/recipes reuse the same ledger via `task_id = <automation id>` — no schema change expected.

## Risks & mitigations

- **Retry storm if backoff is bypassed.** The claim query MUST gate on `next_attempt_at <= now`. Add a test that a task failing N times is not re-attempted before the backoff window.
- **Stuck `running` rows after a crash.** Startup/claim path must treat `state='running' AND lease_until < now` as reclaimable. Mirror `atom_pipeline_jobs`'s lease-index recovery. Test a simulated crash (row left `running`, lease in the past) is reclaimed.
- **Multi-DB regression.** `task_runs`, GC, and the runner are all per-DB. Do not place `task_runs` in `registry.db`; do not introduce a process-global GC timestamp. Reuse the existing `manager.list_databases()` fan-out and per-`(task_id, db_id)` keying (CLAUDE.md multi-DB gotcha).
- **Migration ordering mistake.** The `LATEST_VERSION` bump + un-pinning the V16→V17 pragma to literal `17` must land together, or the schema version logic breaks. Called out explicitly above.
- **GC write contention on desktop.** Batched deletes + hourly cadence; never a single large `DELETE` under the write lock.

## Resolved decisions

1. **File reclamation deferred.** DELETE doesn't shrink the SQLite file without `VACUUM`/`auto_vacuum`. Accepted: rows are small and the retention caps keep practical growth modest, so this is explicitly out of scope. May revisit as a later refinement (periodic `PRAGMA incremental_vacuum` / WAL checkpoint) if file growth proves to matter in practice.
2. **Retention defaults confirmed.** `keep_per_subject = 50`, `retain_days = 30`, `retain_failed_days = 90`. Ship as-is.
3. **No server/desktop split.** One set of defaults for all deployment shapes; headless `atomic-server` deployments that want more generous retention use the settings overrides. No code branching on deployment type.
