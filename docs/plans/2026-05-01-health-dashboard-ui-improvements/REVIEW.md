# Plan Review — Knowledge Health Dashboard UI Improvements

**Source plan:** [plan.md](./plan.md)  
**Reviewed:** 2026-05-01  
**Reviewer:** Plan Review Command (plan-review/reviewer, claude-opus-4-7)  
**Overall Assessment:** ✅ Approved — all critical/major findings applied to plan.md

---

## Executive Summary

The plan correctly diagnoses the missing OpenAPI surface and the four-phase structure matches actual codebase state. Phase 0 and Phase 1 backend/frontend breakdowns are well-grounded. However, five Critical/Major accuracy defects must be resolved before execution: the plan prescribes unconditional `ToSchema` derives that will break atomic-core's feature-flag pattern, uses wrong `HealthStatus` enum variants, silently introduces a contract-breaking type change to `HealthReport`, references a non-existent `AtomicCoreError` variant, and leaves the single-check dispatch matrix dangerously incomplete.

---

## 1. Executive Summary

| | |
|---|---|
| **Strengths** | Phase 0 rationale correct — health routes genuinely absent from ApiDoc (verified). All 7 handler names and route paths accurate. Backend additions (previous_score, compute_single_check) are legitimate gaps. Phased delivery produces usable value after each phase. |
| **Critical issues** | 5 (see Section 2) |
| **Major issues** | 8 (see Section 3) |
| **Minor issues** | 6 (see Section 4) |
| **LOE** | 100h understated; revised estimate 108–112h |

---

## 2. Critical Issues

### C1 — Phase 0 `ToSchema` derive pattern breaks atomic-core feature flag
**Dimension:** Accuracy  
**Severity:** 🔴 Critical  
**Location:** Phase 0 §0.2 — ToSchema derive block  

**Finding:** Plan instructs adding `#[derive(..., ToSchema)]` directly (unconditionally) to all health structs in `atomic-core`. But `atomic-core` already guards every `ToSchema` derive behind `#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]` — utoipa is an *optional* dep behind `[features] openapi = ["utoipa"]` in `crates/atomic-core/Cargo.toml`. An unconditional derive will fail to compile whenever `openapi` feature is off (e.g., in any crate that depends on `atomic-core` without the feature).

**Evidence:** `crates/atomic-core/Cargo.toml` — `utoipa = { version = "5", features = ["preserve_order"], optional = true }` + `[features] openapi = ["utoipa"]`; `crates/atomic-core/src/models.rs` uses `cfg_attr` throughout.

**Recommendation:** Replace all `ToSchema` derives in Phase 0 with `#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]`. Do NOT add utoipa as an unconditional dep. The same pattern must apply to `audit.rs` types.

---

### C2 — Wrong `HealthStatus` enum variants in Phase 0 code snippet
**Dimension:** Accuracy  
**Severity:** 🔴 Critical  
**Location:** Phase 0 §0.2 — HealthStatus snippet  

**Finding:** Plan shows `pub enum HealthStatus { Ok, Warning, Critical }`. The real enum has four variants: `Healthy, NeedsAttention, Degraded, Unhealthy` (with snake_case serde rename). Copy-pasting the plan's snippet would silently change the variant set, break the `score → status` mapping, and corrupt `overall_status` strings used by the frontend.

**Evidence:** `crates/atomic-core/src/health/mod.rs` L45–71.

**Recommendation:** Fix the code snippet to `{ Healthy, NeedsAttention, Degraded, Unhealthy }` with existing serde renames. Explicitly state "only derive is being added — no variant changes."

---

### C3 — Phase 2 `HealthReport` change silently breaks the `overall_status` contract
**Dimension:** Consistency  
**Severity:** 🔴 Critical  
**Location:** Phase 2 §2.1 HealthReport code block  

**Finding:** The Phase 2 struct snippet changes `overall_status: String` to `overall_status: HealthStatus` as a side effect of adding `previous_score`. This is an unannounced breaking change — frontend TypeScript, stored `report_json` rows in the `health_reports` table, iOS/Android typed bindings, and all MCP consumers will break. No migration plan is mentioned.

**Evidence:** `crates/atomic-core/src/health/mod.rs` L90 — `overall_status: String`, populated via `.as_str().to_string()` at L258.

**Recommendation:** Either (a) keep `overall_status: String` and only add `previous_score: Option<u32>` in Phase 2, or (b) make the type change an explicit, planned step with a decision-log entry, frontend TS update, stored-JSON migration, and API version bump.

---

### C4 — `compute_single_check` dispatch matrix dangerously incomplete
**Dimension:** Completeness  
**Severity:** 🔴 Critical  
**Location:** Phase 1 §1.1 compute_single_check match arm  

**Finding:** The code snippet handles only `embedding_coverage` and `tagging_coverage` with `// ...etc`. The real check list is **11 names** (`content_overlap`, `embedding_coverage`, `tagging_coverage`, `source_uniqueness`, `wiki_coverage`, `semantic_graph_freshness`, `content_quality`, `orphan_tags`, `tag_health`, `contradiction_detection`, `boilerplate_pollution`) plus the async-only `broken_internal_links` — which cannot be dispatched via a sync `checks::X(&raw)` call because it needs `compute_link_check(core).await` with multiple async DB lookups. The `// ...etc` placeholder hides this complexity.

**Evidence:** `crates/atomic-core/src/health/checks.rs` L12–418 (11 sync checks); `compute_link_check` in `mod.rs` L315 (async, per-atom).

**Recommendation:** Enumerate all 11 names explicitly in the plan. Special-case `broken_internal_links` to call `compute_link_check(core).await`. Mark `contradiction_detection` as stub if not yet implemented. Verify CHECK_ORDER in HealthWidget.tsx covers all 11 names.

---

### C5 — Non-existent `AtomicCoreError::InvalidInput` variant
**Dimension:** Accuracy  
**Severity:** 🔴 Critical  
**Location:** Phase 1 §1.1 compute_single_check error return  

**Finding:** Plan uses `AtomicCoreError::InvalidInput(...)`. This variant does not exist. The real variants are: `Database, Provider, Configuration, NotFound, Validation, Io, Json, Lock, Conflict, Embedding, Search, Wiki, Clustering, Compaction, Ingestion, DatabaseOperation`.

**Evidence:** `crates/atomic-core/src/error.rs`.

**Recommendation:** Replace with `AtomicCoreError::Validation(format!("Unknown health check: {}", check_name))`.

---

## 3. Major Issues

### M1 — `health_check_data_sync` called as free function; it's a storage method
**Dimension:** Clarity / Accuracy  
**Location:** Phase 1 §1.1  

Plan calls `health_check_data_sync(core).await?`. It is actually a method on the storage trait: `core.storage().health_check_data_sync().await`. Update the snippet.

---

### M2 — `undoStack: FixResponse[]` is incoherent
**Dimension:** Accuracy / Consistency  
**Location:** Phase 3 §3.2  

The undo endpoint requires a single `fix_id`. `FixResponse` contains `actions_taken: Vec<FixAction>`, `skipped`, `new_score` — no `fix_id`. If a batch fix produces N actions, it's unclear which id to pop. Options: (a) `undoStack: FixAction[]` — pop last action id; or (b) `undoStack: { fix_id: string; label: string }[]` keyed from `HealthFixLog.id` returned after `log_fix`. Decide and document.

---

### M3 — URL parameter name inconsistency
**Dimension:** Consistency  
**Location:** Phase 0 table (col "Path"), Phase 1 §1.1 handler comment  

Table shows `{name}`; handler comment shows `{check_name}`; routes/mod.rs registration not shown in §0.3. Pick one name consistently across handler signature, route config, and ApiDoc annotation.

---

### M4 — `HealthReviewModal` prop signature mismatch
**Dimension:** Consistency / Completeness  
**Location:** Phase 1 §1.3; HealthWidget refactor snippet  

HealthWidget snippet passes `reportCheck={report.checks[showReviewModal]}` and `checkName={showReviewModal}` to `HealthReviewModal`, but the existing modal takes `{ report, onClose, onResolved }` — not a single `reportCheck` + `checkName`. Phase 1 §1.3 says only "accept checkName prop to pre-select tab" without documenting modal tab structure or the full new interface. Define the complete new props interface.

---

### M5 — Severity badge thresholds conflict with existing HealthStatus scale
**Dimension:** Consistency**  
**Location:** Plan L93 (Executive Summary), Plan L566–570 (`getSeverityBadge`), Design Principles  

New severity badges use `0–40 🔴 / 41–70 🟠 / 71–85 🟡 / 86–100 🟢`. Existing `HealthStatus::from_score` mapping uses `<50 Unhealthy / 50–69 Degraded / 70–89 NeedsAttention / ≥90 Healthy`. Two coexisting classification scales will confuse users — a score of 72 would show a 🟡 badge but green "Healthy" status text. Reconcile or explicitly document the divergence as intentional UX design.

---

### M6 — `CHECK_ORDER` coverage not verified
**Dimension:** Completeness  
**Location:** Throughout plan  

Plan extensively references `CHECK_ORDER`, `CHECK_LABELS`, and `CHECK_DESCRIPTIONS` constants but doesn't audit whether they currently cover all 11 real check names. If `boilerplate_pollution`, `broken_internal_links`, or `contradiction_detection` are absent from `CHECK_ORDER`, those checks will never render in the UI regardless of the backend work.

**Action:** Read `HealthWidget.tsx` L160–172 and verify or extend the constant.

---

### M7 — Markdown export via `<a>` tag blocked in Tauri
**Dimension:** Completeness / Risk**  
**Location:** Phase 3 §3.3  

Plan shows `<a href="data:...">` download. In production Tauri builds, `data:` blob downloads may be blocked by CSP or require `@tauri-apps/plugin-fs` / `plugin-dialog`. No Tauri-specific download path documented.

**Recommendation:** Add a conditional: web uses `<a download>`, Tauri uses `window.__TAURI__.dialog.save()` + `fs.writeTextFile()`.

---

### M8 — Phase 2 hardcoded `Last: 2h ago` contradicts backend plan
**Dimension:** Consistency  
**Location:** Phase 2 §2.3 HealthCheckRow snippet  

Row snippet shows hardcoded string `Last: 2h ago`. Phase 2 §2.1 claims the backend will store a `last_run` timestamp per check. Either compute the relative timestamp from real data or explicitly mark the hardcoded version as a Phase 2 placeholder to replace in Phase 3.

---

## 4. Minor Issues

| ID | Dimension | Location | Finding |
|----|-----------|----------|---------|
| m1 | Accuracy | Phase 2 §2.2, L557 | `getTrend(..., previousScore?: u32)` — `u32` is a Rust type, not valid TypeScript. Should be `number`. |
| m2 | Accuracy | Phase 0 Decision Log | "crate already transitively pulls utoipa via atomic-server" is reversed — atomic-server pulls atomic-core *with* `features = ["openapi"]`, activating utoipa inside atomic-core. The dep direction matters for feature wiring. |
| m3 | Realism | Phase 0, LOE table | Header says "~100–110 hours"; table and summary say exactly 100h; Phase 0 body says "8–10 hours" but table says 10h. Tighten to a single range. |
| m4 | Accuracy | Phase 0 §0.2 | Claim "utoipa only contributes schema metadata at compile time" — over-stated. utoipa generates Schema impls that are evaluated at spec-build time (binary runtime). Trivial cost, but not purely compile-time. |
| m5 | Completeness | Phase 0 §0.4 | `jq` expected paths list in verification step should note `compute_single_check` path only appears after Phase 1 ships, not at Phase 0 merge. |
| m6 | Consistency | Phase 3 §3.2 | Toast timeout: summary says "10s"; Testing §3 says "Undo button available for 10s"; no explicit `setTimeout` cleanup or cancellation on user interaction documented. |

---

## 5. Gaps and Missing Considerations

1. **No `cfg_attr` pattern documented for Phase 0** — all health type ToSchema derives must match the existing `models.rs` convention.
2. **`broken_internal_links` async path** in `compute_single_check` not addressed.
3. **Frontend `HealthReport` TS interface** update for `previous_score` (local interface in `HealthWidget.tsx` — not in a shared types file).
4. **`StoredHealthReport` and `HealthFixLog` ToSchema** also need `cfg_attr` treatment — not called out separately.
5. **SQLite migration story** if `HealthReport` JSON shape changes in stored `health_reports.report_json` rows.
6. **Command-map.ts `health_check_single` entry** — needs the full HTTP spec (method, path, bodyTransform) consistent with other entries, but no example provided.
7. **`AtomicCore` vs `Database` receiver** — plan's `compute_single_check(core: &AtomicCore)` matches existing pattern; confirm `db: Db` extractor in route handler unwraps correctly (other handlers use `db.0`).

---

## 6. Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Unconditional utoipa dep breaks non-openapi builds | High (if plan followed literally) | High — CI fails | Fix C1 before any Phase 0 work begins |
| `HealthStatus` variant swap silently corrupts data | High | High — wrong status for all existing atoms | Fix C2 immediately |
| `overall_status` type change breaks iOS/MCP clients | Medium (if plan followed literally) | High | Fix C3: keep as String in Phase 2 |
| `// ...etc` placeholder → missing checks at runtime | Certain | Medium — some rows never run | Fix C4: enumerate all 11 |
| `InvalidInput` variant → compile error | Certain | Low (caught by `cargo check`) | Fix C5 |
| Performance: history fetch on every refresh | Medium | Low (addressed in Risks §) | Plan already notes lazy-load mitigation — adequate |
| Tauri export blocked by CSP | High | Low (feature, not data loss) | Add plugin-fs path (M7) |

---

## 7. LOE Assessment

Original 100h is **optimistic**. Revised estimate:

| Phase | Plan | Revised | Notes |
|-------|------|---------|-------|
| 0 | 10h | 10–11h | cfg_attr pattern slightly more careful than unconditional |
| 1 | 35h | 38–40h | Full 11-check dispatch + async branch + modal signature fix |
| 2 | 30h | 32–34h | First-run NULL handling, potential JSON migration, TS interface update |
| 3 | 25h | 27–28h | Toast infra setup, Tauri CSP, a11y live region |
| **Total** | **100h** | **107–113h** | |

Phase 0 and Phase 1 frontend work can run in parallel only after C1's `cfg_attr` pattern is settled.

---

## 8. Action Items (Priority Order)

| Priority | Action | Rationale |
|----------|--------|-----------|
| 🔴 1 | Rewrite Phase 0 §0.2 to use `cfg_attr(feature = "openapi", derive(utoipa::ToSchema))` — do NOT add utoipa unconditionally | Matches atomic-core convention; prevents broken non-openapi builds |
| 🔴 1 | Fix `HealthStatus` variant list to `Healthy/NeedsAttention/Degraded/Unhealthy` in Phase 0 snippet | Wrong variants break score-to-status mapping |
| 🔴 1 | Keep `overall_status: String` in Phase 2; add only `previous_score: Option<u32>` | Avoids unannounced contract break |
| 🔴 1 | Enumerate all 11 check names + async branch for `broken_internal_links` in `compute_single_check` | `// ...etc` hides the real dispatch matrix |
| 🔴 1 | Replace `AtomicCoreError::InvalidInput` with `AtomicCoreError::Validation` | Non-existent variant — compile error |
| 🟠 2 | Normalize URL param: `{check_name}` everywhere | Prevents routing mismatch |
| 🟠 2 | Define `undoStack` semantics: `FixAction[]` keyed by `fix_id` | Current `FixResponse[]` doesn't surface a single `fix_id` |
| 🟠 2 | Reconcile severity badge thresholds with `HealthStatus` scale | Two conflicting health scales confuse users |
| 🟠 2 | Update `HealthReviewModal` signature in plan with full new prop interface | Current plan doesn't match real modal props |
| 🟡 3 | Audit `CHECK_ORDER` covers all 11 checks including recent additions | Missing names = invisible UI rows |
| 🟡 3 | Add Tauri-specific markdown export path (plugin-fs/plugin-dialog) | `data:` blob download may be blocked by Tauri CSP |
| 🟡 3 | Revise LOE to 108–112h range | Accounts for async branch, toast infra, JSON migration |

---

*Full reviewer session:* `/Users/brandonkiefer/.omp/agent/sessions/-projects-atomic/2026-05-01T15-06-56-660Z_1accb47a-6b128d88-23d84057-7f51.jsonl`
