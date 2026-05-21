# Phase 3 — Briefing Collapse + External-consumer Kind Filter

Companion to `docs/plans/reports.md`. Phase 1 added the `kind` discriminator
and filter discipline; phase 1.5 added the `task_runs` execution ledger;
phase 2 added the reports primitive (schema, runner, scheduler loop, REST
surface). Phase 3 collapses the legacy briefing path onto the reports
runner, migrates historical briefings into finding atoms, and ships the
external-consumer `?kinds=` parameter on atom-list endpoints.

## Pre-plan finding — Obsidian plugin

The base plan doc (`docs/plans/reports.md` §"External consumers") assumes
the Obsidian plugin pulls atoms into the vault as markdown files and would
therefore materialize finding atoms as files on disk. **It doesn't.** The
current plugin (`plugins/obsidian-plugin/src/`) is push-only — Obsidian
files become atoms in atomic via `createAtom` / `updateAtom`, but no path
goes the other way. There is no list-atoms call in `atomic-client.ts` and
no file-creation logic in `sync-engine.ts` triggered by atom events.

Operational consequences:

- `?kinds=` ships server-side for **future** consumers (MCP exports,
  mobile sync, third-party API users) — defensive future-proofing, not a
  current product fix.
- No coordinated plugin release is required for phase 3. Server and
  plugin can ship independently.
- The plan-doc's "hard prereq" about plugin-passes-`?kinds=captured`-first
  is moot for the current plugin codebase.

We still ship `?kinds=` because the protocol shape should be right before
any external consumer materializes findings.

## Deliverables

1. **`?kinds=` parameter on atom-list endpoints.** `GET /api/atoms`
   accepts `?kinds=captured`, `?kinds=report`, or `?kinds=captured,report`.
   Missing/empty = all kinds (backwards compatible). Invalid value → 400
   `AtomicCoreError::Validation`. Parses to `KindFilter::Only(...)` and
   threads through `list_atoms`. Internal UI callers continue passing
   `KindFilter::All` explicitly.

2. **Seed the default Daily Briefing report on every DB.** Idempotent on
   every server start (existing DBs migrated in place, fresh DBs seeded
   on first boot). Reads existing `briefing_prompt` setting to populate
   `research_prompt`; converts the existing `BriefingSchedule`
   (frequency + time + tz) into a cron expression + IANA timezone;
   creates the `Reports/Briefings` tag and stamps it as the report's
   `output_atom_tags`; sets `dashboard.featured_report_id` to the new
   report's id.

3. **Migrate historical `briefings` → finding atoms.** One-shot Rust
   data migration, gated by a per-DB `briefings.migrated_to_findings`
   settings flag so it never runs twice. For each row in `briefings`:
   insert an atom with `kind = 'report'`, content + `created_at`
   preserved, tagged with `Reports/Briefings`, `tagging_status =
   'skipped'`; insert a `report_findings` provenance row pointing at the
   seeded Daily Briefing report; for each row in `briefing_citations`
   insert a corresponding `report_finding_citations` row. Drops the
   `briefings` and `briefing_citations` tables at the end of the same
   transaction.

4. **Replace `DailyBriefingTask` with the reports loop.** Delete the
   `TaskRegistry::register(DailyBriefingTask)` call in
   `atomic-server::main.rs`. The phase-2 reports background loop picks
   up the seeded report automatically once it exists.

5. **Dashboard reads from the featured report.** `BriefingWidget.tsx` +
   `BriefingContent.tsx` rewire from `get_latest_briefing` /
   `list_briefings` to `list_findings_for_report(featured_report_id,
   limit=1)`. The render shape is identical (finding atom prose +
   citation list), so the UX is unchanged.

6. **Briefings module + table teardown.** Once #3 has run on every DB
   and #5 reads from findings, the entire briefing path comes down in
   the same commit:
   - Delete `crate::briefing::*` (agentic loop, schedule helpers, task,
     models).
   - Delete the `BriefingStore` trait and both SQLite/Postgres impls.
   - Drop the `/api/briefings/*` REST routes and their command-map
     entries.
   - The Rust data migration drops the `briefings` /
     `briefing_citations` tables at the end of its commit.
   - Remove `briefing_*` settings keys from `crate::settings::DEFAULTS`.

7. **Per-DB `dashboard.featured_report_id` setting + endpoints.**
   Stored via `core.storage().set_setting_sync()` (per-DB, not
   registry-routed — value must isolate per database). REST surface:
   - `GET /api/dashboard/featured-report`
   - `PUT /api/dashboard/featured-report`
   Cleared automatically when the referenced report is deleted (handled
   in `reports.rs::delete_report`).

## What's NOT in phase 3

Reserved for phase 4 ("Report authoring UX") per `docs/plans/reports.md`
§Phasing:

- Report authoring UI (create / edit / enable-disable / run-now / view
  history).
- Dashboard featured-report picker (user-facing chooser).
- TZ-aware cron editor in the settings panel.

Phase 3 lands the data model and plumbing; the UI to drive it is phase 4.
For phase 3, the user can edit the seeded report via REST.

## Migration ordering

The collapse runs in this exact order on each DB on server startup,
before any HTTP listener is bound:

1. SQL migrations 1..=21 (existing) — schema is at end of phase 2.
2. SQL migration V22 — empty marker block (`PRAGMA user_version = 22`).
   No DDL because the table drops are owned by the Rust path below; a
   pure SQL `DROP TABLE briefings` would run before the data migration
   and lose history.
3. Rust `reports::seed::seed_default_briefing_report(core)` —
   idempotent. Creates the seeded report row + `Reports/Briefings` tag
   + `dashboard.featured_report_id` setting if missing.
4. Rust `reports::seed::migrate_briefings_to_findings(core)` — gated by
   the per-DB flag. Reads `briefings` / `briefing_citations` via raw
   SQL (the storage trait is being deleted), writes finding atoms +
   provenance + citations, sets the flag, drops the briefings tables in
   the same transaction.
5. Schema is now at V22; the `briefings` tables are gone; the seeded
   report owns the historical content.

The seed step is split from the migration step so that fresh DBs (with
no briefings rows to migrate) still get the seed and the featured-report
pointer.

## Decisions

- **All-in-one teardown.** One commit drops briefings tables + module +
  routes. Slightly more risk on the migration step; mitigation is the
  per-DB flag + that the migration runs in a single transaction
  (rollback if any row fails).
- **No Obsidian plugin work this phase.** Push-only plugin has no
  finding-materialization path. `?kinds=` ships server-side only.
- **Seeded report's `enabled` matches prior briefing status.** A user
  who had briefings disabled stays disabled; a user who had them daily
  gets a daily report on the same schedule. Preserves intent without
  surprise activation or surprise silence.
- **Rust data migration uses raw SQL** for reading the briefings tables
  rather than going through `BriefingStore`. The trait is being deleted
  in the same commit; no reason to add helpers we'll throw away.
- **Migration runs at server startup, not in a one-shot CLI.**
  Idempotent via the per-DB settings flag, so multiple starts are safe.
- **Schedule conversion deterministic:**
  - `Daily HH:MM` → `0 MM HH * * *`
  - `Weekly HH:MM Day` → `0 MM HH * * <0-6>` (Sun = 0, Sat = 6 — matches
    the `cron` crate's accepted DoW range).
  - `Off` → seeded `enabled = false`, default cron `0 0 7 * * *`.
- **Legacy `briefing_prompt` setting cleared after seeding.** The
  seeded report's `research_prompt` is the new source of truth.
  Leaving the legacy setting around invites drift.

## File-level plan

### Schema

- `crates/atomic-core/src/db.rs`: bump `LATEST_VERSION` to 22; add an
  empty V21→V22 block (`PRAGMA user_version = 22;`). The Rust migration
  owns the `DROP TABLE` statements.
- `crates/atomic-core/src/storage/postgres/migrations/018_briefings_teardown.sql`:
  same empty marker block. Postgres migration framework already records
  the version via `schema_version`.

### Models / core API

- Delete `pub mod briefing;` from `lib.rs`.
- Delete `BriefingStore` trait + SQLite + Postgres impls + dispatch
  entries.
- Remove from `AtomicCore`: `run_daily_briefing`,
  `get_briefing_schedule`, `set_briefing_schedule`, `get_latest_briefing`,
  `get_briefing`, `list_briefings`.
- New module `crates/atomic-core/src/reports/seed.rs`:
  - `pub async fn seed_default_briefing_report(core: &AtomicCore) ->
    Result<(), AtomicCoreError>` — idempotent.
  - `pub async fn migrate_briefings_to_findings(core: &AtomicCore) ->
    Result<usize, AtomicCoreError>` — gated by per-DB flag, returns
    rows migrated.
  - Internal: `briefing_schedule_to_cron(schedule:
    &BriefingSchedule) -> String` (kept private to the module since
    `BriefingSchedule` itself is being deleted).
  - Internal: `get_or_create_reports_briefings_tag(core) ->
    Result<String, AtomicCoreError>`.
- New `AtomicCore::get_featured_report_id()` and
  `AtomicCore::set_featured_report_id(Option<&str>)`.

### Server (`crates/atomic-server`)

- `routes/atoms.rs`:
  - Extend `GetAtomsQuery` with `kinds: Option<String>`.
  - New helper `parse_kinds(Option<&str>) -> Result<KindFilter,
    HttpResponse>` returning 400 on invalid values.
  - Threaded into `db.0.list_atoms(...)`.
- Delete `routes/briefings.rs` entirely; remove from `routes/mod.rs`.
- New `routes/dashboard.rs`:
  - `GET /api/dashboard/featured-report` — returns
    `{ "report_id": "..." | null }`.
  - `PUT /api/dashboard/featured-report` — `{ "report_id": "..." |
    null }`.
- `routes/reports.rs::delete_report` clears
  `dashboard.featured_report_id` if it matches the deleted id.
- `main.rs`:
  - Remove the `DailyBriefingTask` registration line.
  - Before `HttpServer::new(...)`: iterate `manager.list_databases()`
    and for each DB call
    `reports::seed::seed_default_briefing_report(&core).await?` then
    `reports::seed::migrate_briefings_to_findings(&core).await?`. Logs
    the row count migrated per DB. A failure in one DB logs at error
    level and skips that DB rather than aborting startup (the next
    boot retries).

### Frontend (`src/`)

- `lib/transport/command-map.ts`:
  - Delete every command starting with `*_briefing*`.
  - Add `get_featured_report_id`, `set_featured_report_id`.
  - Ensure `list_findings_for_report` is present (added in phase 2).
- `components/dashboard/widgets/BriefingWidget.tsx`: rewrite as
  `FeaturedReportWidget`. Loads `featured_report_id`, then loads
  `list_findings_for_report(id, limit=1)`. Empty state with placeholder
  copy pointing at settings (phase 4 wires the chooser).
- `components/dashboard/widgets/BriefingContent.tsx`: rename to
  `FindingContent.tsx`. Same markdown + citation render path —
  resolves citation atom ids the same way it did before.
- `components/settings/SettingsModal.tsx`: delete the briefing schedule
  + prompt section. Replace with a stub linking to "Reports (coming in
  the next release)" or hide entirely.
- Delete `BriefingScheduleStatus`, `BriefingSchedule`, `Briefing`,
  `BriefingCitation`, `BriefingWithCitations` TypeScript types.

## Tests

Inline in `crates/atomic-core/src/lib.rs` and route-level in
`crates/atomic-server/tests` (or inline in lib.rs where the route handler
sits — match existing convention).

1. `kinds_query_filter_parses_captured` — `?kinds=captured` →
   `KindFilter::Only([Captured])`.
2. `kinds_query_filter_parses_csv_both` — `?kinds=captured,report` →
   `KindFilter::Only([Captured, Report])`.
3. `kinds_query_invalid_returns_400` — `?kinds=banana` → 400.
4. `kinds_query_missing_returns_all_kinds` — no param → `KindFilter::All`
   (backwards compat).
5. `seed_default_briefing_report_idempotent` — run twice, one row.
6. `seed_pulls_research_prompt_from_briefing_prompt_setting` — legacy
   setting populated, seeded report's `research_prompt` matches.
7. `seed_converts_daily_schedule_to_cron` — Daily 07:00 → `0 0 7 * * *`.
8. `seed_converts_weekly_schedule_to_cron` — Weekly Monday 09:30 →
   `0 30 9 * * 1`.
9. `seed_respects_off_frequency_with_enabled_false` — Off → seeded with
   `enabled = false`.
10. `seed_creates_reports_briefings_tag_idempotently` — re-runs don't
    duplicate.
11. `seed_sets_dashboard_featured_report_id` — settings table holds the
    seeded report's id under the documented key.
12. `migrate_briefings_to_findings_writes_atoms_with_kind_report` —
    every migrated atom has `kind = 'report'` and
    `tagging_status = 'skipped'`.
13. `migrate_briefings_to_findings_preserves_citations` — every
    `briefing_citations` row produces a `report_finding_citations` row
    pointing at the same `cited_atom_id`.
14. `migrate_briefings_to_findings_idempotent` — flag prevents re-run;
    second call returns 0.
15. `featured_report_id_cleared_when_report_deleted` — deleting the
    referenced report clears the setting.
16. `end_to_end_seed_then_empty_scope_run` — seed on a fresh DB, call
    `run_report` directly, observe `RunOutcome::EmptyScope` and an
    advanced `last_run_at`.

## Risks & mitigations

- **Large historical briefings table.** A user with thousands of rows
  could see a slow first startup. *Mitigation:* stream the migration
  row-by-row (low memory footprint), log progress every 100 rows,
  commit per row so partial progress is preserved on crash. The
  per-DB flag is set only after a full pass.
- **Schedule conversion fidelity.** Weekly schedules with a weekday
  field need careful DoW mapping. *Mitigation:* dedicated unit tests
  covering Daily / Weekly / Off, asserting on the exact cron string.
- **Multi-DB startup cost.** Each DB pays the migration on first boot.
  *Mitigation:* the migration is gated by the per-DB flag and the cron
  scheduler doesn't run during startup; users see slightly delayed
  HTTP availability on first boot only.
- **A user has manually deleted their `briefings` table.** Rust
  migration reads from a now-empty table, writes zero finding atoms,
  sets the flag, drops the (empty) table. No-op, no error.
- **The seeded `Reports/Briefings` tag conflicts with a user-authored
  tag of the same name.** *Mitigation:* the seed helper uses
  get-or-create semantics on `(name, parent)`, so the existing user
  tag is reused.
- **Settings drift between `briefing_prompt` and the seeded report's
  `research_prompt`.** *Mitigation:* the seed clears the legacy
  setting after copying it into the report. The settings panel deletes
  the corresponding input in the same commit.

## Estimate & branching

- ~1500–2000 LOC including tests and frontend changes.
- Branch `reports-phase-3-collapse` stacked on phase 2
  (`reports-phase-2-primitive`, HEAD `eea47ad`).
- One commit, message `reports phase 3: briefing collapse + kinds
  query parameter`.

## Acceptance gate

- `cargo test -p atomic-core --lib` (default features) — all green.
- `cargo test -p atomic-core --lib --features postgres` — all green.
- `cargo check --workspace --all-features` — clean.
- `cargo clippy -p atomic-core -p atomic-server --all-targets` — clean
  on touched files.
- `cargo fmt --check` — clean.
- `npm run lint && npm run typecheck` in `src/` — clean.
- Manual smoke: start the server against a DB seeded with several
  historical briefings → observe seed + migration in logs → confirm
  `GET /api/reports/{id}/findings` returns the migrated rows → confirm
  `GET /api/atoms?kinds=captured` excludes finding atoms.
