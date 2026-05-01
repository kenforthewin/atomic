# Tag Accordion Random Scroll Issue

**Date:** 2026-04-30  
**Status:** Analysis Complete  
**Severity:** Medium (UX friction, not data loss)  
**Component:** `src/components/tags/TagTree.tsx`

## Problem Statement

Clicking a tag with an accordion dropdown (chevron) to expand/collapse scrolls the sidebar **randomly** or to incorrect positions:
- Sometimes scrolls to the very top
- Sometimes scrolls to a random position mid-list
- Expected behavior: Stay in place or smoothly scroll the tag into view

## Root Cause

**The virtualizer is scrolling to an index that becomes stale between the expansion action and the scroll execution.**

### Evidence

1. **Tag expansion and scroll are decoupled** (`src/components/tags/TagNode.tsx:24-30`):
   ```typescript
   const handleToggle = useCallback(async (e: MouseEvent) => {
     e.stopPropagation();
     if (!isExpanded && tag.children_total > tag.children.length) {
       await fetchTagChildren(tag.id);  // Async fetch
     }
     toggleTagExpanded(tag.id);  // State update
   }, [isExpanded, tag.children_total, tag.children.length, tag.id, fetchTagChildren, toggleTagExpanded]);
   ```

2. **Separate effect tries to scroll, but timing is wrong** (`src/components/tags/TagTree.tsx:90-99`):
   ```typescript
   const flatTags = useMemo(
     () => flattenVisibleTags(tags, expandedTagIds),
     [tags, expandedTagIds]
   );

   const tagIndexMap = useMemo(() => {
     const map = new Map<string, number>();
     for (let i = 0; i < flatTags.length; i++) {
       map.set(flatTags[i].tag.id, i);
     }
     return map;
   }, [flatTags]);

   // Scroll to selected tag
   useEffect(() => {
     if (selectedTagId) {
       const index = tagIndexMap.get(selectedTagId);
       if (index !== undefined) {
         setTimeout(() => {
           virtualizer.scrollToIndex(index, { align: 'auto', behavior: 'smooth' });
         }, 50);  // ← Fixed 50ms delay is arbitrary
       }
     }
   }, [selectedTagId, tagIndexMap, virtualizer]);
   ```

### Why This Breaks

1. **Clicking chevron** → calls `handleToggle` → calls `toggleTagExpanded(tag.id)`
2. **`toggleTagExpanded`** updates state → `expandedTagIds` changes
3. **`expandedTagIds` changes** → `flatTags` updates (new tree shape after expansion)
4. **`flatTags` updates** → `tagIndexMap` updates (new index positions for all tags)
5. **`selectedTagId` effect runs** → but `selectedTagId` may not have changed! The effect only runs when `selectedTagId` changes
6. **User manually clicks the tag text** → then `setSelectedTag` fires → `selectedTagId` changes
7. **Now the effect runs**, but the `virtualizer` might not be ready, the 50ms timeout is stale, or the tree has changed again

### The Timing Bug

The 50ms `setTimeout` is a **fragile workaround** for race conditions:

- If the virtualizer isn't finished measuring yet → scroll goes to wrong position
- If the tree expanded during the delay → the index changed, so the target tag is now at a different position
- If multiple expansions happen quickly → timers queue up and execute in wrong order

This is a classic **virtualizer stale state problem**: the list size changed (items are now visible that weren't before), the virtualizer's measurement of item positions is invalidated, but you're scrolling based on an old index.

## Data Flow

```
User clicks chevron
    ↓
TagNode.handleToggle()
    ├─ fetchTagChildren() [async]
    └─ toggleTagExpanded(tag.id) [sync update]
        ↓
    UI store: expandedTagIds[tag.id] = !expandedTagIds[tag.id]
        ↓
    TagTree: expandedTagIds changes
        ├─ flatTags recalculates (new tree shape)
        ├─ tagIndexMap recalculates (new positions)
        └─ virtualizer doesn't know list size changed
        ↓
    [50ms later]
    setTimeout fires → scrollToIndex()
        ↓
    ❌ Scrolls to stale index (or positions shifted while timer was running)
```

## The Real Issue

**Expanding/collapsing a tag is a UI-only operation that doesn't change `selectedTagId`.** The scroll-to-selected effect only fires when `selectedTagId` changes, not when the tree structure changes.

When you expand a tag, the virtualizer's **measured item sizes and positions become invalid** because new items are now visible. But the code doesn't tell the virtualizer to re-measure.

## Recommended Fix

**Don't scroll on tag expansion/collapse—only scroll when a tag is selected.**

Change the trigger:
- ✅ When `selectedTagId` changes: scroll the selected tag into view (current behavior)
- ✅ When `expandedTagIds` changes: **tell the virtualizer to remeasure** (currently missing)
- ❌ Don't use arbitrary timeouts

### Implementation Strategy

1. **Remove the fixed timeout** in the scroll effect
2. **Add a virtualizer remeasure call** when `flatTags` changes:
   ```typescript
   useEffect(() => {
     // When the tree structure changes, invalidate measurements
     virtualizer.measure();
   }, [flatTags, virtualizer]);
   ```
3. **Then scroll to the selected tag**, but only if it's actually in the list:
   ```typescript
   useEffect(() => {
     if (selectedTagId) {
       const index = tagIndexMap.get(selectedTagId);
       if (index !== undefined) {
         // Don't use setTimeout—let requestAnimationFrame or just call directly
         virtualizer.scrollToIndex(index, { align: 'center', behavior: 'smooth' });
       }
     }
   }, [selectedTagId, tagIndexMap, virtualizer]);
   ```

### Why This Works

- **When tree expands**: `flatTags` updates → virtualizer remeasures → virtual positions are now correct
- **When you click a tag**: `selectedTagId` changes → scroll to the now-correct index
- **No race conditions**: The virtualizer's state is fresh before scrolling
- **No arbitrary timeouts**: Execution order is clear and deterministic

## Files to Modify

| File | Change |
|------|--------|
| `src/components/tags/TagTree.tsx` | Add `virtualizer.measure()` call when `flatTags` changes; remove setTimeout from scroll effect |
| `src/components/tags/TagNode.tsx` | (No changes needed; expansion logic is correct) |

## Verification Plan

1. **Expand a tag** → No scroll should occur (tag group expands in place)
2. **Click on a tag in the expanded group** → Scrolls smoothly to center that tag
3. **Expand deeply nested tags** → Smooth scroll, no jank or jumping
4. **Rapidly expand/collapse multiple tags** → No stale index bugs
5. **Scroll manually, then click a tag** → Scrolls to correct position

## Risk Assessment

**Low risk**: This is a pure UI fix with no backend changes or data mutations.
- No database schema changes
- No API contract changes
- No state shape changes
- Only affects scroll behavior on tag interactions

## Open Questions

1. Should expanded tags scroll to center (`align: 'center'`) or just into view (`align: 'auto'`)?
   - Recommendation: `'center'` for consistency with selection highlight
2. Should `behavior: 'smooth'` remain, or use instant scroll?
   - Recommendation: Keep smooth (better UX) but reduce duration if performance is a concern

## Decision Log

- ✅ Root cause identified: stale timeout + virtualizer remeasure bug
- ✅ Isolated to TagTree.tsx scroll effect
- ✅ Solution avoids broad refactoring
- ⏳ Awaiting implementation
