# Wiki "Generate Update" Fails With `AppendToSection: heading '...' not found`

**Date:** 2026-04-30
**Status:** Analysis Complete — Implementation Pending
**Project:** atomic-core (wiki proposal loop)
**Severity:** High — blocks wiki updates entirely whenever the LLM targets a nested heading. Currently reproducing repeatedly on real articles.
**Request:** "The generate update failed with that error, this is repeated and needs to be analyzed and resolved." Error: `Wiki error: AppendToSection: heading 'Monday.com' not found. Existing headings: ['Overview', 'Tools and Systems', 'Process Maturity Assessment', 'Work Intake and Triage', 'Requirements and Readiness', 'WIP Management', 'Capacity Planning', 'Blocker Management', 'Roles in Project Management', 'Commitment and Estimation', 'Metrics and Measurement', 'Operational Cadences', 'Deployment Pipeline', 'Knowledge Management and Developer Enablement', 'AI Tools in Project Management', 'Structural Diagnosis', 'Recommended Action Sequence']`

---

## Executive Summary

`Monday.com` is almost certainly a **level-3 heading under `Tools and Systems`** in the current article. The section-ops applier only tracks **level-2** headings, so when the LLM correctly targets a subsection the update is rejected as "hallucinated" and the whole `strategy_propose` call aborts. No retry, no partial-apply, no fallback.

There is a secondary failure mode with the same shape: LLMs sometimes emit `AppendToSection { heading: "New Tool" }` intending to create a section, instead of `InsertSection`. Today both get the same terse error and the same hard abort.

**Recommended fix (primary, minimum diff):** broaden the applier to match headings at any level (H2–H6), keyed by trimmed text. This is a 3-line change in `find_section_idx` + `parse_sections` and unblocks every real-world article today.

**Recommended fix (secondary, small diff):** before the hard abort, if `AppendToSection` targets a heading that doesn't exist but an `InsertSection`-compatible slot is obvious (no `after_heading` ambiguity), coerce it to `InsertSection { after_heading: last_h2, heading, content }` OR drop only that op and continue. Pick one; my recommendation is **drop the bad op + continue** (keep the other valid ops) and surface a warning.

**Do not** try to solve this with prompt-engineering alone — the LLM is acting rationally given the headings block it's handed. The bug is on our side: we show the LLM only H2 headings, then reject anything it targets below H2.

---

## Current Architecture / Evidence

### The error path

Call chain (verified via `blast_radius`):

1. `strategy_propose` (`crates/atomic-core/src/wiki/mod.rs:160`) → `generate_section_ops_proposal` (`mod.rs:282`).
2. The LLM returns a JSON list of ops. Each op is deserialized through `WikiSectionOpWire::into_op` (`section_ops.rs:59`) into `WikiSectionOp`.
3. `apply_section_ops(existing, ops)` (`section_ops.rs:131`) runs each op in order. `AppendToSection` calls `find_section_idx(&sections, heading)`. On miss, it produces the exact error string the user is seeing.
4. A miss is an **unrecoverable error** for the whole proposal — `?` propagates out of `apply_section_ops` (`mod.rs:404-407`), out of `generate_section_ops_proposal`, out of `strategy_propose`. The UI's "Generate Update" surfaces it as the toast shown in the bug report.

### Why H3+ headings are invisible to the applier

`parse_sections` (`section_ops.rs:196-241`) only opens a new `Section` when `level == 2`:

```rust
if let Some((level, heading)) = parse_heading(line) {
    if level == 2 {
        // start new section
        continue;
    }
}
// otherwise push the whole line into the current section body
```

That means an article like:

```md
## Tools and Systems

### Monday.com

The team uses Monday.com for ticket tracking [3].

### Slack

...
```

Parses as **one** section (`Tools and Systems`) whose body contains the literal text `### Monday.com\n\nThe team uses...`. `find_section_idx(sections, "Monday.com")` returns `None` → hard error.

This is consistent with the `list_headings` output in the failure message: every entry in the shown list is a plausible H2 heading. No sub-headings are listed, which confirms H3s exist in the article but are filtered out both from the applier and from the LLM's view.

### Why the LLM emits `Monday.com`

`extract_current_headings` (`mod.rs:501-515`) is also H2-only:

```rust
if hashes == 2 && hashes < bytes.len() && bytes[hashes] == b' ' {
    headings.push(stripped[hashes + 1..].trim().to_string());
}
```

That list is injected into the user prompt as `CURRENT SECTION HEADINGS (use these values verbatim in your operations — do not paraphrase)` (`mod.rs:352-369`).

So the LLM sees **only H2 headings**, is told to use them verbatim, and then — because the article body still contains `### Monday.com` text visible in `CURRENT ARTICLE` — it (reasonably) targets `Monday.com` when the new source is specifically about Monday.com. The prompt never forbids H3 targets, never tells the model H3 is not rewritable, and never tells the model to append to the parent H2 for sub-topics.

### Why retries don't help

The call is one-shot. `generate_section_ops_proposal` → `call_llm_for_wiki_typed` → parse → `apply_section_ops` → first error aborts. The LLM is not re-prompted with the rejection reason. Every regeneration from the UI just re-rolls the same dice with the same prompt.

This also explains why the same article keeps failing. Structured outputs are deterministic-ish for identical inputs, and there's no feedback loop to push the model away from the miss.

### Secondary: `AppendToSection` used in place of `InsertSection`

Real failures also include the LLM inventing a brand-new H2 name under `AppendToSection` (e.g. an article about "Hiring Pipeline" getting `AppendToSection { heading: "Candidate Sourcing" }` when the article has no "Candidate Sourcing" section). Same code path, same hard abort. Fixing H3 targeting alone won't catch this; it needs the drop-and-continue or coerce-to-insert safety net described below.

### Downstream blast radius

`apply_section_ops` is used only by `generate_section_ops_proposal` in non-test code (`blast_radius apply_section_ops` → 3 files: `section_ops.rs`, `mod.rs`, and a plan doc). `WikiSectionOp` is serialized to SQLite/Postgres (`storage/sqlite/wiki.rs:390`, `storage/postgres/wiki.rs:780`) — those paths `serde_json::from_str` existing stored ops, so any on-disk format is unchanged as long as the enum variants keep their names and field shapes. Fix is safe at the DB boundary.

---

## Root Cause

Two independent bugs in the section-ops feature, both rooted in the same assumption that wiki articles are flat (H2-only):

1. **The parser/applier ignores sub-headings.** `parse_sections` only opens sections at H2, so anything nested is invisible to `find_section_idx`.
2. **The LLM prompt only advertises H2 headings.** The model can see H3s in the article body but is told "only these are valid targets," so when it picks one of the body-visible H3s it fails with "hallucinated heading" — but it hasn't hallucinated anything; our prompt lied about what's rewritable.

The second bug means even after we fix the applier, we should update the headings list to include sub-headings (or at minimum describe the nesting) so the LLM's mental model matches the applier's.

A third, unrelated robustness hole: there is no graceful degradation when any single op is invalid. The entire proposal dies on the first miss.

---

## Recommended Approach

Fix the parser and the prompt together, then add a single-op tolerance so one bad op doesn't nuke the whole update.

### Phase 1 — Make H3+ headings first-class targets (primary fix)

1. **`parse_sections`** (`section_ops.rs:196`): open a new `Section` on any heading level 2..=6, not just 2. Preserve `level` as today.
2. **`find_section_idx`** (`section_ops.rs:263`): unchanged logic (it already matches on `heading.trim()`), but now sub-sections are visible.
3. **`serialize_sections`** (`section_ops.rs:295`): re-emit each section with its stored `level` (`#` repeated `level` times). Today it likely hardcodes `##` — confirm and generalize. (Inspect before editing.)
4. **`InsertSection`** (`section_ops.rs:161`): today creates `Section { level: 2, ... }`. When `after_heading` points at an H3/H4 section, the new section should inherit that level (or stay H2 and be inserted after the parent H2 — pick the simpler behavior and document it). Recommendation: inherit the level of `after_heading`. If `after_heading` is `None`, default to H2 as today.
5. **`extract_current_headings`** (`mod.rs:501`): include all levels 2..=6. Render them with their level in the prompt, so the LLM sees the hierarchy:

   ```text
   ## Tools and Systems
     ### Monday.com
     ### Slack
   ## Process Maturity Assessment
   ```

6. Update the prompt to note that sub-headings are valid targets and that `InsertSection.after_heading` can be any existing heading at any level.

This is the minimum change that resolves the reported bug.

### Phase 2 — Soft-fail individual ops (secondary safety net)

Inside `apply_section_ops` (`section_ops.rs:131`):
- On a `find_section_idx` miss for `AppendToSection` or `ReplaceSection`, don't propagate. Log a structured warning (`tracing::warn!` with op, heading, existing headings), record the skipped op, and `continue`.
- Return the merged content **plus** a `Vec<SkippedOp>` describing what was dropped. Caller (`generate_section_ops_proposal`) logs and optionally surfaces a soft warning event.
- If **every** op is invalid, then — and only then — abort with the same error string we produce today.

Rationale: a typical proposal emits 1–5 ops. One bad op should not kill the 4 valid ones. This also makes the feature more resilient to prompt drift over time.

### Phase 3 — Optional: one-shot retry with rejection feedback

If Phase 2 is felt to be too lax (i.e. product wants every op to land), add a single retry in `generate_section_ops_proposal` where the second LLM call receives:

> Your previous response tried to `AppendToSection { heading: "Monday.com" }`, but that heading doesn't exist. Valid headings are: [...]. Retry.

Cap at one retry. This is not required to unblock the current bug and can be deferred.

---

## Implementation Plan

| Phase | Change | File | Notes |
|-------|--------|------|-------|
| 1.1 | Multi-level section parse | `crates/atomic-core/src/wiki/section_ops.rs` — `parse_sections`, `Section` | Accept `level` 2..=6 as section boundaries |
| 1.2 | Level-preserving serialize | `crates/atomic-core/src/wiki/section_ops.rs` — `serialize_sections` | Emit `#` × `level`. Verify current literal-`##` assumption first |
| 1.3 | InsertSection inherits level | `crates/atomic-core/src/wiki/section_ops.rs` — `apply_section_ops::InsertSection` | `level = sections[idx].level` when `after_heading` is `Some` |
| 1.4 | Prompt headings include hierarchy | `crates/atomic-core/src/wiki/mod.rs` — `extract_current_headings` | Return `(level, text)`; render with indent in `headings_block` |
| 1.5 | Prompt copy update | `crates/atomic-core/src/wiki/mod.rs` — `WIKI_UPDATE_SECTION_OPS_PROMPT` | Note that sub-headings are valid targets; drop the "## prefix" sentence or generalize it |
| 2.1 | Soft-fail single op | `crates/atomic-core/src/wiki/section_ops.rs` — `apply_section_ops` | Collect skipped ops; abort only if `ops.len() > 0 && skipped.len() == ops.len()` |
| 2.2 | Surface skipped-op warning | `crates/atomic-core/src/wiki/mod.rs` — `generate_section_ops_proposal` | Log + attach to `WikiProposalDraft` (consider a new `skipped_ops` field for UI display) |
| 3.1 (optional) | LLM retry with rejection feedback | `crates/atomic-core/src/wiki/mod.rs` — `generate_section_ops_proposal` | One retry only |

Ordering matters: **land Phase 1 first** (unblocks the reported bug alone). Phase 2 is a separate PR.

### Tests to add (Phase 1)

Extend `section_ops.rs`'s test module:

- `parse_sections_splits_h3_as_its_own_section` — input with `## A\n### A1\ntext\n### A2\nmore`, assert 3 sections (A as H2, A1/A2 as H3) and `find_section_idx("A1")` → `Some(1)`.
- `append_to_h3_section` — `AppendToSection { heading: "A1", content: "new text" }` on the above input produces content where `new text` lives under `### A1` only and `### A2` is byte-for-byte untouched.
- `serialize_preserves_levels` — round-trip `## A\n### A1\n### A2\n## B` with `NoChange` yields byte-identical output.
- `insert_section_after_h3_inherits_level` — `InsertSection { after_heading: Some("A1"), heading: "A1.1", content: ... }` produces `### A1.1` not `## A1.1`.

Extend `mod.rs`'s test module:

- `extract_current_headings_includes_h3` — with a multi-level article, returns the full list in document order with levels.

Tests to add (Phase 2):

- `apply_section_ops_tolerates_single_bad_op` — mix of one hallucinated-heading op and one valid op yields merged content reflecting the valid op + a non-empty skipped-ops report.
- `apply_section_ops_aborts_when_all_ops_invalid` — two bad ops → `Err` (unchanged posture).

### Things to verify before editing

1. `serialize_sections` (`section_ops.rs:295-325`) — confirm whether it emits a hardcoded `##` or already respects `Section.level`. If it already respects level, Phase 1.2 is free.
2. `Section` struct (`section_ops.rs:115-123`) — `level: u8` is already stored (confirmed from `parse_sections` setting `level`). Good, no struct change needed.
3. `wire_shape_*` tests — make sure none assert that `heading`-level anything is restricted to H2; Phase 1 shouldn't touch the wire format.
4. Any existing on-disk data: `SELECT value FROM settings WHERE key LIKE 'wiki%'` (per-DB settings) won't be affected — ops are stored post-apply, not as structured data that the parser re-reads.

---

## Files / Components To Change

| File | Change |
|------|--------|
| `crates/atomic-core/src/wiki/section_ops.rs` | Multi-level `parse_sections`, level-preserving `serialize_sections`, level-inheriting `InsertSection`, soft-fail in `apply_section_ops` (Phase 2), new tests |
| `crates/atomic-core/src/wiki/mod.rs` | `extract_current_headings` returns `(level, String)`, `headings_block` renders hierarchy, `WIKI_UPDATE_SECTION_OPS_PROMPT` text reflects new rules, optional retry loop (Phase 3) |

No changes to:
- `storage/sqlite/wiki.rs` / `storage/postgres/wiki.rs` — on-disk `WikiSectionOp` shape unchanged.
- `src/stores/wiki.ts` — TS `WikiSectionOp` union unchanged; all variants keep the same names.
- REST routes, command map, event normalizer — external API shape preserved.

---

## Data Flow / Interfaces

```
User clicks "Generate Update"
    ↓
strategy_propose(strategy, ctx, existing)                 [mod.rs:160]
    ↓
select_update_chunks() → (new_chunks, total_atom_count)
    ↓
generate_section_ops_proposal(ctx, existing, new_chunks)  [mod.rs:282]
    ├─ extract_current_headings(existing.content)          ← Phase 1.4
    │      needs to surface H3+ so prompt matches applier
    ├─ build user_content w/ CURRENT SECTION HEADINGS
    ├─ call_llm_for_wiki_typed(prompt, user_content, …)
    ├─ wire → enum conversion
    ├─ no-op short-circuit
    └─ apply_section_ops(existing.content, ops)            [section_ops.rs:131]
           ├─ parse_sections()                             ← Phase 1.1
           ├─ for op in ops:                               ← Phase 2.1 (soft-fail)
           │      find_section_idx()
           │      ↳ miss → WARN + skip (instead of hard error)
           └─ serialize_sections()                         ← Phase 1.2
```

Post-fix, the failure surfaces as a UI warning ("one op was skipped") rather than a blocking toast, and H3-targeted ops succeed silently.

---

## Configuration / Secrets / Deployment Notes

None. Pure Rust code change inside `atomic-core`. Ships with the next `atomic-server` / Tauri build. No migrations, no settings, no provider config changes.

---

## Testing / Validation Plan

1. `cargo test -p atomic-core wiki::section_ops` — new unit tests from Phase 1 + Phase 2 pass.
2. `cargo test -p atomic-core wiki` — existing wiki tests still pass (prompt string changes will hit `lint_wiki_section_ops_schema` which is schema-only, so should be unaffected).
3. Manual end-to-end against the specific failing article:
   - `sqlite3 databases/{uuid}.db "SELECT content FROM wiki_articles WHERE tag_id = '<project-management-tag>' AND superseded_at IS NULL;"` — confirm `### Monday.com` exists.
   - Trigger "Generate Update" from the UI.
   - Verify the proposal is produced, the Monday.com subsection receives the new citations, and no error toast fires.
4. Regression: trigger update on an article with only H2s; verify byte-for-byte output for untouched sections (existing `append_preserves_untouched_sections_byte_for_byte` covers this — should still pass).
5. Regression: articles with mixed `### `-looking content inside code fences — not a concern, because `parse_heading` already operates on the line-level, and the current test suite includes heading-detection-in-body cases implicitly via the byte-for-byte test. Add a new fixture if desired.

---

## Risks, Assumptions, and Open Questions

**Risk — level inheritance ambiguity.** If `after_heading` points at an H3 inside section "A", inserting after it as H3 is obvious. Inserting as H2 would split section "A". The proposal chooses "inherit `after_heading`'s level". Document this in the prompt so the LLM knows.

**Risk — byte-for-byte guarantee.** The current serializer is trusted to reproduce the article exactly under `NoChange`/partial edits (see `append_preserves_untouched_sections_byte_for_byte`). Changing `serialize_sections` to emit variable-width headings must maintain that guarantee for untouched sections. Verify by re-running that test after the change.

**Risk — prompt regression.** Rendering headings with indentation is a prompt-format change; structured-output LLMs typically tolerate this, but verify with at least one update run per provider (OpenRouter + Ollama) before calling it done.

**Assumption — `Monday.com` is indeed an H3.** Based on the visible H2 list and the nature of a "Tools and Systems" section with a tool name inside it, this is the most likely shape. If it turns out the LLM is inventing `Monday.com` entirely (not in the article at all), that's the secondary failure mode — Phase 2 (soft-fail) covers that case too. Either way, the fix bundle is correct.

**Open question — should skipped ops bubble to the UI?** Options: (a) silent warning in server logs only, (b) include a `skipped_ops` count in the proposal banner ("1 update was skipped — see logs"), (c) a full inline diff of what was dropped. Recommendation: (b). Cheap, honest, and maintainers can inspect logs for details.

**Open question — retry loop?** Phase 3 is optional. If Phase 1+2 eliminates the user-visible error in practice, don't add retry. If we still see meaningful drop rates on skipped ops, add Phase 3.

---

## LOE / Effort Estimate

| Phase | LOE | Confidence |
|-------|-----|------------|
| Phase 1 (multi-level parse, prompt headings, prompt copy) | ~1 focused day including tests | High |
| Phase 2 (soft-fail + skipped-op plumbing) | ~0.5 day | High |
| Phase 3 (retry with feedback, optional) | ~0.5 day | Medium |

Total to resolve the reported bug decisively: **1.5 engineer-days**, testing-heavy. Shippable as a single PR or split as "parser fix" + "robustness" if desired.

---

## Decision Log

- ✅ Root cause identified: applier only recognizes H2, but articles and LLM naturally use H3+.
- ✅ Prompt lie confirmed: `CURRENT SECTION HEADINGS` hides sub-headings from the model.
- ✅ No retry/fallback today — single miss kills the entire proposal.
- ✅ On-disk shape of `WikiSectionOp` unchanged by the fix; migration not required.
- ✅ Primary fix scoped to `section_ops.rs` + `mod.rs` headings block + prompt text; no REST, storage, or frontend changes.
- ⏳ Awaiting implementation sign-off; recommend landing Phase 1 first as an isolated PR.
