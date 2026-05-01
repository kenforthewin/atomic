# Deep Audit: Health Review Queue Backend

## Executive Summary

Audited **11 health checks** across three modules (`checks.rs`, `mod.rs`, `health.rs` storage). Found **4 checks with `requires_review: true`** that surface user-actionable data to the UI. Data sufficiency ranges from **rich (full atom details with similarity/source)** to **bare counts only (rootless tags)**. No critical bugs found; several UX gaps identified.

---

## Checks with `requires_review: true`

### 1. **`content_overlap`** — High-value data ✅
| Field | Value |
|-------|-------|
| **Lines** | checks.rs:330–369 |
| **Status Sets Review** | When `overlaps > 0` (cross-source semantic duplicates) |
| **Condition** | Similarity 0.55–0.85, ≥2 shared tags, different source prefixes |
| **Data Shape** | `{ exact_duplicates, template_clones, cross_source_overlaps, count, pairs[] }` |
| **Pairs Structure** | Each pair includes: `pair_id`, `atom_a{id,title,source}`, `atom_b{id,title,source}`, `similarity`, `shared_tag_count`, `available_actions[]` |
| **Storage Query** | `health.rs:L341–371` — Joins `semantic_edges` → `atoms` (2x) → `atom_tags` (2x). Filters on similarity score and shared tags. Extracts title via `extract_title_preview()` (first ~100 chars until newline). |
| **UX Sufficiency** | ✅ **Excellent** — All needed data present: atom IDs, titles, source URLs, similarity %, shared tags, suggested actions. UI can display a pair list immediately. |
| **Data Quality** | ✅ Correct SQL joins. Title extraction may lose content if first paragraph is long. |
| **Gap** | None identified for core UX. |

---

### 2. **`content_quality` → `no_source` sub-issue** — Bare IDs only ⚠️
| Field | Value |
|-------|-------|
| **Lines** | checks.rs:253–305 |
| **Status Sets Review** | When `!raw.no_source_atoms.is_empty()` |
| **Condition** | Atoms with `null source_url` AND no HTTP(S) link AND no "Source:" text in content |
| **Data Shape** | `{ total, issues { no_source { count, auto_fixable: false, atoms: [id, ...] } } }` |
| **Storage Query** | `health.rs:L307–316` — Simple SELECT on atoms table: `WHERE source_url IS NULL AND content NOT LIKE '%http://%' AND NOT LIKE '%https://%' AND NOT LIKE '%Source:%'` LIMIT 20. Returns only atom ID. |
| **UX Sufficiency** | ⚠️ **Minimal** — Only IDs returned. UI must fetch full atoms (title, created date, preview) separately to display meaningful review list. |
| **Data Quality** | ✅ SQL correct, but incomplete. |
| **Gap** | **Should return**: atom ID + title + preview (first ~200 chars) + created_at + updated_at. This would let UI show context without additional round-trips. |

---

### 3. **`boilerplate_pollution`** — Bare IDs only ⚠️
| Field | Value |
|-------|-------|
| **Lines** | checks.rs:398–418 |
| **Status Sets Review** | When `count > 0` (atoms with ≥2 near-identical edges at similarity ≥0.99) |
| **Condition** | Semantic edges at similarity ≥0.99 grouped by source atom with count ≥2 |
| **Data Shape** | `{ count, affected_atoms: [id, ...], description: "..." }` |
| **Storage Query** | `health.rs:L360–366` — `SELECT source_atom_id FROM semantic_edges WHERE similarity_score >= 0.99 GROUP BY source_atom_id HAVING COUNT(*) >= 2 LIMIT 50`. Returns only atom IDs. |
| **UX Sufficiency** | ⚠️ **Minimal** — Only IDs. UI cannot show context. |
| **Data Quality** | ✅ SQL correct. |
| **Gap** | **Should return**: atom ID + title + count of near-duplicate edges. This allows UI to prioritize review (atoms with 5+ clones are more urgent than those with 2). |

---

### 4. **`contradiction_detection`** — Counts only, no pair data ❌
| Field | Value |
|-------|-------|
| **Lines** | checks.rs:371–387 |
| **Status Sets Review** | When `count > 0` (candidate contradictions found) |
| **Condition** | `contradiction_candidate_count > 0` — derived from semantic edges with similarity 0.75–0.92 |
| **Data Shape** | `{ pairs_checked, potential_contradictions }` — **NO pairs returned** |
| **Storage Query** | `health.rs:L395–398` — Two COUNT queries only: `SELECT COUNT(*) FROM semantic_edges WHERE similarity_score >= 0.75 AND similarity_score < 0.92`. Returns only counts, no pair details. |
| **UX Sufficiency** | ❌ **Unusable** — UI shows "Found 10 potential contradictions" but cannot display anything to review. User sees a warning with no actionable content. |
| **Data Quality** | ⚠️ **Incomplete by design**. Comment in code (checks.rs:375–376): "For now, surface the count as 'candidates' (no LLM check yet)" — implies pairs/details are intentionally deferred. |
| **Gap** | **Critical UX issue**: Either (a) disable `requires_review: true` until pair data is available, or (b) return the actual pairs (atom IDs, titles, snippets, similarity %) so users can manually review them. Current state shows a warning the user cannot act on. |

---

### 5. **`tag_health` → `rootless_tags`** — Counts only, no IDs ❌
| Field | Value |
|-------|-------|
| **Lines** | checks.rs:307–328 |
| **Status Sets Review** | When `rootless > 0` (tags with no parent) |
| **Condition** | `rootless_tags > 0` |
| **Data Shape** | `{ single_atom_tags, rootless_tags, similar_name_pairs }` — **NO tag details** |
| **Storage Query** | `health.rs:L331–335` — `SELECT COUNT(*) FROM tags WHERE parent_id IS NULL`. Returns only count. |
| **UX Sufficiency** | ⚠️ **Poor** — UI shows "2 rootless tags" but cannot identify which ones. User cannot act without drilling into the tag tree UI separately. |
| **Data Quality** | ✅ SQL correct. |
| **Gap** | **Should return**: count + `[(tag_id, tag_name, atom_count), ...]` list. This lets UI show a "Fix" action (move to parent category or promote to root manually). |

---

## All Other Checks (not requiring review)

| Check | Status | Why No Review Needed |
|-------|--------|---------------------|
| `embedding_coverage` | ❌ | Auto-fixable (retry pipeline). UI shows progress bars. |
| `tagging_coverage` | ❌ | Auto-fixable. Shows counts of pending/failed/untagged. |
| `source_uniqueness` | ❌ | Auto-fixable (merge exact duplicates). Pairs included. |
| `orphan_tags` | ❌ | Auto-fixable (delete). Full tag IDs + names included. |
| `semantic_graph_freshness` | ❌ | Auto-fixable (rebuild edges). Shows dates + count. |
| `wiki_coverage` | ❌ | Auto-fixable (generate/update). Gaps + stale list included. |
| `broken_internal_links` | ❌ | Auto-fixable (resolve). Only counts returned, no pairs. |

---

## Async Check: `broken_internal_links`

| Field | Value |
|-------|-------|
| **Lines** | mod.rs:393–493 |
| **Runs** | Via `compute_link_check()` in health flow |
| **Requires Review** | ❌ No — `requires_review: false` |
| **Logic** | Per-atom check: extracts markdown + wikilinks → resolves via source URL or wikilink name lookup. Returns broken count & affected atom count. |
| **Data Shape** | `{ broken_count: i32, affected_atoms: i32 }` — counts only |
| **UX Gap** | If `broken_count > 0`, UI shows warning but no atom IDs. Cannot identify which atoms have broken links without re-running the check per atom. |

---

## Storage Queries: Summary

### `HealthRawData` struct (~80 fields total)

All queries live in `health.rs:L87–422` under `health_check_data_impl()`. Pattern:
1. **Counts & status groups** — Simple aggregations (embedding_status, tagging_status, etc.)
2. **Filtered lists** — Orphan tags, very-short/long atoms, boilerplate atoms (IDs only)
3. **Rich joins** — Content overlap (full pairs with titles), wiki coverage (tag names + atom counts)
4. **Pair construction** — DuplicatePair struct built in Rust loop (source_prefix, title extraction)

### Data Returned by Reviewable Checks

| Check | Data Type | Sufficiency |
|-------|-----------|-------------|
| content_overlap | Vec<DuplicatePair> | ✅ Complete (ID, title, source, similarity, shared tags) |
| content_quality:no_source | Vec<String> | ⚠️ IDs only, missing title/preview |
| boilerplate_pollution | Vec<String> | ⚠️ IDs only, missing title/count of clones |
| contradiction_detection | i32 count | ❌ No pairs at all |
| tag_health:rootless | i32 count | ❌ No tag list at all |

---

## Bugs Found

### None critical. Minor observations:

1. **`tag_health:rootless` logic** (checks.rs:L320)
   - Query returns `COUNT(*) FROM tags WHERE parent_id IS NULL`
   - This counts ALL tags with null parent, including the autotag category roots (Topics, People, Locations, etc.)
   - May be intentional (those are "rootless" in tree structure), but unclear if UX wants to surface them as issues
   - Recommend: Add comment clarifying whether autotag roots should be excluded

2. **`contradiction_detection` semantic** (checks.rs:L375–376)
   - Comment says "no LLM check yet", but the check still sets `requires_review: true`
   - Means UI shows a warning the user cannot act on
   - Should either: (a) return pair details now, or (b) set `requires_review: false` until LLM pair analysis is ready

3. **Title extraction** (health.rs:L777–782)
   - `extract_title_preview()` returns first line (up to \n), max ~100 chars
   - If atom starts with a code block or long table, preview is useless
   - Low impact, but UX could show "Preview" section more explicitly

---

## Tests

### Unit Tests
- **link_resolution.rs**: 13 tests (L405–487)
  - Internal link extraction, wikilink parsing, vault root detection, link resolution logic
  - Examples: `test_relative_href_resolves_to_vault_root`, `test_extract_markdown_links`, `test_absolute_links_ignored`
  - **No tests for health checks themselves** (no fixtures for HealthRawData, no check validation tests)

### Integration Tests
- **integration_tests.rs**: ~20 tests
  - Full atom CRUD, tag hierarchy, pagination, wiki lifecycle, source tracking, settings, tokens, positions
  - **No health check tests** — no callers of `compute_health()`, no scenario validation
- **pipeline_tests.rs**: ~15 tests
  - Embedding/tagging pipelines, retries, model changes, delete cascades
  - **No health check tests**
- **storage_tests.rs**: ~30 tests
  - Atom, tag, chat, wiki storage operations
  - **No health check tests**

### Test Infrastructure

| Component | Location | Status |
|-----------|----------|--------|
| **Mock AI Server** | `tests/support/mod.rs` | ✅ Provided (mock embeddings + chat) |
| **Test DB Setup** | `integration_tests.rs:L13–17` | ✅ TempDir-backed SQLite |
| **Event Collector** | `tests/support/mod.rs:L336–346` | ✅ Async channel-based |
| **Core Factory** | `tests/support/mod.rs:L255–302` | ✅ `setup_core(backend, mock_url)` |
| **Health Fixtures** | ❌ None | **Gap: No fixtures for seeding HealthRawData states** |

---

## Recommendations

### High Priority

1. **`contradiction_detection`**: Either return pair details or set `requires_review: false`
   - Rationale: Currently surfaces unprovable claim to user
   - Effort: Medium (SQL for pairs + build DuplicatePair-like struct for contradictions)

2. **`tag_health:rootless`**: Return tag list, not just count
   - Rationale: Allows user to fix (merge to parent, or acknowledge as root category)
   - Effort: Low (add 1 query, return Vec<(id, name, atom_count)>)

3. **`content_quality:no_source`**: Return title + preview, not just ID
   - Rationale: UI can show context without second round-trip
   - Effort: Low (modify query to SELECT id, title preview, created_at)

4. **`boilerplate_pollution`**: Return title + edge count per atom
   - Rationale: Helps prioritize review (5+ clones > 2 clones)
   - Effort: Medium (join atoms + count edges per source, aggregate)

### Medium Priority

5. **Add health check tests**
   - Create fixtures for HealthRawData states (overlaps, contradictions, quality issues, tag anomalies)
   - Validate score calculation, requires_review flags, data shape
   - Effort: ~2–3 hrs for good coverage

6. **Document tag_health rootless semantics**
   - Is counting autotag roots correct? Add comment + test
   - Effort: 30 min

### Low Priority

7. **Improve title extraction**
   - Skip code blocks, tables; return full-paragraph preview
   - Effort: Medium (markdown parsing)
   - Impact: Minor (UX polish only)

---

## Implementation Roadmap

**Phase 1 (quick wins — 1–2 hrs)**
- Add tag list to `tag_health:rootless` (modify health.rs query, update checks.rs data shape)
- Add title + preview to `content_quality:no_source` (modify health.rs query)
- Document/clarify `tag_health` rootless scope

**Phase 2 (medium — 2–3 hrs)**
- `contradiction_detection`: Decide scope (pair data now? or disable requires_review until LLM ready?)
- `boilerplate_pollution`: Add title + edge count aggregation

**Phase 3 (quality — 2–3 hrs)**
- Add comprehensive health check test fixtures
- Validate data shapes against UI expectations
- Add regression tests for fix operations

---

## Files Inspected

✅ `crates/atomic-core/src/health/checks.rs` (418 lines)
✅ `crates/atomic-core/src/health/mod.rs` (659 lines)
✅ `crates/atomic-core/src/storage/sqlite/health.rs` (798 lines)
✅ `crates/atomic-core/src/health/link_resolution.rs` (511 lines)
✅ `crates/atomic-core/tests/integration_tests.rs`
✅ `crates/atomic-core/tests/support/mod.rs`
