# Knowledge Health Dashboard — UI Improvements

**Date:** 2026-05-01  
**Status:** Reviewed (2026-05-01) — see [REVIEW.md](./REVIEW.md)
**Project:** Atomic (desktop + web)  
**Request:** Implement comprehensive UX enhancements to the health dashboard component, including per-item actions, sample review, filtering, trending, and improved affordances.

---

## Executive Summary

The current health dashboard (`HealthWidget.tsx`) displays a vertical list of check rows with scores, summary text, and a global "Apply N fixes" button. This plan adds:

1. **Per-row expandability** with Run and Review buttons, individual fix toggles
2. **Sample review panels** showing 3–5 atoms that triggered each issue, with quick-actions (Fix/Dismiss/Open)
3. **Score trending** (↑↓→ indicators) and last-run timestamps
4. **Filtering & sorting** (severity, auto-fixable, recency)
5. **Severity badges** (🔴🟠🟡🟢) and status colors per check
6. **Improved action bar** with confirmation modals, undo stack, export
7. **Micro-interactions** (animated bars, toast notifications, keyboard shortcuts)
8. **OpenAPI spec coverage** for all `/api/health/*` endpoints (prerequisite — currently missing)

**Scope:** Frontend (React/TypeScript) + Backend (Rust: utoipa annotations + ApiDoc registration). Health endpoints are currently missing from the generated OpenAPI spec and must be added before external clients (iOS, Android, MCP, SDK consumers) can use them.

**Effort:** 4 phases, ~100–110 hours of development (Phase 0 adds 8–10 hours for spec coverage).

---

## Current Architecture & Evidence

### HealthWidget.tsx (src/components/dashboard/widgets/)
- **Current structure:**
  - Single component with hardcoded `CHECK_ORDER` (L160–L172)
  - Per-row display: icon, label, score bar, description (L288–L332)
  - Global "Apply N fixes" button with expandable checklist (L353–L383)
  - Review modal dispatch (`HealthReviewModal`, L406–L410)
  - No per-row timestamps, trending, or individual fix toggles

- **Current state management:**
  - `report`: Full `HealthReport` object with all checks
  - `showPending`: Boolean toggle for "What will this do?" checklist
  - `showReview`: Boolean for modal
  - `lastFix`: Latest fix response result
  - No per-check UI state (expanded, running, etc.)

### HealthReviewModal.tsx
- Opens for one category at a time
- Shows pairs/samples and per-pair actions (Merge/Keep/Delete/Open)
- Uses `get_atom` API to fetch atom details
- No cross-category comparison or bulk operations

### Backend API Surface (crates/atomic-server/src/routes/health.rs)
- `GET /api/health/knowledge` — Returns `HealthReport` with all checks + computed_at
- `POST /api/health/fix` — Takes `FixRequest { mode, include_medium, dry_run }` → returns `FixResponse`
- `POST /api/health/fix/{check}/{item_id}` — Manual per-item fix (merge/delete strategies)
- `POST /api/health/undo/{fix_id}` — Undo a fix from audit log
- `GET /api/health/history` — Recent stored reports (for trending)
- `GET /api/health/fixes/recent` — Recent fix log entries

**Gap:** No endpoint for running a single check in isolation. Needed for per-row "Run" buttons.

### Type Definitions (atomic-core/src/health/mod.rs)
- `HealthCheckResult`: `status`, `score`, `auto_fixable`, `requires_review`, `fix_action`, `data`
- `HealthReport`: `overall_score`, `overall_status`, `computed_at`, `checks: HashMap<String, HealthCheckResult>`, `auto_fixable`, `requires_review`
- `FixResponse`: `actions_taken: Vec<FixAction>`, `skipped`, `new_score`

---

## Recommended Approach

### Design Principles
1. **Preserve existing color scale** (green ≥90, yellow 70–89, orange 50–69, red <50)
2. **Dark theme (Obsidian-inspired):** `#1e1e1e` bg, `#7c3aed` purple accent
3. **Progressive disclosure:** Summary row → expandable for details → modal for complex decisions
4. **Idempotent actions:** All fixes are safely retryable
5. **Accessibility:** ARIA labels, focus states, keyboard navigation

### Technical Strategy

#### Phase 1: Foundation (Expandable rows, per-row state, Run/Review buttons)
- Refactor single `HealthWidget` into `HealthCheckRow` sub-component with local state
- Add `expandedChecks` Set to track which rows are open
- New endpoint: `POST /api/health/check/{check_name}` for isolated check runs
- Per-row buttons: Run (spinner), Review (lazy-load samples), individual fix toggle
- Local UI state: `lastRunTimes`, `checkTrends`, `checkSamples`

#### Phase 2: Trends, Timestamps, Filtering (Score history, severity badges, sort/filter UI)
- Fetch historical reports from `GET /api/health/history`
- Compute score delta (current vs. previous) for trend indicator
- Add `last_run` timestamp to each check result (backend: store in report)
- Filter row above checks: Severity, Auto-fixable, Recency
- Sort options: By score (asc/desc), by affected count, alphabetical, auto-fixable first
- Severity badge logic: 🔴 (0–40), 🟠 (41–70), 🟡 (71–85), 🟢 (86–100)

#### Phase 3: Advanced Affordances (Confirmation modals, undo stack, export, keyboard shortcuts)
- Confirmation modal before batch fixes (grouped by check, showing expected delta)
- Undo toast: "Undo" button + 10s timeout
- Export: Generate markdown report with all findings, citations, sample atoms
- Keyboard shortcuts: `r` (refresh), `1–7` (expand check), `f` (apply fixes), `?` (help)
- Animated score bars: CSS transition on mount/update
- Toast notifications: "✅ Fixed N items. Score 80 → 85"

---

## Implementation Plan

### Phase 0: OpenAPI Spec Coverage for Health Endpoints (Prerequisite, ~8–10 hours)

**Why this is Phase 0:** The `export-openapi` binary and `utoipa` `ApiDoc` struct in `crates/atomic-server/src/lib.rs` drive spec generation for all external SDK consumers (iOS, Android, MCP bridge, third-party integrations). Every `/api/health/*` route is currently **missing from the spec** because:

1. None of the handler functions in `crates/atomic-server/src/routes/health.rs` have `#[utoipa::path(...)]` attribute macros.
2. No health route paths are listed in the `#[openapi(paths(...))]` declaration in `crates/atomic-server/src/lib.rs` (lines 30–167).
3. Health-specific schema types (`HealthReport`, `HealthCheckResult`, `FixRequest`, `FixResponse`, `FixAction`, `SkippedFix`, `ManualFixRequest`, `HistoryQuery`, `StoredHealthReport`, `HealthFixLog`) are not in the `components(schemas(...))` list.

**Evidence:**
- `crates/atomic-server/src/routes/health.rs` — handlers lack utoipa annotations (confirmed by reading the file; comments show routes but no `#[utoipa::path]` decorators)
- `crates/atomic-server/src/lib.rs:30–167` — paths list has no `routes::health::*` entries
- `crates/atomic-server/src/lib.rs:175–286` — components schemas list has no health types
- `crates/atomic-server/src/lib.rs:290–313` — tags list has no `health` entry

**Impact of not fixing:**
- iOS/Android clients cannot generate typed bindings for health endpoints
- MCP tools for LLM agents cannot discover health operations
- External SDK consumers have no schema contract; they must reverse-engineer from handler code
- API reference docs (served at `/scalar`) omit the entire health surface

#### 0.1 Annotate health route handlers with `#[utoipa::path(...)]`
**File:** `crates/atomic-server/src/routes/health.rs`

Each handler needs a path macro. Example for `get_health_knowledge`:

```rust
#[utoipa::path(
    get,
    path = "/api/health/knowledge",
    tag = "health",
    responses(
        (status = 200, description = "Current health report", body = HealthReport),
        (status = 500, description = "Internal server error", body = ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
)]
pub async fn get_health_knowledge(db: Db) -> HttpResponse { ... }
```

All seven handlers need annotation:

| Handler | Method | Path | Notes |
|---------|--------|------|-------|
| `get_health_knowledge` | GET | `/api/health/knowledge` | Returns `HealthReport` |
| `run_health_fix` | POST | `/api/health/fix` | Body: `FixRequest`, returns `FixResponse` |
| `apply_manual_fix` | POST | `/api/health/fix/{check}/{item_id}` | Path params + `ManualFixRequest` body, returns `FixAction` or `{status: "no_op"}` |
| `undo_health_fix` | POST | `/api/health/undo/{fix_id}` | Path param, returns `{status, fix_id}` |
| `get_health_history` | GET | `/api/health/history` | Query: `limit`, returns `Vec<StoredHealthReport>` |
| `get_recent_fixes` | GET | `/api/health/fixes/recent` | Query: `limit`, returns `Vec<HealthFixLog>` |
| `compute_single_check` (Phase 1 addition) | POST | `/api/health/check/{name}` | Path param, returns `(String, HealthCheckResult)` tuple |

For path params and query params, add `params(...)` section to the macro. For the `{status: "no_op"}` literal-shape response, either define a typed `NoOpResponse` struct with `ToSchema` or use `body = Object` and document inline.

#### 0.2 Add `ToSchema` derives to all health types
**File:** `crates/atomic-core/src/health/mod.rs`

The struct definitions currently have `#[derive(Debug, Clone, Serialize, Deserialize)]`. Add `utoipa::ToSchema` using the feature-gated pattern already established in `atomic-core` (see `crates/atomic-core/src/models.rs`):

```rust
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResult { ... }

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport { ... }

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixAction { ... }

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFix { ... }

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixResponse { ... }

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixRequest { ... }

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum FixTier { Safe, Low, Medium, High }

// Real variants — do NOT change them; only add the cfg_attr derive.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum HealthStatus { Healthy, NeedsAttention, Degraded, Unhealthy }
```

**File:** `crates/atomic-core/src/health/audit.rs`
```rust
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthFixLog { ... }

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredHealthReport { ... }
```

`atomic-core` already has `utoipa` as an *optional* dep behind `[features] openapi = ["utoipa"]` in `crates/atomic-core/Cargo.toml`. **Do not add utoipa unconditionally** — use `cfg_attr` throughout so non-openapi consumers compile cleanly. `atomic-server` activates the feature via `atomic-core = { features = ["openapi", ...] }` in its own Cargo.toml.

#### 0.3 Register health paths and schemas in ApiDoc
**File:** `crates/atomic-server/src/lib.rs`

In the `#[openapi(paths(...))]` block (around line 30), add:

```rust
// Health
routes::health::get_health_knowledge,
routes::health::run_health_fix,
routes::health::apply_manual_fix,
routes::health::undo_health_fix,
routes::health::get_health_history,
routes::health::get_recent_fixes,
routes::health::compute_single_check,  // added in Phase 1
```

In the `components(schemas(...))` block (around line 175), add:

```rust
// Health
atomic_core::health::HealthReport,
atomic_core::health::HealthCheckResult,
atomic_core::health::HealthStatus,
atomic_core::health::FixRequest,
atomic_core::health::FixResponse,
atomic_core::health::FixAction,
atomic_core::health::SkippedFix,
atomic_core::health::FixTier,
atomic_core::health::audit::StoredHealthReport,
atomic_core::health::audit::HealthFixLog,
routes::health::ManualFixRequest,
```

In the `tags(...)` block (around line 290), add:

```rust
(name = "health", description = "Knowledge base health checks and auto-remediation"),
```

#### 0.4 Verify spec generation

Regenerate the OpenAPI JSON and confirm health endpoints appear:

```bash
cargo run --bin export-openapi -p atomic-server -- openapi.json

# Verify health paths are present
jq '.paths | keys | map(select(startswith("/api/health")))' openapi.json
# Expected: ["/api/health/check/{name}", "/api/health/fix", "/api/health/fix/{check}/{item_id}",
#           "/api/health/fixes/recent", "/api/health/history", "/api/health/knowledge",
#           "/api/health/undo/{fix_id}"]

# Verify health schemas are registered
jq '.components.schemas | keys | map(select(startswith("Health") or startswith("Fix") or . == "ManualFixRequest"))' openapi.json
```

#### 0.5 Verify downstream consumers

- Hit `/scalar` in dev mode — confirm health section renders with all 7 endpoints, each with request/response schemas
- Rebuild iOS/Android typed client bindings (if automated via codegen) and verify no compile errors
- MCP bridge: check that `atomic-mcp` discovers health tools if it reflects on the OpenAPI surface

**Effort estimate:** 8–10 hours
- 2h — ToSchema derives on core types + Cargo.toml utoipa dep
- 3h — `#[utoipa::path]` annotations on all 6 existing handlers (plus the Phase 1 handler)
- 1h — `ApiDoc` registration in lib.rs
- 1h — spec regeneration, jq verification, `/scalar` smoke test
- 1–2h — fixing any `ToSchema` derivation issues (e.g., `HashMap<String, HealthCheckResult>` may need explicit schema hint; `DateTime<Utc>` needs a format attribute)

---

### Phase 1: Expandable Rows & Per-Check Actions (Week 1, ~35 hours)

#### 1.1 Backend: New single-check compute endpoint
**File:** `crates/atomic-server/src/routes/health.rs`

```rust
// POST /api/health/check/{check_name}
pub async fn compute_single_check(
    db: Db,
    path: web::Path<String>,
) -> HttpResponse {
    let check_name = path.into_inner();
    // Call atomic-core with just this check
    match health::compute_single_check(&db.0, &check_name).await {
        Ok(result) => HttpResponse::Ok().json(result),
        Err(e) => crate::error::error_response(e),
    }
}
```

**File:** `crates/atomic-core/src/health/mod.rs`

```rust
/// Compute a single health check by name.
pub async fn compute_single_check(
    core: &AtomicCore,
    check_name: &str,
) -> Result<(String, HealthCheckResult), AtomicCoreError> {
    let result = match check_name {
        // Sync checks — fetch raw data once, dispatch to the appropriate fn
        "embedding_coverage"
        | "tagging_coverage"
        | "content_overlap"
        | "source_uniqueness"
        | "wiki_coverage"
        | "semantic_graph_freshness"
        | "content_quality"
        | "orphan_tags"
        | "tag_health"
        | "contradiction_detection"
        | "boilerplate_pollution" => {
            let raw = core.storage().health_check_data_sync().await?;
            match check_name {
                "embedding_coverage"      => checks::embedding_coverage(&raw),
                "tagging_coverage"        => checks::tagging_coverage(&raw),
                "content_overlap"         => checks::content_overlap(&raw),
                "source_uniqueness"       => checks::source_uniqueness(&raw),
                "wiki_coverage"           => checks::wiki_coverage(&raw),
                "semantic_graph_freshness" => checks::semantic_graph_freshness(&raw),
                "content_quality"         => checks::content_quality(&raw),
                "orphan_tags"             => checks::orphan_tags(&raw),
                "tag_health"              => checks::tag_health(&raw),
                "contradiction_detection" => checks::contradiction_detection(&raw),
                "boilerplate_pollution"   => checks::boilerplate_pollution(&raw),
                _ => unreachable!(),
            }
        }
        // Async check — requires per-atom DB lookups
        "broken_internal_links" => compute_link_check(core).await?,
        _ => return Err(AtomicCoreError::Validation(
            format!("Unknown health check: {}", check_name),
        )),
    };
    Ok((check_name.to_string(), result))
}
```

**Backend routes registration:** Update `crates/atomic-server/src/routes/mod.rs` to add `POST /api/health/check/{check_name}` into the health scope (alongside the other health routes).

#### 1.2 Frontend: Refactor to component-per-row
**File:** `src/components/dashboard/widgets/HealthCheckRow.tsx` (new)

```typescript
interface HealthCheckRowProps {
  checkName: string;
  check: HealthCheckResult;
  isExpanded: boolean;
  onToggleExpand: (name: string) => void;
  onRun: (name: string) => void;
  onReview: (name: string) => void;
  isRunning?: boolean;
}

export function HealthCheckRow({
  checkName,
  check,
  isExpanded,
  onToggleExpand,
  onRun,
  onReview,
  isRunning,
}: HealthCheckRowProps) {
  return (
    <div className="border-b border-white/5 py-3">
      {/* Header */}
      <div className="flex items-center gap-3">
        <CheckStatusIcon status={check.status} />
        
        {/* Label & score */}
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2">
            <span className="text-sm text-gray-300">
              {CHECK_LABELS[checkName] ?? checkName}
            </span>
            <span className="text-xs text-gray-600">
              {check.score}
            </span>
          </div>
          <ScoreBar score={check.score} />
        </div>

        {/* Right-align buttons */}
        <button
          onClick={() => onRun(checkName)}
          disabled={isRunning}
          className="text-gray-500 hover:text-gray-300 p-1 transition-colors"
          title="Run this check"
          aria-label={`Run ${CHECK_LABELS[checkName]} check`}
        >
          <Play className="w-4 h-4" />
        </button>

        {check.requires_review && (
          <button
            onClick={() => onReview(checkName)}
            className="text-gray-500 hover:text-gray-300 p-1 transition-colors"
            title="Review samples"
            aria-label={`Review samples for ${CHECK_LABELS[checkName]}`}
          >
            <Search className="w-4 h-4" />
          </button>
        )}

        <button
          onClick={() => onToggleExpand(checkName)}
          className="text-gray-500 hover:text-gray-300 p-1 transition-colors"
        >
          {isExpanded ? (
            <ChevronUp className="w-4 h-4" />
          ) : (
            <ChevronDown className="w-4 h-4" />
          )}
        </button>
      </div>

      {/* Description */}
      {!isExpanded && (
        <p className="text-xs text-gray-500 pl-5 mt-1">
          {CHECK_DESCRIPTIONS[checkName]?.(check.data)}
        </p>
      )}

      {/* Expanded detail */}
      {isExpanded && (
        <div className="mt-3 pl-5 space-y-2">
          <p className="text-xs text-gray-500">
            {CHECK_DESCRIPTIONS[checkName]?.(check.data)}
          </p>

          {check.auto_fixable && (
            <label className="flex items-center gap-2 text-xs text-gray-400">
              <input type="checkbox" defaultChecked className="rounded" />
              <span>Include in auto-fix batch</span>
            </label>
          )}

          {check.requires_review && (
            <button
              onClick={() => onReview(checkName)}
              className="text-xs text-blue-400 hover:text-blue-300"
            >
              View samples →
            </button>
          )}
        </div>
      )}
    </div>
  );
}
```

**File:** `src/components/dashboard/widgets/HealthWidget.tsx` (refactored)

```typescript
export function HealthPanel() {
  const [report, setReport] = useState<HealthReport | null>(null);
  const [expandedChecks, setExpandedChecks] = useState<Set<string>>(new Set());
  const [runningCheck, setRunningCheck] = useState<string | null>(null);
  const [showReviewModal, setShowReviewModal] = useState<string | null>(null);
  // ... other state

  const runSingleCheck = useCallback(async (checkName: string) => {
    setRunningCheck(checkName);
    try {
      const result = await getTransport().invoke<{
        name: string;
        result: HealthCheckResult;
      }>('health_check_single', { check_name: checkName });
      
      // Update report with new check result
      setReport((prev) => {
        if (!prev) return prev;
        return {
          ...prev,
          checks: { ...prev.checks, [checkName]: result.result },
        };
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Check failed');
    } finally {
      setRunningCheck(null);
    }
  }, []);

  const toggleExpandCheck = useCallback((checkName: string) => {
    setExpandedChecks((prev) => {
      const next = new Set(prev);
      if (next.has(checkName)) next.delete(checkName);
      else next.add(checkName);
      return next;
    });
  }, []);

  return (
    <div className="p-4 bg-[#252525] rounded border border-white/5 space-y-3">
      {/* Header & score bar */}
      {/* ... existing code ... */}

      {/* Per-check rows */}
      {issueChecks.length > 0 ? (
        <div className="space-y-0">
          {issueChecks.map((checkName) => {
            const check = report.checks[checkName];
            if (!check) return null;
            return (
              <HealthCheckRow
                key={checkName}
                checkName={checkName}
                check={check}
                isExpanded={expandedChecks.has(checkName)}
                onToggleExpand={toggleExpandCheck}
                onRun={runSingleCheck}
                onReview={(name) => setShowReviewModal(name)}
                isRunning={runningCheck === checkName}
              />
            );
          })}
        </div>
      ) : (
        /* healthy state */
      )}

      {showReviewModal && (
        <HealthReviewModal
          reportCheck={report.checks[showReviewModal]}
          checkName={showReviewModal}
          onClose={() => setShowReviewModal(null)}
          onResolved={fetchHealth}
        />
      )}
    </div>
  );
}
```

**Command map:** Add `health_check_single` to `src/lib/transport/command-map.ts`.

#### 1.3 Update HealthReviewModal for row-triggered opens
- Accept `checkName: string` as a new required prop alongside the existing `report`, `onClose`, and `onResolved`
- Use `checkName` to set initial `activeTab` state so the modal opens on the correct category
- Update the prop interface in `HealthReviewModal.tsx` to `{ report: HealthReport; checkName: string; onClose: () => void; onResolved: () => void }`

**Effort estimate:** 35 hours (backend endpoint, TS types, component refactor, testing)

---

### Phase 2: Trends, Filtering, Sorting (Week 2, ~30 hours)

#### 2.1 Backend: Enhance HealthReport with metadata
**File:** `crates/atomic-core/src/health/mod.rs`

```rust
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub overall_score: u32,
    pub overall_status: String,  // Keep as String — "healthy" | "needs_attention" | "degraded" | "unhealthy"
    pub computed_at: String,  // ISO 8601
    pub atom_count: i32,
    pub checks: HashMap<String, HealthCheckResult>,
    pub auto_fixable: i32,
    pub requires_review: i32,
    pub previous_score: Option<u32>,  // Added in Phase 2 for trending; None on first run
}

**File:** `crates/atomic-core/src/storage/sqlite/health.rs`

Enhance `get_latest_health_report_impl` to fetch previous score from history table.

#### 2.2 Frontend: Add trend computation and filter UI
**File:** `src/components/dashboard/widgets/HealthWidget.tsx`

```typescript
interface FilterState {
  severity: 'all' | 'critical' | 'warning' | 'needs-attention' | 'healthy';
  autoFixable: 'all' | 'fixable' | 'manual-only';
  sort: 'score-asc' | 'score-desc' | 'alphabetical' | 'affected-count';
}

// Trend indicator helper
function getTrend(check: HealthCheckResult, previousScore?: number): '↑' | '↓' | '→' {
  if (!previousScore) return '→';
  if (check.score > previousScore) return '↑';
  if (check.score < previousScore) return '↓';
  return '→';
}

// Severity badge
function getSeverityBadge(score: number): string {
  if (score <= 40) return '🔴';
  if (score <= 70) return '🟠';
  if (score <= 85) return '🟡';
  return '🟢';
}

// Filtered and sorted checks
function getVisibleChecks(
  report: HealthReport,
  filter: FilterState
): string[] {
  let visible = CHECK_ORDER.filter((k) => {
    const check = report.checks[k];
    if (!check || check.status === 'ok') return false;

    // Severity filter
    if (filter.severity !== 'all') {
      const score = check.score;
      const severity =
        score <= 40 ? 'critical' :
        score <= 70 ? 'warning' :
        score <= 85 ? 'needs-attention' : 'healthy';
      if (severity !== filter.severity) return false;
    }

    // Auto-fixable filter
    if (filter.autoFixable === 'fixable' && !check.auto_fixable) return false;
    if (filter.autoFixable === 'manual-only' && check.auto_fixable) return false;

    return true;
  });

  // Sorting
  if (filter.sort === 'score-asc') {
    visible.sort((a, b) => report.checks[a].score - report.checks[b].score);
  } else if (filter.sort === 'score-desc') {
    visible.sort((a, b) => report.checks[b].score - report.checks[a].score);
  } else if (filter.sort === 'alphabetical') {
    visible.sort((a, b) => CHECK_LABELS[a].localeCompare(CHECK_LABELS[b]));
  } else if (filter.sort === 'affected-count') {
    visible.sort((a, b) => {
      const countA = extractCount(report.checks[a]);
      const countB = extractCount(report.checks[b]);
      return countB - countA;
    });
  }

  return visible;
}
```

#### 2.3 HealthCheckRow enhancement: Timestamps and trends
```typescript
// Update row header to show trend & last-run time
<div className="flex items-center gap-2 text-xs text-gray-600">
  <span className="text-green-400">{getTrend(check, previousScore)}</span>
  <span>Last: 2h ago</span>
</div>

// Severity badge before icon
<span className="text-lg">{getSeverityBadge(check.score)}</span>
```

**Effort estimate:** 30 hours (backend report enrichment, filter logic, sorting, UI layout)

---

### Phase 3: Advanced UX (Modals, Undo, Export, Keyboard Shortcuts, Animations) (Week 3, ~25 hours)

#### 3.1 Confirmation modal for batch fixes
```typescript
interface FixConfirmationModalProps {
  pending: { label: string; check: string }[];
  report: HealthReport;
  onConfirm: (selectedChecks: Set<string>) => void;
  onCancel: () => void;
}

// Shows grouped summary:
// "This will: retag 26 atoms, remove 9 duplicate clones, trim 20 long atoms"
// With per-fix checkbox
```

#### 3.2 Undo stack & toast
```typescript
// Undo stack entries: each holds the fix_id (from HealthFixLog) and a human label
const [undoStack, setUndoStack] = useState<{ fix_id: string; label: string }[]>([]);

// After fix applied, push { fix_id, label } onto the stack and show toast
// fix_id comes from the HealthFixLog.id returned by log_fix (server includes it in FixResponse.fix_id)
// Toast auto-dismisses in 10s (clearTimeout on click); Undo calls POST /api/health/undo/{fix_id}

#### 3.3 Export to markdown
```typescript
function exportHealthReport(report: HealthReport): string {
  let md = `# Knowledge Base Health Report\n\n`;
  md += `**Overall Score:** ${report.overall_score}/100\n`;
  md += `**Generated:** ${new Date(report.computed_at).toLocaleString()}\n\n`;

  // Per-check section with data
  for (const check of CHECK_ORDER) {
    const result = report.checks[check];
    if (!result) continue;
    md += `## ${CHECK_LABELS[check]}\n`;
    md += `**Score:** ${result.score}/100\n`;
    md += `**Status:** ${result.status}\n`;
    md += `${CHECK_DESCRIPTIONS[check]?.(result.data)}\n\n`;
  }

  return md;
}
```

#### 3.4 Keyboard shortcuts
- `r`: Refresh all checks
- `f`: Apply fixes (open confirmation modal)
- `1–9`: Expand nth check in filtered list
- `?`: Show help overlay

#### 3.5 Animations
```css
/* Smooth score bar fill */
.score-bar {
  transition: width 600ms cubic-bezier(0.34, 1.56, 0.64, 1); /* ease-out */
}

/* Row expand/collapse */
[data-expanded="true"] {
  animation: slideDown 200ms ease-out;
}

@keyframes slideDown {
  from {
    opacity: 0;
    transform: translateY(-8px);
  }
  to {
    opacity: 1;
    transform: translateY(0);
  }
}
```

**Effort estimate:** 25 hours (modals, UX polish, keyboard handling, animations)

---

## Files & Components to Change

### Backend
- `crates/atomic-server/src/routes/health.rs` — Add `compute_single_check` endpoint; annotate all handlers with `#[utoipa::path(...)]` (Phase 0)
- `crates/atomic-server/src/lib.rs` — Register health paths, schemas, and tag in `ApiDoc` (Phase 0)
- `crates/atomic-core/src/health/mod.rs` — `compute_single_check()` function + `ToSchema` derives on all health types (Phase 0)
- `crates/atomic-core/src/health/audit.rs` — `ToSchema` derives on `HealthFixLog`, `StoredHealthReport` (Phase 0)
- `crates/atomic-core/Cargo.toml` — Add `utoipa` dependency (Phase 0)
- `crates/atomic-core/src/storage/sqlite/health.rs` — Query previous report score for trending
- `crates/atomic-server/src/routes/mod.rs` — Route registration

### Frontend
- `src/components/dashboard/widgets/HealthWidget.tsx` — Main refactor (expandable, filtering, actions)
- `src/components/dashboard/widgets/HealthCheckRow.tsx` — NEW (per-row component)
- `src/components/dashboard/widgets/HealthReviewModal.tsx` — Minor: accept checkName prop
- `src/components/dashboard/widgets/HealthConfirmModal.tsx` — NEW (batch fix confirmation)
- `src/components/dashboard/widgets/HealthExportModal.tsx` — NEW (markdown export)
- `src/lib/transport/command-map.ts` — Add `health_check_single` command
- `src/styles/animations.css` — NEW or extend (score bar animations)

---

## Data Flow & Interfaces

### Single-Check Compute Flow
```
User clicks "Run" button on Tagging row
  → onRun('tagging_coverage')
  → POST /api/health/check/tagging_coverage
  → compute_single_check(core, 'tagging_coverage')
  → fetch raw data, run just tagging check
  → return HealthCheckResult
  → update report.checks['tagging_coverage']
  → row re-renders with new score, animate bar
```

### Batch Fix with Confirmation
```
User clicks "Apply N fixes"
  → open FixConfirmationModal
  → show checklist of pending.map(fix_action)
  → user can toggle individual fixes
  → user clicks "Confirm"
  → POST /api/health/fix { mode: 'auto', include_medium, dry_run }
  → FixResponse + new_score
  → update report
  → show toast: "✅ Fixed 5 items. Score 80 → 85"
  → Undo button available for 10s
```

### Trend Computation
```
GET /api/health/knowledge
  → HealthReport { overall_score, checks, computed_at, previous_score? }
  → for each check, if previous_score exists:
     delta = current - previous
     trend = delta > 0 ? '↑' : delta < 0 ? '↓' : '→'
  → display trend icon next to score
```

---

## Configuration & Deployment Notes

### Environment
- No new env vars needed; all features toggle on frontend state
- Backend endpoint (`compute_single_check`) available on all deployments

### Feature Flags
None required; all features are additive and don't conflict with existing UI.

### Accessibility
- All buttons have aria-labels and keyboard focus states
- Modals use `dialog` ARIA role
- Color not the only indicator (use icons + text)
- Keyboard shortcuts documented in `?` overlay

---

## Testing & Validation Plan

### E2E Tests (Playwright)
1. **Per-row run button**
   - Click Run on a single check
   - Verify spinner appears
   - Verify score updates when response arrives
   - Verify can click Run multiple times without errors

2. **Expandable rows**
   - Click row header → expands and shows details + buttons
   - Click again → collapses
   - Expand state persists until user collapses

3. **Batch fix confirmation**
   - Click "Apply N fixes" → modal opens
   - Each fix has unchecked checkbox
   - User can toggle individual fixes
   - Click Confirm → fixes run, new score displayed
   - Toast shows "Fixed N items. Score X → Y"
   - Click Undo → reverts (calls undo endpoint)

4. **Filtering & sorting**
   - Change severity filter → only matching checks displayed
   - Change sort order → checks reorder
   - Verify all checks still displayed when filter cleared

5. **Sample review**
   - Click "Review samples" on a failing check
   - Modal opens showing 3–5 sample atoms
   - Each sample has "Fix", "Dismiss", "Open atom" buttons
   - Quick actions work as expected

### Commands
```bash
# Run E2E tests
npm run playwright:test -- --grep "health.*ui"

# Run unit tests for helper functions
npm run test -- HealthWidget

# Manual testing flow
npm run dev:mobile:ios &
# or
make dev-desktop-fast

# In app:
1. Navigate to dashboard
2. Open Health panel
3. Test each UI interaction as per E2E list above
```

### Verification
- All checks remain sortable/filterable after batch fix
- Score bars animate smoothly on update
- Keyboard shortcuts work (test with `r`, `f`, `?`)
- No console errors during interactions
- Modal accessibility tested with screen reader (NVDA/Voiceover)

---

## Risks, Assumptions, and Open Questions

### Risks
1. **Performance:** Fetching `HealthReport` + historical data on every refresh could be slow for large KBs. Mitigation: Memoize last report, fetch history lazily on first filter/trend request.

2. **Undo semantics:** If user applies fixes, then runs a check that changes scores, what happens to undo? Fix: Undo button is only valid for 10s immediately after fix; once new data fetched, undo is stale.

3. **Concurrent fixes:** User clicks "Run" on a single check while batch fix is running. Mitigation: Disable Run buttons while batch fix in progress.

### Assumptions
1. Backend will have `POST /api/health/check/{check_name}` endpoint for single-check compute (requirement for Phase 1).
2. History API already exists and returns previous reports (assumed; verify with backend team).
3. Undo endpoint (`POST /api/health/undo/{fix_id}`) already works (implemented in Phase 1 of prior sprint).
4. HealthCheckResult data shape is stable; no breaking changes to check payloads.

### Open Questions
1. **Sample review:** Currently `HealthReviewModal` shows pairs for overlaps/dupes. For other checks (e.g., untagged atoms, too-long atoms), how should samples be structured? Should we add a new `/api/health/samples/{check}` endpoint?

2. **Export location:** Where should markdown export be saved? Recommendation: web uses `<a download>` with a `data:text/markdown` URI; Tauri desktop uses `@tauri-apps/plugin-dialog` (`save()`) + `@tauri-apps/plugin-fs` (`writeTextFile()`) to avoid CSP-blocked blob downloads. Branch on `window.__TAURI__` at runtime.

3. **Trend baseline:** Should we compare to the *previous run* or a *rolling average* over 7 days? Recommend previous run (simpler, clearer signal).

4. **Mobile:** Filter/sort UI is complex on mobile. Should we hide advanced filters on small screens and show only "Severity" dropdown? Or move to slide-over panel?

---

## LOE & Effort Estimate

| Phase | Task | Hours | Notes |
|-------|------|-------|-------|
| 0 | Backend: `cfg_attr` ToSchema derives on health core types | 2 | Feature-gated pattern; ~10 structs/enums in mod.rs + audit.rs |
| 0 | Backend: `#[utoipa::path]` on health handlers | 3 | Six existing + one Phase 1 addition |
| 0 | Backend: Register paths/schemas/tags in ApiDoc | 1 | Edit `atomic-server/src/lib.rs` |
| 0 | Verify generated spec, regen clients | 2 | `export-openapi`, jq checks, `/scalar` smoke test |
| 0 | Fix ToSchema derivation edge cases | 2 | DateTime formats, HashMap schema hints |
| **Phase 0 total** | | **10** | Prerequisite — must ship before external clients use health APIs |
| 1 | Backend: single-check endpoint (all 11 + async link check) | 10 | Full dispatch matrix + `broken_internal_links` async path |
| 1 | Frontend: HealthCheckRow component | 12 | Component extraction, state per-row, buttons |
| 1 | Frontend: HealthWidget refactor | 10 | Migrate to per-row render, integrate new state |
| 1 | Integration testing, fixes | 6 | E2E tests for Run, Review, expand/collapse |
| **Phase 1 total** | | **38** | |
| 2 | Backend: report enrichment (prev score) | 7 | Query history, add field, handle first-run NULL, update TS interface |
| 2 | Frontend: filter/sort logic | 10 | Data structures, comparison functions |
| 2 | Frontend: filter UI, severity badges | 8 | Layout, state management |
| 2 | Integration testing | 7 | Filter combinations, sorting verified |
| **Phase 2 total** | | **32** | |
| 3 | FixConfirmationModal component | 6 | Modal boilerplate, checkbox logic |
| 3 | Undo stack & toast integration | 7 | Toast library setup, undo flow, aria-live, timeout cleanup |
| 3 | Export function + UI (web + Tauri paths) | 5 | Markdown generation, plugin-fs for desktop, data: for web |
| 3 | Keyboard shortcuts | 3 | Event listeners, help overlay |
| 3 | Animations & micro-interactions | 4 | CSS transitions, polish |
| 3 | E2E and polish | 3 | Final testing, edge cases |
| **Phase 3 total** | | **28** | |
| **Grand total** | | **108** | ~3–4 weeks at 30 hrs/week (Phase 0 can run in parallel with Phase 1 frontend work once `cfg_attr` pattern settled) |

---

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-05-01 | Phased delivery (foundation → trends → polish) | Allows early feedback and MVP deployment after Phase 1 |
| 2026-05-01 | Per-row Run button over modal | Faster iteration; avoid extra click depth for common operation |
| 2026-05-01 | Severity filter over custom query language | Simpler UX; covers 90% of user needs |
| 2026-05-01 | 10s undo timeout vs. infinite stack | Prevents confusion; aligns with user mental model (like Ctrl+Z in editors) |
| 2026-05-01 | Markdown export vs. JSON/CSV | Markdown is human-readable, shareable, LLM-friendly for context |
| 2026-05-01 | Add Phase 0 for OpenAPI spec coverage | Health endpoints are invisible to external SDK/MCP/iOS clients until registered in `ApiDoc`; Phase 0 unblocks all downstream consumers and is a prerequisite for Phase 1's new `compute_single_check` endpoint being usable outside the web UI |
| 2026-05-01 | Use `cfg_attr(feature = "openapi", derive(utoipa::ToSchema))` for health types | Matches existing atomic-core convention (models.rs); utoipa is already an optional dep behind `openapi` feature — unconditional derive breaks non-openapi consumers |

---

## Summary

This plan structures a high-UX dashboard enhancement into four phased releases:

0. **Phase 0 (Prerequisite):** OpenAPI spec coverage for all `/api/health/*` endpoints — adds utoipa annotations, `ToSchema` derives, and `ApiDoc` registration so external SDK/MCP/mobile clients can consume health APIs.
1. **Phase 1 (MVP):** Expandable rows with Run/Review per-check, lays foundation for remaining features.
2. **Phase 2 (Insight):** Trending, filtering, sorting so users can prioritize high-impact fixes.
3. **Phase 3 (Polish):** Confirmations, undo, export, keyboard shortcuts, animations — delightful UX.

**Recommendation:** Start Phase 0 immediately in parallel with Phase 1 frontend work — backend annotation work doesn't block React refactor. Settle the `cfg_attr` pattern (Phase 0 §0.2) before merging Phase 1 to avoid feature-flag conflicts. Phase 2 and 3 follow incrementally.

Estimated **~108 hours total** across all four phases, with Phase 0 (~10h) deliverable within a day or two and unblocking all external API consumers.
