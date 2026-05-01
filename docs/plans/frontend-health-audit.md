# Frontend Health Review Queue Audit
**Date:** 2026-05-01 | **Auditor:** Scout  
**Scope:** `src/components/dashboard/widgets/HealthReviewModal.tsx`, `HealthCheckRow.tsx`, `HealthWidget.tsx`

---

## Executive Summary

**Critical Bug Found:** Merge actions in the overlap section send `check: 'duplicate_detection'` to the API, but the modal is triggered from the `content_overlap` check. The backend expects `check: 'content_overlap'` for the fix endpoint.

**Data Flow Issues:**
- `contradiction_detection`: Shows only count, no pair data to inspect
- `content_quality`: Shows raw atom IDs, missing titles/content preview
- `tag_health`: Shows counts only, no actionable drill-down
- All sections missing comprehensive loading/error/empty states

**Test Coverage:** Zero tests for health components (no `__tests__` directory exists).

---

## Component Structure & Data Flow

### File Organization
```
src/components/dashboard/widgets/
├── HealthWidget.tsx           (714 lines) — Main panel, orchestrator
├── HealthReviewModal.tsx      (561 lines) — Modal with 5 tabs + pair actions
├── HealthCheckRow.tsx         (169 lines) — Single check row (expand/run/review)
├── HealthConfirmModal.tsx     — Fix confirmation dialog
├── HealthExportModal.tsx      — Markdown export
└── (no __tests__ directory)   ⚠️ Zero test coverage
```

### Modal Trigger Flow
```
HealthWidget.tsx
  → onReview(checkName) 
    → setShowReviewModal(checkName)
      → HealthReviewModal receives { report, checkName, onClose, onResolved }
        → Extracts report.checks[checkName].data
        → Renders tab-specific sections
```

### API Endpoints Called
| Section | Endpoint | Params |
|---------|----------|--------|
| Overlap pairs | `apply_health_item_fix` | `{ check, item_id, action }` |
| Boilerplate | `get_atom` | `{ id }` (per atom) |
| Boilerplate | `retry_embedding` | `{ atomId }` |
| (others) | None | Count-only display |

---

## Tab-by-Tab Analysis

### 1. Content Overlap Tab
**Check Name:** `content_overlap`  
**Data Source:** `report.checks['content_overlap']?.data?.pairs` → `OverlapPair[]`  
**Expected Data Structure:**
```typescript
OverlapPair {
  pair_id: string;
  atom_a: { id, title, source? };
  atom_b: { id, title, source? };
  similarity: number;
  shared_tag_count: number;
  available_actions: string[];
}
```

**What It Renders:**
- ✅ Atom titles (from `pair.atom_a.title`, `pair.atom_b.title`)
- ✅ Source labels (extracted via `sourceLabel()` helper)
- ✅ Similarity percentage (with color coding)
- ✅ Shared tag count
- ✅ Expandable content comparison (fetches full atom content on expand)
- ✅ Two action buttons: "Merge" and "Keep both"

**UX Features:**
- ✅ Loading indicator during expand
- ✅ Error state display
- ✅ Completion state (shows "Merged" or "Kept both" with checkmark)

**🔴 CRITICAL BUG: Check Name Mismatch**  
**Line 467:**
```typescript
await getTransport().invoke('apply_health_item_fix', {
  check: 'duplicate_detection',  // ❌ WRONG
  item_id: itemId,
  action,
});
```

**Problem:** Backend expects `check: 'content_overlap'` (the actual check name), not `'duplicate_detection'`.  
**Impact:** Merge/Keep actions will fail with "unknown check" error.  
**Fix:** Change to `check: 'content_overlap'`.

**⚠️ Missing State:** No loading indicator while action processes (only local UI state).

**Data Completeness:** ✅ Full — titles, sources, similarity, tags all populated by backend.


---

### 2. Boilerplate Pollution Tab
**Check Name:** `boilerplate_pollution`  
**Data Source:** `report.checks['boilerplate_pollution']?.data?.affected_atoms` → `string[]` (atom IDs only)

**What It Does:**
1. Fetches each atom via `get_atom('id')` in `Promise.allSettled()`
2. Extracts first non-empty line and treats it as `title`
3. Fallback to atom ID if fetch fails
4. Shows title, source URL (if present), and "Re-embed" button per atom

**What It Renders:**
- ✅ Atom title (extracted from first line of content)
- ✅ Source URL (if present, with external link button)
- ✅ Re-embed button with loading spinner
- ✅ Completion badge ("Queued") after success

**UX Features:**
- ✅ Loading spinner while fetching all atoms (`setLoadingAtoms`)
- ✅ Per-atom action state (idle → loading → done/error)
- ✅ Fallback title (atom ID) if content fetch fails

**⚠️ ISSUES:**

1. **Missing empty state message:** If `atomIds.length === 0`, shows empty grid instead of message.

2. **Title extraction brittle:** Uses `first_line.replace(/^#+\s*/, '').trim().slice(0, 80)` which breaks if:
   - First line is list item (`- ` or `* `)
   - First line is code fence (` ``` `)
   - First line is quote (`> `)
   - First line too short, truncated mid-word

3. **Re-embed endpoint:** Calls `retry_embedding` with `atomId` param. Verify backend signature matches.

4. **No error message per atom:** If `get_atom()` fails, shows ID as fallback but doesn't indicate error.

5. **Success state confusing:** Shows "Queued" after `retry_embedding`, implying immediate re-embed. Misleading — just queued for next pipeline run.

**Data Completeness:** ⚠️ Partial — backend provides atom IDs only; frontend must fetch full atoms to get titles.

---

### 3. Contradiction Detection Tab
**Check Name:** `contradiction_detection`  
**Data Source:** `report.checks['contradiction_detection']?.data` → `{ potential_contradictions: number, pairs_checked: number }`

**What It Renders:**
- Count of potential contradiction candidates
- Total pairs checked
- Generic explanation text

**🔴 CRITICAL ISSUE: No actionable data**
- Shows **count only** — user cannot see which pairs contradict
- No way to drill into individual pairs
- No action buttons to resolve contradictions
- Component is read-only information dump

**UX:** Dead end — user sees "5 contradictions found" but cannot do anything.

**Expected:** Should render list of contradiction pairs similar to overlap pairs, with diff/comparison and merge/resolve actions. Currently not implemented.

---

### 4. Content Quality Tab
**Check Name:** `content_quality`  
**Data Source:** `report.checks['content_quality']?.data?.issues?.no_source?.atoms[]` → `string[]` (atom IDs only)

**What It Renders:**
- Count of unsourced atoms
- Atom IDs in monospace font
- No other context

**⚠️ ISSUES:**

1. **Shows raw IDs instead of titles:**
```typescript
{noSourceAtoms.map(id => (
  <div key={id} className="flex items-center gap-2 p-2 bg-[#1e1e1e] rounded border border-white/5 text-xs">
    <span className="text-gray-500 font-mono truncate flex-1">{id}</span>  // Just ID!
  </div>
))}
```

2. **No way to navigate to atom:** ID displayed but no link/button to open editor.

3. **No fetch to get titles:** Unlike boilerplate section, doesn't attempt to fetch atom titles.

4. **No action:** Cannot edit from here. User must:
   - Copy ID manually
   - Navigate to atoms panel
   - Search for ID
   - Open editor
   - Add source

5. **Missing other quality issues:** Only handles `no_source`. Ignores `very_short_atoms`, `very_long_atoms`, `no_heading_atoms` if present.

**Data Completeness:** ❌ Very Poor — backend provides only IDs; no titles, no access path.

---

### 5. Tag Health Tab
**Check Name:** `tag_health`  
**Data Source:** `report.checks['tag_health']?.data?.{ rootless_tags: number, similar_name_pairs: number }`

**What It Renders:**
- Count of rootless tags (top-level, no parent)
- Count of similar-name pairs (potential duplicates)
- Explanation text
- Note: "Tag IDs not surfaced — navigate tree to find and fix"

**⚠️ ISSUES:**

1. **No actionable list:** Shows counts but not the actual tags.

2. **Impossible to find tags:** User told to "navigate tree" but:
   - With 1000+ tags, finding 15 rootless ones manually is tedious
   - No way to filter/highlight rootless tags in tree
   - No bulk actions to nest them

3. **Similar-name pairs completely hidden:** User told duplicates exist but cannot see which.

4. **No actionable state:** Component read-only summary; no merge/nest buttons.

**Expected:** List of rootless tag names with quick-nest buttons; list of similar pairs with merge buttons.

---

## Modal-Level Issues

### Tab Pre-selection (checkName Prop)
**Line 432:**
```typescript
const [selectedTab, setSelectedTab] = useState<string | null>(checkName ?? null);
const activeTab = tabs.find(t => t.key === selectedTab)?.key ?? tabs[0]?.key ?? null;
```

**Flow:**
1. ✅ Parent passes `checkName` (e.g., `'content_overlap'`)
2. ✅ State initialized to `checkName` or `null`
3. ✅ `activeTab` resolves to that tab if it exists in computed `tabs` array
4. ✅ Falls back to first available tab if `checkName` not in `tabs`

**Potential Issue:** If user reviews a check with zero issues (e.g., `contradiction_detection` with `count === 0`), that check excluded from `tabs`. Pre-selection silently falls back to first tab. Behavior correct but could warn if requested tab unavailable.

---

## Error & Loading States Matrix

| Section | Loading | Error | Empty | Notes |
|---------|---------|-------|-------|-------|
| Overlap pairs | ✅ None | ✅ Displayed | ✅ Message | Per-pair states shown |
| Boilerplate | ✅ Spinner | ❌ No feedback | ❌ No message | Fallback to ID on fail |
| Contradiction | ❌ None | ❌ None | ✅ Message | Count-only, no detail |
| Content quality | ❌ None | ❌ None | ✅ Message | Raw IDs, no context |
| Tag health | ❌ None | ❌ None | ❌ None | No empty state |

**Summary:** Overlap pairs solid; others bare-minimum or missing.


---

## Type Safety & Code Quality

### Unsafe Casts
**Line 330 (BoilerplateSection):**
```typescript
const issues = data.issues as Record<string, { count: number; atoms?: string[] }> | undefined;
```
Type-cast without narrowing. Works but fragile to schema changes.

**Line 351 (ContentQualitySection):**
```typescript
const issues = data.issues as Record<string, { count?: number }> | undefined;
```
Similar cast. Should validate or use Zod schema.

### Missing TypeScript Validation
- `data: Record<string, unknown>` passed to all sections — no schema validation
- Backend could return different shape and frontend silently fails
- No error boundary if shape is wrong

### String Literal Keys
All tab keys are string literals scattered across code:
```typescript
'content_overlap'
'boilerplate_pollution'
'contradiction_detection'
'content_quality'
'tag_health'
```

Should be defined as constants/enums to avoid typos.

---

## API Contract Observations

### Endpoints Used
1. **`apply_health_item_fix`**
   - Called by overlap pairs (merge/keep)
   - **Bug:** Sends `check: 'duplicate_detection'` instead of `'content_overlap'`

2. **`get_atom`**
   - Called by boilerplate section to fetch titles
   - Called by overlap expand to fetch full content
   - No error handling beyond Promise.allSettled()

3. **`retry_embedding`**
   - Called by boilerplate section to re-queue atom
   - Returns success/error; frontend shows "Queued" or "error" state

### Data Consistency Issues
- Backend returns `pairs` array for overlap (processed)
- Backend returns `affected_atoms` ID array for boilerplate (frontend fetches rest)
- Backend returns only counts for contradiction, quality, health (frontend cannot drill down)

**Pattern:** Inconsistent payload shapes suggest incomplete backend or mismatched frontend expectations.

---

## Missing Functionality

1. **Contradiction pairs inspection:** Backend has pairs data (health.rs), modal doesn't render them.

2. **Tag navigation:** Tag health section tells user to "navigate tree" but no links/filters provided.

3. **Bulk actions:** No way to resolve multiple items in batch (e.g., nest 5 rootless tags).

4. **Action history:** No log of fixes applied, when, by whom.

5. **Undo per-item:** Only modal-level undo (last batch); no undo individual actions in review session.

6. **Direct atom access:** Content quality and tag health sections provide no way to open atoms/tags directly.

---

## Test Coverage

**Current:** Zero  
**Test files found:** `src/lib/import-tags.test.ts`, `src/lib/import-apple-notes.test.ts` (data utilities only)

**No tests for:**
- HealthWidget render/fetch/fix flow
- HealthReviewModal tab navigation and data extraction
- PairRow merge/keep action submission
- BoilerplateSection atom fetch and title extraction
- Error states, loading states, empty states
- API endpoint error handling
- Pre-selection logic for `checkName` prop

**Test Framework:** Vitest (v3.2.4) configured but health components untested.

---

## Recommendations

### Critical (Fix Immediately)
1. **Fix check name bug (Line 467):** Change `check: 'duplicate_detection'` → `check: 'content_overlap'`
   - Merge/keep actions currently fail silently or show wrong error
   - One-line fix, high impact

2. **Add unit tests:** Create `__tests__/HealthReviewModal.test.tsx` with:
   - Tab navigation
   - Data extraction from report structure
   - Action submission (merge, keep, re-embed)
   - Error state handling
   - Empty state handling

### High Priority (Before Release)
1. **Contradiction pairs:** Implement backend query for pairs and render as list with diff view or action buttons.

2. **Content quality drill-down:** Fetch atom titles, show in list, add "Open atom" link/button to navigate to editor.

3. **Tag health drill-down:** List rootless tags and similar pairs; add nest/merge buttons or tree navigation links.

4. **Validate data schemas:** Use Zod to parse `report.checks[key].data` shape before rendering sections. Add fallback UI for schema mismatch.

5. **Error boundaries:** Wrap each section in try-catch; show fallback UI if rendering fails.

### Medium Priority
1. **Define check name constants:** Centralize `'content_overlap'`, `'boilerplate_pollution'`, etc. in a shared enum or config.

2. **Per-atom error feedback:** In boilerplate section, show "fetch failed" indicator if `get_atom()` errors instead of silent fallback.

3. **Improve title extraction:** Use regex or markdown parser; handle edge cases (lists, code, quotes).

4. **Loading state in modal:** Show spinner during action submission; disable buttons while inflight.

5. **Undo granularity:** Track individual action history; offer undo per-item or per-section, not just batch.

6. **Toast notifications:** Show action result (success/error) in toast instead of relying on onResolved() refresh.

### Low Priority
1. **Bulk actions UI:** Multi-select + batch nest/merge with preview.

2. **Action audit log:** Log with timestamps, reversible operations, user attribution.

3. **Tag tree integration:** Link rootless tags to tree panel with filter/highlight.

4. **Keyboard shortcuts:** Arrow keys to navigate pairs, Enter to apply action, etc.

5. **Export per-section:** Download individual review section as CSV/JSON for offline processing.

---

## Code Locations Summary

| Issue | File | Line(s) | Fix |
|-------|------|---------|-----|
| Check name bug | HealthReviewModal.tsx | 467 | `check: 'content_overlap'` |
| Unsafe cast | HealthReviewModal.tsx | 330 | Validate with Zod |
| Unsafe cast | HealthReviewModal.tsx | 351 | Validate with Zod |
| No empty state | HealthReviewModal.tsx | 246–254 | Add message when length=0 |
| Brittle title extract | HealthReviewModal.tsx | 223 | Use markdown parser |
| No contradiction pairs | HealthReviewModal.tsx | 318–337 | Implement pair list render |
| No quality drill-down | HealthReviewModal.tsx | 341–373 | Fetch titles, add links |
| No tag drill-down | HealthReviewModal.tsx | 377–404 | List tags, add actions |
| No tests | (new file) | — | Create `__tests__/HealthReviewModal.test.tsx` |

---

## Conclusion

**Severity:** High — one critical bug prevents merge actions from working; four tabs lack drill-down/action capability; zero test coverage.

**Effort to Fix:** 
- Critical bug: 1 line
- Tests: 1–2 days (moderate complexity, async/modal/data flow)
- Drill-down features: 2–3 days per tab (fetching, rendering, validation)
- Total: 1 week to make production-ready

**Risk:** Currently deployed health review modal is partially non-functional (merge fails). Recommend fix and test before release.
