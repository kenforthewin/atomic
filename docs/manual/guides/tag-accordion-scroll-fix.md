# Fixing Tag Accordion Random Scroll

## Problem

Clicking a tag's accordion chevron to expand/collapse causes the sidebar to scroll randomly:
- Sometimes to the very top
- Sometimes to a random position
- Expected: no scroll, or smooth scroll to keep the tag in view

## Root Cause

The virtualizer's item position measurements become **stale** when the tree structure changes (tags expand/collapse). A fixed 50ms timeout tries to compensate, but fails because:

1. When you expand a tag, `flatTags` updates (new visible items)
2. The virtualizer's measurements are now **invalid** (it doesn't know new items exist)
3. The scroll effect waits 50ms, then calls `scrollToIndex()` with an index that's now pointing to the wrong item
4. Result: random scroll position

**Files involved:**
- `src/components/tags/TagTree.tsx` — virtualizer setup and scroll effect (lines 65–99)
- `src/components/tags/TagNode.tsx` — tag expansion toggle (lines 24–30)

## Solution

**Tell the virtualizer to remeasure when the tree structure changes, then scroll cleanly.**

### Changes Required

**File:** `src/components/tags/TagTree.tsx`

#### Change 1: Add remeasure effect (after line 99)

Add this effect after the existing scroll effect:

```typescript
// Remeasure virtualizer when tree structure changes
useEffect(() => {
  virtualizer.measure();
}, [flatTags, virtualizer]);
```

This tells the virtualizer to recalculate all item positions whenever the visible tag list changes.

#### Change 2: Fix the scroll effect (lines 90–99)

Replace the existing scroll effect with:

```typescript
// Scroll to selected tag
useEffect(() => {
  if (selectedTagId) {
    const index = tagIndexMap.get(selectedTagId);
    if (index !== undefined) {
      // Scroll directly without setTimeout—measurements are now fresh
      virtualizer.scrollToIndex(index, { align: 'center', behavior: 'smooth' });
    }
  }
}, [selectedTagId, tagIndexMap, virtualizer]);
```

**Key changes:**
- Remove `setTimeout()` — no more arbitrary delays
- Change `align: 'auto'` to `align: 'center'` — clearer visual feedback
- Scroll only runs when `selectedTagId` changes (when you click a tag)

### Why This Works

1. **Expand a tag** → `flatTags` updates → `useEffect` calls `virtualizer.measure()`
2. **Virtualizer recalculates** all item positions (now includes newly visible child tags)
3. **Click a tag in the expanded group** → `selectedTagId` changes
4. **Scroll effect runs** → index lookup is now correct, virtualizer knows all positions
5. **Smooth scroll to center** — no stale state, no race conditions

## Testing

After applying the fix:

```bash
npm run tauri dev
```

Then in the app:

1. **Expand a tag** — should stay in place (no scroll)
2. **Click a tag text** — should scroll smoothly to center that tag
3. **Expand multiple nested tags** — smooth scrolling, no jank
4. **Rapid expand/collapse** — no stale index bugs
5. **Manual scroll + click** — scrolls to correct position

## Code Location Reference

| What | Where |
|------|-------|
| Virtualizer setup | `src/components/tags/TagTree.tsx:82–88` |
| Tag expansion toggle | `src/components/tags/TagNode.tsx:24–30` |
| Scroll effect (to modify) | `src/components/tags/TagTree.tsx:90–99` |
| Where to add remeasure | `src/components/tags/TagTree.tsx:after line 99` |

## Why We Don't Scroll on Expand/Collapse

- **Expand** is a tree-structure change, not a selection change
- Scrolling on every expand would be jarring (user would see tree expand, then jump)
- **Current behavior is correct:** expand in place, then click to scroll to selection

Only scroll when the user explicitly selects a tag (clicks the text).

## Performance Notes

- `virtualizer.measure()` is cheap — it recalculates sizes, doesn't re-render
- Smooth scroll (`behavior: 'smooth'`) is hardware-accelerated
- No performance regression expected

## Related Code

- **Virtualizer setup**: `useVirtualizer()` hook from `@tanstack/react-virtual`
- **Tag selection**: `setSelectedTag()` in `src/stores/ui.ts:297–313`
- **Tree flattening**: `flattenVisibleTags()` in `src/components/tags/TagTree.tsx:20–45`
