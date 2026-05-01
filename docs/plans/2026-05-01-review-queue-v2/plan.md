# Knowledge Health Review Queue — UI Improvements (v2)

**Date:** 2026-05-01
**Status:** Planning
**Project:** Atomic
**Request:** Enhance the existing Review Queue modal with per-item inline actions, per-tab re-scan, richer resolution workflow (3-option Keep A/Keep B/Merge, source/recency badges, diff highlighting), batch selection, filtering/sorting, resolved counters, markdown export, and dashboard deep-linking. Preserve existing theme and layout.

---

## Executive Summary

The Review Queue modal (`HealthReviewModal.tsx`) renders 5 tabs over a single `HealthReport` snapshot. Today it has lightweight actions on two tabs (Content overlap: Merge/Keep both; Boilerplate: informational) and passive display on the other three (Contradictions, No source, Tag structure). The proposed v2 turns it into an interactive queue: every tab supports per-item actions, batch selection, filtering, and a persistent "dismissed/resolved" state. Several backend additions are required — dismissal storage, per-item source updates, per-check re-scan, LLM strip-boilerplate and merge-editor previews, and tag merge/move endpoints exposed for the modal.

**Recommended phasing:** ship in 4 waves — (A) dismissals + inline actions that reuse existing endpoints, (B) per-tab re-scan + resolved counters + lazy loading, (C) resolution upgrades (3-option resolver, diff highlighting, source badges, merge editor), (D) batch operations + export + dashboard integration.

---

## Current Architecture & Evidence

### Modal structure — `src/components/dashboard/widgets/HealthReviewModal.tsx` (644 lines)

- Single top-level `HealthReviewModal` component (L502–L643) takes a full `HealthReport` and a `checkName` pre-selector
- Tabs array built at L520–L526 — included conditionally based on which `checks[*]` has non-empty data
- `selectedTab` state at L528; `activeTab` defaults to first available tab (L529)
- `resolvedCount` state (L531) currently only increments on Content overlap `applyPairFix`; no persistence across sessions or tabs
- Escape key + body scroll lock at L533–L541; no other keyboard shortcuts
- `applyPairFix` callback at L543–L556 calls `apply_health_item_fix` with `check: 'duplicate_detection'`; `setResolvedCount(n => n + 1)` on success
- Tab bodies: `PairRow` (L68–L197), `BoilerplateSection` (L227–L267), `ContradictionRow`/`ContradictionSection` (L271–L376), `ContentQualitySection` (L380–L427), `TagHealthSection` (L431–L487)
- No batch selection, no filtering, no sorting controls, no re-scan, no export, no source/recency badges, no diff highlighting

### Action endpoints — `crates/atomic-server/src/routes/health.rs`

| Endpoint | What it does today | Relevance |
|---|---|---|
| `GET /api/health/knowledge` | Full report with all 5 review-data blobs | Used on modal open |
| `POST /api/health/fix` (`run_health_fix`) | Batch auto-fix across all checks | Used by the dashboard's big button, not by the modal |
| `POST /api/health/fix/{check}/{item_id}` (`apply_manual_fix`) | Per-item manual fix | Currently only handles `(duplicate_detection, merge_with_llm)` (L100–L125); **all other check+action pairs return 400** |
| `POST /api/health/undo/{fix_id}` | Undo a logged fix | Wired in the dashboard undo toast |
| `GET /api/health/history` / `GET /api/health/fixes/recent` | Historical reports and fix log | Not used by the modal |
| `POST /api/health/check/{check_name}` (`compute_single_check`) | Re-run one check in isolation | **Already exists** — can power per-tab re-scan |

### Existing fix primitives we can reuse

- `crates/atomic-core/src/health/llm_fixes.rs`
  - `merge_duplicate_pair(core, atom_a, atom_b, dry_run)` — returns the merged content when `dry_run=true` (no writes), otherwise writes + logs (L79–L226). Already supports preview.
  - `fix_untagged_complete_atoms(core, ids, dry_run)` — re-runs tagging pipeline
- `crates/atomic-core/src/storage/sqlite/tags.rs`
  - `apply_tag_merges_impl(&[TagMerge { winner_name, loser_name, reason }])` — canonical tag merge path (L512–L532); also exposed on `AtomicCore::apply_tag_merges` (`lib.rs` L2134)
  - `update_tag_impl(id, name, parent_id)` at L178–L219 — can reparent a tag (used for "Move under…")
  - `delete_tag_impl(id, recursive)` at L394 — exists for orphan cleanup
- Atom updates: `update_atom` command (command-map.ts L74–L79) takes `{ content, source_url, published_at, tag_ids, ... }` — "Add source" inline can reuse this with the atom's existing content

### Data shapes that back the queue

All live inside the `HealthReport` blob computed on demand; per-atom pre-fetches happen in `PairRow.toggleExpand` (L96–L110) and `ContradictionRow.toggleExpand` (L276–L292) via `get_atom`. There is **no** persistent "dismissed" state — if you refresh, everything that was dismissed returns.

### Dashboard integration — `src/components/dashboard/widgets/HealthWidget.tsx`

- "Apply N automatic fixes" button at the bottom of the widget (excluded checks tracked in `excludedFromFix`, L370+). Already has tooltip/label infrastructure.
- `setShowReviewModal(checkName)` is called from two places — auto-pops to first `requires_review` check on the main "Review" button, and from the `HealthCheckRow` component per-row. Deep-link works already; the issue is post-resolution dashboard refresh.

---

## Recommended Approach

Split across 4 phases so UX improvements land incrementally and each wave is independently shippable:

| Phase | Theme | Major deps |
|---|---|---|
| **A** | Dismissals + inline per-item actions | New DB table `health_dismissals`; extend `apply_manual_fix` |
| **B** | Per-tab re-scan, resolved counters, lazy content fetch | Reuse `compute_single_check`; new `checkUpdatedAt` state |
| **C** | 3-option resolver, source/recency badges, diff highlighting, merge-editor, contradiction summary | Extend `get_atom` cache; LLM-powered conflict summary; `diff-match-patch` dep |
| **D** | Batch selection, Strip boilerplate LLM pass, export, dashboard real-time sync | New `POST /api/health/strip-boilerplate`; frontend markdown export helper |

### Why dismissals must be persistent

Every feature (resolved counter, "Show deferred" toggle, "Mark intentional", batch dismiss, "Ignore pair") depends on somewhere to store *"this item should not appear until the underlying condition changes"*. Without it, every refresh re-surfaces everything, which defeats the queue metaphor. The cheapest fix is a new `health_dismissals` table keyed by `(check_name, item_key)` — see Phase A below for the schema.

### Dependency footprint

- Backend (Rust): 1 new migration, 1 new table, ~6 new endpoints/wrapper methods, 1 LLM prompt for "strip boilerplate", 1 LLM prompt for "contradiction summary"
- Frontend (TS): `diff-match-patch` (≈ 50KB, widely used, no peer deps). All other work uses existing primitives (Zustand store, Tailwind, lucide icons)

---

## Implementation Plan

### Phase A — Dismissals + Inline Per-Item Actions (~20h)

#### A1. New `health_dismissals` table (migration V18)

**File:** `crates/atomic-core/src/db.rs`

```sql
CREATE TABLE IF NOT EXISTS health_dismissals (
    id TEXT PRIMARY KEY,
    check_name TEXT NOT NULL,
    item_key TEXT NOT NULL,         -- e.g. atom_id, pair_id, tag_id, 'a_b' for pairs
    reason TEXT NOT NULL,           -- 'intentional_no_source', 'ignored_pair', 'deferred', 'resolved_other'
    dismissed_at TEXT NOT NULL,
    expires_at TEXT                 -- null = permanent until underlying data changes
);
CREATE UNIQUE INDEX idx_health_dismissals_lookup
    ON health_dismissals(check_name, item_key);
```

Bump `LATEST_VERSION` to 18. Follow the V17 idempotent pattern if existing tests re-run the migration.

#### A2. Storage methods and `AtomicCore` wrappers

**File:** `crates/atomic-core/src/storage/sqlite/health.rs`

```rust
pub(crate) fn list_dismissed_keys_impl(&self, check_name: &str) -> StorageResult<Vec<(String, String)>>;
pub(crate) fn dismiss_health_item_impl(&self, check_name: &str, item_key: &str, reason: &str, expires_at: Option<&str>) -> StorageResult<()>;
pub(crate) fn undismiss_health_item_impl(&self, check_name: &str, item_key: &str) -> StorageResult<()>;
```

Wire through `StorageBackend` (async) and `AtomicCore`.

#### A3. Filter dismissed items inside `compute_single_check` / `compute_health`

Add one `SELECT` per reviewable check. Feed a `HashSet<String>` of dismissed keys into the check function and exclude matches from `data.pairs` / `data.affected_atoms` / `data.issues.no_source.atoms` / `data.rootless_tag_list`.

Keep item keys stable:
- `content_overlap` / `contradiction_detection`: `{atom_a_id}__{atom_b_id}` sorted by id lexicographically
- `content_quality` no_source: atom_id
- `boilerplate_pollution`: atom_id
- `tag_health` rootless: tag_id
- `tag_health` similar_name: `{winner_id}__{loser_id}` sorted

#### A4. Extend `apply_manual_fix` with new (check, action) tuples

**File:** `crates/atomic-server/src/routes/health.rs` (L93–L126)

| check | action | Body | Behaviour |
|---|---|---|---|
| `content_overlap` | `keep_a` / `keep_b` | — | Delete loser atom; log undoable `before_state` |
| `content_overlap` | `dismiss` | — | Insert dismissal reason=`resolved_other` |
| `contradiction_detection` | `defer` | — | Dismissal with `expires_at = now + 7 days` |
| `contradiction_detection` | `dismiss` | — | Dismissal `resolved_other` |
| `contradiction_detection` | `summary` | — | LLM one-liner (Phase C4) |
| `content_quality` | `add_source` | `{url}` | `update_atom` preserving existing content |
| `content_quality` | `mark_intentional` | — | Dismissal reason=`intentional_no_source` |
| `tag_health` | `move_under` | `{parent_id}` | `update_tag_impl(id, name, Some(parent_id))` |
| `tag_health` | `merge` | `{into_tag_id}` | `apply_tag_merges_impl` |
| `tag_health` | `ignore_pair` | — | Dismissal `ignored_pair` |
| `boilerplate_pollution` | `reembed` | — | Enqueue `retry_embedding` |

#### A5. Frontend: per-item actions

- **No source tab**: `NoSourceRow` component — inline URL input + Save; "Mark intentional"; "Open ↗"
- **Tag structure tab**: rootless rows get "Move under…" dropdown populated from `useTags()` store; similar-name pairs get "Merge" confirm dialog + "Ignore pair"
- **Boilerplate tab**: "View edges" lazy-expand (same pattern as `PairRow.toggleExpand`); "Re-embed" button; "Strip boilerplate" disabled with "Coming soon" tooltip until Phase D

---

### Phase B — Per-Tab Re-scan, Resolved Counters, Lazy Loading (~10h)

#### B1. Per-tab "Re-scan" button

Top of each tab body: `↻ Re-scan`. Calls `health_check_single({check_name})` (command already exists at command-map.ts L722). On success, splice result into local `report.checks[name]` state.

Track `lastScannedAt: Record<string, string>`; render "Last checked: 2m ago" via `Intl.RelativeTimeFormat`.

#### B2. Resolved counters

Upgrade `resolvedCount` to `Record<string, number>`. Persist to localStorage scoped by active database id. Clear daily. Show "Resolved today: N" at the top of each tab; add a progress bar (`X / initial_queue_size`).

#### B3. Lazy tab content + virtualization

Only mount the active tab's body. For >50 items in a tab, wrap the list in `@tanstack/react-virtual` — already in deps via the canvas widget.

---

### Phase C — Resolution Upgrades (~25h)

#### C1. 3-option resolver for pairs

Replace Merge/Keep both with `Keep A | Keep B | Merge (edit)`:

- `Keep A` / `Keep B` — archive the loser via new `apply_manual_fix` action
- `Merge (edit)` — opens `MergeEditorModal`:
  1. Call `apply_health_item_fix` with `action: merge_with_llm, dry_run: true` (already supported by `merge_duplicate_pair` at llm_fixes.rs L79+)
  2. Show synthesis in CodeMirror editor pre-populated with dry-run content
  3. "Save merge" → new action `merge_with_edited_content` body `{ content, winner_atom_id, loser_atom_id }`
  4. Single `FixAction` for undo

#### C2. Source trust + recency indicator

Backend: content-overlap SQL already joins atoms; add `created_at` to the selected columns in `storage/sqlite/health.rs`. Contradiction query needs the same enrichment.

Frontend helper:
```ts
function trustScore(source: string | null, createdAt: string): { badge: string; score: number }
```
- +10 if hostname is in the `trusted_sources` setting (comma-separated)
- +5 if `created_at` within last 30 days
- Render per-atom badge; higher-scoring atom gets "Recommended" chip; ties → no chip

#### C3. Diff highlighting

Add `diff-match-patch` (~50KB). In `PairRow` / `ContradictionRow` expanded view, replace raw `<pre>` with line-diff. Atom A pane highlights removals red; Atom B pane highlights additions green. Content always fully visible.

#### C4. Contradiction summary (LLM)

New action in `apply_manual_fix`: `(contradiction_detection, summary)` body empty. Calls LLM: "In one sentence describe what factual claims conflict between these atoms, or 'no real conflict' if the differences are perspective, not fact." Cache per `pair_id` in frontend state.

#### C5. "Flag for later" + "Show deferred"

`defer` action inserts dismissal with `expires_at = now + 7d`. Tab header shows `Show deferred (N)` toggle when any deferred items exist. When enabled, pass `?include_deferred=true` to `compute_single_check` — rename the param if it conflicts, otherwise add it to the query extractor.

---

### Phase D — Batch, Strip Boilerplate, Export, Dashboard Sync (~25h)

#### D1. Selection mode

Checkbox per row. State `selectedItems: Record<string, Set<string>>` keyed by tab. Floating action bar when any selected:

```
[3 selected]  [Dismiss all]  [Apply suggested merge]  [Clear]
```

Sequential batch dispatch with progress callback. Undo stack captures all action ids; Undo applies in reverse.

#### D2. "Strip boilerplate" LLM pass

**New endpoint:** `POST /api/health/strip-boilerplate/{atom_id}` body `{ dry_run: bool }`.

New function `strip_boilerplate` in `llm_fixes.rs`:
1. Load atom + all atoms sharing ≥5 near-identical chunks (via `semantic_edges` ≥0.99)
2. LLM prompt: "The following atoms share template text. Return the unique content of atom_X only, preserving its specific details but removing shared sections present in all samples."
3. `dry_run=true` returns proposed content; `false` writes via `update_atom_content_only`

Frontend: dry-run → before/after diff modal → confirm → real call.

#### D3. Export queue to Markdown

Frontend-only. New `buildReviewQueueMarkdown(report, dismissals)` that iterates all 5 tab datasets and emits the format the prompt specifies. Reuse web/Tauri file-save split from existing `HealthExportModal.tsx`. Button lives in the modal header.

#### D4. Dashboard real-time sync

Debounce-wrap `fetchHealth()` in `HealthPanel` so batch actions only trigger one refresh. Optional: backend returns `{ dirty: true }` on dismissal changes so the dashboard can show "Scores may be stale — refresh" instead of forcing recompute.

#### D5. "Apply N automatic fixes" tooltip

Add tooltip `"Auto-fixes only affect: broken links, re-tagging empty atoms, trimming long content. Manual review items are handled in the Review Queue."` to the button in `HealthWidget.tsx`.

---

## Files / Components To Change

### Backend (Rust)

| File | Change |
|---|---|
| `crates/atomic-core/src/db.rs` | V18 migration; bump `LATEST_VERSION`; idempotent ALTER pattern |
| `crates/atomic-core/src/storage/sqlite/health.rs` | 3 dismissal methods; enrich overlap/contradiction queries with `created_at` |
| `crates/atomic-core/src/storage/mod.rs` | `StorageBackend` async wrappers for the 3 dismissal methods |
| `crates/atomic-core/src/health/checks.rs` | Thread `dismissed_keys` into every reviewable check; exclude matching items |
| `crates/atomic-core/src/health/mod.rs` | `compute_health` / `compute_single_check` pass dismissals; add `include_deferred` param |
| `crates/atomic-core/src/health/llm_fixes.rs` | New `strip_boilerplate` function; new `merge_with_edited_content`; `summarize_contradiction` |
| `crates/atomic-server/src/routes/health.rs` | Extend `apply_manual_fix` match with all new action tuples; add `POST /api/health/strip-boilerplate/{atom_id}` + OpenAPI annotation; thread `include_deferred` query param through `compute_single_check` |
| `crates/atomic-server/src/routes/mod.rs` | Register new strip-boilerplate route |
| `crates/atomic-server/src/lib.rs` | Add new handler + schema types to `#[openapi(paths(...))]` |

### Frontend (TypeScript)

| File | Change |
|---|---|
| `src/components/dashboard/widgets/HealthReviewModal.tsx` | Split into multiple files; add checkbox state, sort/filter bar, export button, per-tab re-scan |
| `src/components/dashboard/widgets/review/NoSourceRow.tsx` | **New** — inline URL editor + Mark intentional + Open |
| `src/components/dashboard/widgets/review/TagRootlessRow.tsx` | **New** — Move under dropdown + Dismiss |
| `src/components/dashboard/widgets/review/TagSimilarPairRow.tsx` | **New** — Merge confirm + Ignore pair |
| `src/components/dashboard/widgets/review/BoilerplateAtomRow.tsx` | **New** — View edges expand + Re-embed |
| `src/components/dashboard/widgets/review/MergeEditorModal.tsx` | **New** — CodeMirror merge editor with dry-run pre-fill |
| `src/components/dashboard/widgets/review/PairDiffView.tsx` | **New** — diff-match-patch line-mode rendering for side-by-side |
| `src/components/dashboard/widgets/review/ReviewQueueExport.ts` | **New** — buildReviewQueueMarkdown helper |
| `src/components/dashboard/widgets/review/trustScore.ts` | **New** — source/recency scoring helper |
| `src/components/dashboard/widgets/HealthWidget.tsx` | Debounced refresh on `onResolved`; "Apply N fixes" tooltip |
| `src/lib/transport/command-map.ts` | `strip_health_boilerplate` entry; pass `include_deferred` to `health_check_single` |
| `package.json` | Add `diff-match-patch` + `@types/diff-match-patch` |

---

## Data Flow / Interfaces

### Dismissal lifecycle

```
user clicks "Mark intentional"
  → POST /api/health/fix/content_quality/{atom_id}  body {action: "mark_intentional"}
  → dismiss_health_item(check="content_quality", key=atom_id, reason="intentional_no_source")
  → frontend optimistically removes row; onResolved() fires
  → next compute_single_check() excludes this atom until it gains a source URL
```

### Merge-editor flow

```
user clicks "Merge (edit)" on a pair
  → POST /api/health/fix/content_overlap/{pair_id}  body {action: "merge_with_llm", dry_run: true}
  → merge_duplicate_pair(dry_run=true) returns synthesized content (no writes)
  → MergeEditorModal opens; pre-fills CodeMirror with synthesis
  → user edits, clicks "Save merge"
  → POST /api/health/fix/content_overlap/{pair_id}  body {action: "merge_with_edited_content", content, winner_atom_id, loser_atom_id}
  → update_atom(winner.id, edited_content); delete_atom(loser.id); log FixAction
```

### Batch dispatch

```
user selects 3 pairs, clicks "Dismiss all"
  → for each: POST /api/health/fix/content_overlap/{pair_id} {action: "dismiss"}
  → frontend shows "Processing 2/3…"
  → on completion: toast "✅ 3 pairs dismissed" with Undo
  → Undo → reverse sequence of undismiss calls
```

---

## Configuration / Secrets / Deployment Notes

### New settings (optional — default empty/off)

- `trusted_sources` (string, comma-separated hostnames) — used by trustScore helper
- `review_queue.auto_defer_days` (int, default 7) — expires_at for "Flag for later"
- `review_queue.batch_concurrency` (int, default 1) — parallelism for batch dispatch; keep at 1 by default to preserve order for undo

No secrets needed. No new env vars. The LLM endpoints reuse the already-configured provider (OpenRouter or Ollama).

### Schema migration deployment

The V18 migration is additive-only (new table + unique index). Safe to deploy without downtime. Backfill is not required — an empty `health_dismissals` table means nothing is dismissed, which is the correct initial state.

### OpenAPI surface

Register 1 new path (`/api/health/strip-boilerplate/{atom_id}`) plus extended request body schema for `ManualFixRequest` (add optional fields for `url`, `parent_id`, `into_tag_id`, `content`, `winner_atom_id`, `loser_atom_id`, `dry_run`). All schemas under `#[cfg_attr(feature = "openapi", derive(ToSchema))]` matching the existing convention.

---

## Testing / Validation Plan

### Automated

**Backend unit tests** (`crates/atomic-core/src/health/tests.rs`):
- `test_dismissed_content_overlap_excluded` — create fixture with 3 pairs, dismiss 1, confirm `compute_single_check` returns 2
- `test_dismissed_tag_health_rootless_excluded` — same pattern for rootless tags
- `test_contradiction_defer_expires` — insert dismissal with `expires_at` in the past, confirm item reappears
- `test_add_source_updates_atom` — call `apply_manual_fix` with `add_source`, verify atom `source_url` is set without touching content
- `test_move_under_reparents_tag` — `apply_manual_fix` with `move_under`, verify `update_tag_impl` was called with the new parent
- `test_tag_merge_via_health_fix` — `apply_manual_fix` with `merge`, verify `apply_tag_merges_impl` ran and atoms were re-tagged
- `test_keep_a_archives_b` — confirm loser atom is soft-deleted, winner untouched
- `test_merge_dry_run_returns_content_no_writes` — already covered by existing `merge_duplicate_pair` test; add assertion that atom row count unchanged
- `test_strip_boilerplate_dry_run` — stub LLM, confirm original atom content unchanged after dry_run

**Frontend unit tests** (`src/components/dashboard/widgets/__tests__/`):
- `NoSourceRow.test.tsx` — click Add source, enter URL, verify `update_atom` called with correct body
- `TagRootlessRow.test.tsx` — select parent from dropdown, verify `apply_health_item_fix` with `move_under`
- `MergeEditorModal.test.tsx` — mock dry_run response, render editor, edit content, save, verify final mutation
- `PairDiffView.test.tsx` — snapshot test for red/green highlighting of a known diff
- `trustScore.test.ts` — table-driven cases (trusted hostname wins, recent age beats old, ties produce no chip)
- `ReviewQueueExport.test.ts` — fixture report → matches expected markdown byte-for-byte

**Commands:**
```bash
cargo test -p atomic-core -- health
cargo test -p atomic-core -- boilerplate
cargo test -p atomic-server -- health
cargo check -p atomic-core -p atomic-server
npx tsc --noEmit
npx vitest run src/components/dashboard/widgets/__tests__/
npm run lint
```

### Manual / E2E

- Build the desktop app: `npm run tauri dev`
- Seed 3 overlapping atoms, 2 no-source atoms, 2 rootless tags in a test DB
- Exercise each per-item action; verify dismissed items stay dismissed across modal close/reopen
- Trigger batch dismiss on 3 items; verify undo rolls all 3 back
- Trigger Merge-edit flow; confirm the editor pre-fills and saving updates both atoms
- Run `npm run build:mobile` then load in Capacitor iOS/Android to smoke-test the new touch-friendly controls (checkboxes, inline URL input). Capacitor builds may require the simulator running — use `npm run dev:mobile:ios` for a live loop.
- Export queue to markdown; diff against a known-good fixture

### Blockers for runnable E2E
- The project does not appear to ship a Playwright / Cypress harness in the repo — E2E is manual via `npm run tauri dev`. If the team wants automated E2E, that's a separate track and not covered here.
- LLM-dependent features (Merge, Strip boilerplate, Contradiction summary) require a reachable OpenRouter key or a running Ollama instance. Tests should mock the provider via the existing `MockLlmProvider` pattern (search support/mod.rs) to avoid hitting the network.

---

## Risks, Assumptions, and Open Questions

### Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Dismissal table grows unbounded (every dismissed pair, tag, atom) | Medium | Add periodic cleanup: delete dismissals where underlying atom/tag no longer exists; cap total at 10k rows per check with FIFO eviction |
| Merge-edit flow can race: user A dismisses while user B is mid-merge | Low | Idempotent actions: merge_with_edited_content checks both atoms still exist before writing; returns 409 on conflict |
| LLM cost for Contradiction summary × 20 pairs on modal open | Medium | Lazy-fetch: only call summary when user expands a pair; cache per-session |
| Diff-match-patch for very long atoms (>10k chars) is slow | Low | Truncate content to first 2000 chars for diff view; show "Content truncated for diff; click Open to view full atom" |
| Batch dispatch partial failure (3rd of 5 fails) | Medium | Stop on first failure; show error toast with what was applied; user can retry the rest |
| "Strip boilerplate" LLM hallucination removes unique content | High | Always dry-run first; show before/after diff; never auto-apply in batch |
| Move-under dropdown performance with 1000s of tags | Low | Virtualize the dropdown (same library as tag tree) |

### Assumptions

- `HealthReport.checks[*].data` structure is stable across all five reviewable checks (verified in current code)
- `merge_duplicate_pair` dry-run returns content in a predictable shape — verify in `llm_fixes.rs`; may need adapter
- `update_atom` preserves `created_at` / `updated_at` semantics (verify; should it bump `updated_at` on source-only edit?)
- Tag merge UI does not need to preview affected atoms before applying — a count is sufficient. If the team wants full preview, add a separate "Preview merge" dry-run mode to `apply_tag_merges_impl`
- `retry_embedding` command is the correct primitive for the "Re-embed" button; already exists in command-map (verify exact name)

### Open questions

1. **Archived vs deleted for Keep A / Keep B** — do we soft-archive (set a `status` flag) or hard-delete? Current `delete_atom` hard-deletes. Soft-archive would need schema work. Recommendation: use existing hard-delete; rely on `health_fix_log.before_state` snapshot for undo. Check whether `log_fix` already stores atom content snapshot on delete.
2. **Item-key collision across DBs** — if the same atom_id exists in two databases, dismissal needs DB scoping. `health_dismissals` is per-DB (lives in data DB, not registry DB), so this is implicit — verify when wiring the migration.
3. **Where does the "resolved today" counter live?** — localStorage is simple but doesn't sync across devices. Server-side is more work. Recommendation: localStorage for Phase B; revisit if users want cross-device.
4. **What happens to a dismissed content_overlap pair when one of the atoms is deleted?** — dismissal becomes stale. Cleanup job: delete `health_dismissals` rows whose `item_key` references a non-existent atom. Run on startup + weekly.
5. **Strip boilerplate threshold** — ≥5 shared edges is the current boilerplate-detection threshold. Reuse that, or make it configurable per-call?
6. **Tag merge confirm count** — `count_atoms_with_tags` already exists. Just wire it up.

---

## LOE / Effort Estimate

| Phase | Hours | Deliverable |
|---|---|---|
| A | 20 | Dismissals table + per-item actions on all tabs (except Strip/Merge-edit) |
| B | 10 | Re-scan + resolved counters + lazy content |
| C | 25 | 3-option resolver + merge editor + source badges + diff highlighting + contradiction summary + "Flag for later" |
| D | 25 | Batch selection + Strip boilerplate LLM pass + Export + Dashboard sync + tooltip |
| **Total** | **80** | ~2.5 weeks at 30 hrs/week |

Additive: ~10% (8h) for unit + component tests across all phases, split evenly. ~15% (12h) for integration/manual QA across mobile + desktop.

**Net estimate:** ~100 hours end-to-end, matching the complexity of the original v1 health dashboard plan.

---

## Decision Log

| Date | Decision | Rationale |
|---|---|---|
| 2026-05-01 | New `health_dismissals` table rather than per-check flag columns | Single polymorphic table serves all 5 checks; easier to add future review categories |
| 2026-05-01 | Dismissal keys are string composites (`a__b` sorted) | Avoids schema changes when we add new key shapes; frontend can construct keys without backend knowledge |
| 2026-05-01 | Reuse `merge_duplicate_pair` dry-run for merge-editor pre-fill | Primitive already exists; avoids new LLM code paths |
| 2026-05-01 | Diff-match-patch over a richer diff library (e.g., react-diff-viewer) | 50KB vs 200KB+; we only need line-mode highlighting, not a full diff UI |
| 2026-05-01 | Resolved counter in localStorage, not server-side | Low stakes; avoids new storage roundtrips. Revisit if cross-device sync requested |
| 2026-05-01 | Hard-delete for Keep A/Keep B, rely on `before_state` for undo | Avoids soft-delete schema work; existing undo infra already handles this pattern for `fix_source_uniqueness` |
| 2026-05-01 | Phase ordering A→B→C→D | Each phase is independently shippable; A unlocks everything, D has the most LLM cost and lowest UX criticality |
