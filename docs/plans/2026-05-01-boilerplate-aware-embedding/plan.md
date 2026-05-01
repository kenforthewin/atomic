# Boilerplate-Aware Embedding

**Date:** 2026-05-01  
**Status:** Planning  
**Project:** Atomic  
**Request:** Strip boilerplate chunks before embedding without changing stored atom content (Option 2). Re-embed should work correctly after this change.

---

## Executive Summary

Atoms that share identical boilerplate sections (headers, footers, disclaimers) generate near-identical embedding vectors because those tokens dominate the vector space. The fix: detect shared chunks at embedding time, exclude them from `vec_chunks` (the semantic search index) while keeping them in `atom_chunks` (FTS and display). Stored atom content is never modified.

The Re-embed button in the health dashboard then becomes meaningful — it will re-run the pipeline with boilerplate filtering, producing distinct vectors for atoms whose unique content had previously been drowned out.

---

## Current Architecture & Evidence

### Embedding pipeline: single atom (`embedding.rs` L511–638)

```
chunk_content(content)              ← chunking.rs:457
  → Vec<String>
  → PendingChunk { atom_id, chunk_index, content }
embed_chunks_batched(provider, pending)   ← sends chunk.content to provider
  → Vec<(PendingChunk, Vec<f32>)>
save_chunks_and_embeddings_sync(atom_id, [(content, vec)])
  → atom_chunks(id, atom_id, chunk_index, content, embedding)
  → vec_chunks(chunk_id, embedding)          ← semantic search index
```

**Injection point:** `embedding.rs:559` — after `chunk_content`, before building `pending`.

### Re-embed path (`embedding.rs` L1630–1824)

Used by the Re-embed button via `retry_embedding`. Loads existing `atom_chunks.content` from the DB and sends those texts to the provider again, then calls `update_chunk_embeddings_sync` which updates both `atom_chunks.embedding` and `vec_chunks.embedding`.

**Injection point:** `embedding.rs:1679–1688` — after loading existing chunks, before building `group_chunks`.

### Chunk storage schema (inferred from `chunks.rs` L100–175, L224)

```sql
atom_chunks (id, atom_id, chunk_index, content, embedding)
vec_chunks  (chunk_id, embedding)   -- sqlite-vec virtual table, drives semantic search
```

`atom_chunks` is also indexed for FTS (`fts_atom_chunks`). `vec_chunks` is the semantic search source. These are currently in sync — every chunk has both a content entry and a vector entry.

### Boilerplate detection query (currently in `health.rs` L394–408)

```sql
SELECT source_atom_id FROM semantic_edges
WHERE similarity_score >= 0.99
GROUP BY source_atom_id HAVING COUNT(*) >= 2
LIMIT 50
```

This detects the *symptom* (near-identical edge scores) but does nothing about the cause at embedding time.

---

## Recommended Approach

### Option 2: Strip boilerplate chunks before embedding, preserve stored content

**Core idea:** compute a normalized fingerprint for each chunk, count how many distinct atoms share that exact chunk, and skip sending it to the embedding provider if it appears in ≥ N atoms. The chunk stays in `atom_chunks` (FTS still works) but gets no entry in `vec_chunks` (semantic search ignores it).

**Threshold:** 5 atoms (configurable via settings key `boilerplate_min_atom_count`, default `5`).

**Normalization:** lowercase + collapse whitespace + strip leading `#` markdown markers. This ensures `# My Header` and `## My Header` with different whitespace are treated as the same boilerplate.

**Fast detection:** add a `content_hash TEXT` column to `atom_chunks` (SHA-256 of normalized text, stored as hex). Index it. One GROUP BY query per embedding run tells us which hashes appear in ≥ N atoms.

---

## Implementation Plan

### Phase 0: New `boilerplate.rs` module (~2h)

**File:** `crates/atomic-core/src/boilerplate.rs`

```rust
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// Normalize chunk text for boilerplate detection.
/// Lowercases, collapses whitespace, strips leading markdown heading markers.
pub fn normalize_for_dedup(text: &str) -> String {
    text.lines()
        .map(|l| l.trim_start_matches('#').trim())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Compute SHA-256 hex digest of normalized text.
pub fn content_hash(text: &str) -> String {
    let normalized = normalize_for_dedup(text);
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Given a map of `hash → distinct_atom_count`, return the indices of chunks
/// that are boilerplate (count >= threshold).
/// If ALL chunks would be filtered, returns an empty set (fallback: embed everything).
pub fn boilerplate_indices(
    chunks: &[String],
    counts: &HashMap<String, i64>,
    min_atom_threshold: i64,
) -> HashSet<usize> {
    let indices: HashSet<usize> = chunks
        .iter()
        .enumerate()
        .filter_map(|(i, chunk)| {
            let h = content_hash(chunk);
            let count = counts.get(&h).copied().unwrap_or(0);
            (count >= min_atom_threshold).then_some(i)
        })
        .collect();

    // Fallback: if every chunk is boilerplate, embed all of them
    // (better than producing a zero-chunk atom with no vector)
    if indices.len() == chunks.len() {
        HashSet::new()
    } else {
        indices
    }
}
```

Add `sha2` to `[dependencies]` in `crates/atomic-core/Cargo.toml` (already likely present — verify).

Declare the module in `crates/atomic-core/src/lib.rs`:
```rust
pub(crate) mod boilerplate;
```

---

### Phase 1: Schema migration — add `content_hash` to `atom_chunks` (~1h)

**File:** `crates/atomic-core/src/db.rs` (SQLite schema migrations)

Find the latest migration version (currently V10 based on the `011_edges_status.sql` Postgres mirror). Add a new SQLite migration:

```rust
// V11: add content_hash column to atom_chunks for boilerplate detection
conn.execute_batch(
    "ALTER TABLE atom_chunks ADD COLUMN content_hash TEXT;
     CREATE INDEX IF NOT EXISTS idx_atom_chunks_content_hash
         ON atom_chunks(content_hash);",
)?;
```

This is a safe `ADD COLUMN` (nullable, no default required). Existing rows will have `content_hash = NULL` until re-embedded.

---

### Phase 2: Write content_hash when saving chunks (~1h)

**File:** `crates/atomic-core/src/storage/sqlite/chunks.rs`, `save_chunks_for_atom` (L224)

Update the INSERT to compute and store the hash:

```rust
use crate::boilerplate::content_hash;

// In save_chunks_for_atom, when inserting each chunk:
let hash = content_hash(&content);
conn.execute(
    "INSERT INTO atom_chunks (id, atom_id, chunk_index, content, content_hash, embedding)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    params![chunk_id, atom_id, idx, content, hash, embedding_blob],
)?;
```

---

### Phase 3: Storage helper for boilerplate count lookup (~1h)

**File:** `crates/atomic-core/src/storage/sqlite/chunks.rs`

Add a new sync method:

```rust
/// Given a list of content hashes, return a map of hash → count of distinct atoms
/// that contain a chunk with that hash. Used for boilerplate detection at embed time.
pub(crate) fn count_chunk_hash_occurrences_sync(
    &self,
    hashes: &[String],
) -> StorageResult<HashMap<String, i64>> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }
    let conn = self.db.read_conn()?;
    let placeholders = hashes.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT content_hash, COUNT(DISTINCT atom_id) as cnt
         FROM atom_chunks
         WHERE content_hash IN ({})
           AND content_hash IS NOT NULL
         GROUP BY content_hash",
        placeholders
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut map = HashMap::new();
    let rows = stmt.query_map(
        rusqlite::params_from_iter(hashes.iter()),
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;
    for row in rows {
        let (hash, cnt) = row?;
        map.insert(hash, cnt);
    }
    Ok(map)
}
```

Wire this up through the `StorageBackend` async wrapper in `ChunkStore` trait and `StorageBackend` dispatcher as `count_chunk_hash_occurrences`.

---

### Phase 4: Inject filtering into single-atom embedding (`process_embedding_only_inner`) (~1.5h)

**File:** `crates/atomic-core/src/embedding.rs` L559–596

After `let chunks = chunk_content(content)`:

```rust
// Boilerplate filtering: exclude chunks shared across >= threshold atoms
let threshold = settings_map
    .get("boilerplate_min_atom_count")
    .and_then(|v| v.parse::<i64>().ok())
    .unwrap_or(5);

let hashes: Vec<String> = chunks.iter().map(|c| boilerplate::content_hash(c)).collect();
let occurrence_counts = storage
    .count_chunk_hash_occurrences_sync(&hashes)
    .await
    .unwrap_or_default();
let boilerplate_set = boilerplate::boilerplate_indices(&chunks, &occurrence_counts, threshold);

if !boilerplate_set.is_empty() {
    tracing::debug!(
        atom_id,
        stripped = boilerplate_set.len(),
        total = chunks.len(),
        "Stripping boilerplate chunks before embedding"
    );
}

let pending: Vec<PendingChunk> = chunks
    .into_iter()
    .enumerate()
    .filter(|(i, _)| !boilerplate_set.contains(i))
    .map(|(index, chunk)| PendingChunk { atom_id: atom_id.to_string(), existing_chunk_id: None, chunk_index: index, content: chunk })
    .collect();
```

> **Note:** chunks are still saved to `atom_chunks` (FTS) after this — the filter only affects what gets embedded. The `save_chunks_and_embeddings_sync` call needs to save ALL chunks to `atom_chunks` but only boilerplate-filtered ones to `vec_chunks`.

**Required change to `save_chunks_and_embeddings_sync` / `save_chunks_for_atom`:**

Change the signature to accept a `boilerplate_indices: &HashSet<usize>` parameter. When inserting a chunk whose index is in `boilerplate_set`, insert into `atom_chunks` with `embedding = NULL` and skip the `vec_chunks` insert.

Alternatively (simpler): save all chunks with embeddings as today, but after saving, delete `vec_chunks` entries for boilerplate chunks. This avoids changing the save signature.

Recommended: the "delete after save" approach for minimal blast radius:

```rust
// After save_chunks_and_embeddings_sync, delete vec_chunks for boilerplate chunk indices
if !boilerplate_set.is_empty() {
    storage.delete_boilerplate_chunk_vectors_sync(atom_id, &boilerplate_set).await.ok();
}
```

New storage method `delete_boilerplate_chunk_vectors_sync(atom_id, indices)`:
```sql
DELETE FROM vec_chunks
WHERE chunk_id IN (
    SELECT id FROM atom_chunks
    WHERE atom_id = ?1 AND chunk_index IN (?,?,...)
)
```

---

### Phase 5: Inject filtering into re-embed path (`process_existing_chunk_reembedding_batch_inner`) (~1.5h)

**File:** `crates/atomic-core/src/embedding.rs` L1678–1700

After loading `existing_chunks` and building `chunks_by_atom`, add boilerplate filtering per atom:

```rust
// Bulk-fetch occurrence counts for all chunk hashes in this group
let all_hashes: Vec<String> = chunks_by_atom
    .values()
    .flat_map(|chunks| chunks.iter().map(|c| boilerplate::content_hash(&c.content)))
    .collect::<HashSet<_>>()
    .into_iter()
    .collect();

let occurrence_counts = storage
    .count_chunk_hash_occurrences_sync(&all_hashes)
    .await
    .unwrap_or_default();

// Filter boilerplate per atom's chunk list
let mut boilerplate_chunk_ids: Vec<String> = Vec::new();
for (atom_id, chunks) in &mut chunks_by_atom {
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let bp_indices = boilerplate::boilerplate_indices(&texts, &occurrence_counts, threshold);
    if !bp_indices.is_empty() {
        for i in &bp_indices {
            if let Some(chunk) = chunks.get(*i) {
                if let Some(ref id) = chunk.existing_chunk_id {
                    boilerplate_chunk_ids.push(id.clone());
                }
            }
        }
        // Remove boilerplate chunks from the re-embed list
        let mut keep = chunks.drain(..).enumerate()
            .filter(|(i, _)| !bp_indices.contains(i))
            .map(|(_, c)| c)
            .collect::<Vec<_>>();
        *chunks = keep;
    }
}

// Delete vec_chunks entries for boilerplate chunk IDs
if !boilerplate_chunk_ids.is_empty() {
    storage.delete_vec_chunks_by_ids_sync(&boilerplate_chunk_ids).await.ok();
}
```

New storage method `delete_vec_chunks_by_ids_sync(chunk_ids: &[String])`:
```sql
DELETE FROM vec_chunks WHERE chunk_id IN (?, ?, ...)
```

---

### Phase 6: Backfill `content_hash` for existing atoms (~0.5h)

Existing `atom_chunks` rows have `content_hash = NULL`. They need hashes so boilerplate detection works on the first re-embed run. Add a one-time backfill function:

```rust
/// Backfill content_hash for all atom_chunks rows that have content but no hash.
/// Called once at startup (skip if all rows already have hashes).
pub(crate) fn backfill_content_hashes_sync(&self) -> StorageResult<usize>
```

```sql
-- Read rows needing backfill
SELECT id, content FROM atom_chunks WHERE content_hash IS NULL LIMIT 1000
-- Update in batches of 1000
UPDATE atom_chunks SET content_hash = ? WHERE id = ?
```

Do this in a background task at server startup (in `main.rs` or the health task scheduler), not blocking the hot path.

---

### Phase 7: Update health dashboard Re-embed UX (~0.5h)

**File:** `src/components/dashboard/widgets/HealthReviewModal.tsx`, `BoilerplateSection`

- Change the button label from **"Re-embed"** to **"Re-embed (strip boilerplate)"** with a tooltip explaining what it does
- After re-embed queues, show a more informative message: `"Queued — boilerplate will be stripped from embedding on next pipeline run"`
- Remove the confusing explanatory text telling users to "edit each atom"

---

## Files / Components To Change

| File | Change |
|------|--------|
| `crates/atomic-core/Cargo.toml` | Add `sha2` dependency if not present |
| `crates/atomic-core/src/boilerplate.rs` | **New** — normalize, hash, filter logic |
| `crates/atomic-core/src/lib.rs` | Declare `pub(crate) mod boilerplate` |
| `crates/atomic-core/src/db.rs` | V11 migration: add `content_hash` column + index |
| `crates/atomic-core/src/storage/sqlite/chunks.rs` | `save_chunks_for_atom` stores hash; new `count_chunk_hash_occurrences_sync`; new `delete_vec_chunks_by_ids_sync`; new `delete_boilerplate_chunk_vectors_sync`; new `backfill_content_hashes_sync` |
| `crates/atomic-core/src/storage/traits.rs` | Add new storage trait methods |
| `crates/atomic-core/src/embedding.rs` | Filter in `process_embedding_only_inner` (L559) and `process_existing_chunk_reembedding_batch_inner` (L1679) |
| `crates/atomic-core/src/health/checks.rs` | `boilerplate_pollution` description update (minor) |
| `src/components/dashboard/widgets/HealthReviewModal.tsx` | Update Re-embed button label and success message |

---

## Data Flow / Interfaces

```
chunk_content(content)
  → Vec<String>                        [all chunks, original text]

boilerplate_indices(chunks, counts, threshold)
  → HashSet<usize>                     [indices to skip for embedding]

embed_chunks_batched(provider, non_boilerplate_pending)
  → Vec<(PendingChunk, Vec<f32>)>      [vectors for unique chunks only]

save_chunks_and_embeddings_sync(atom_id, all_chunks_with_vecs)
  → atom_chunks: all chunks (FTS intact)
  → vec_chunks: all chunks initially

delete_boilerplate_chunk_vectors_sync(atom_id, boilerplate_indices)
  → vec_chunks: boilerplate chunk entries removed
```

---

## Configuration

New settings key: `boilerplate_min_atom_count` (default: `"5"`)

- Stored in `settings` table like all other settings
- Readable via `core.get_setting("boilerplate_min_atom_count")`
- Lower = more aggressive stripping (e.g. `3`); higher = more conservative (e.g. `10`)

---

## Testing / Validation Plan

### Unit tests — `crates/atomic-core/src/boilerplate.rs`

```rust
#[test]
fn test_normalize_strips_heading_markers() { ... }

#[test]
fn test_normalize_collapses_whitespace() { ... }

#[test]
fn test_content_hash_deterministic() { ... }

#[test]
fn test_boilerplate_indices_all_unique() {
    // All counts < threshold → no indices returned
}

#[test]
fn test_boilerplate_indices_shared_chunks() {
    // 3 chunks, 2 appear in >= 5 atoms → indices {0, 2} returned
}

#[test]
fn test_boilerplate_indices_fallback_all_boilerplate() {
    // All chunks are boilerplate → returns empty set (fallback)
}
```

### Integration test — `crates/atomic-core/tests/health_tests.rs`

```rust
#[tokio::test]
async fn test_boilerplate_chunks_excluded_from_vec_search() {
    // 1. Create 6 atoms all sharing the same header chunk
    // 2. Run embedding pipeline for all 6
    // 3. Verify: atom_chunks contains the shared header for each atom
    // 4. Verify: vec_chunks does NOT contain vectors for the shared header chunks
    // 5. Verify: vec_chunks DOES contain vectors for the unique body chunks
}

#[tokio::test]
async fn test_reembed_strips_boilerplate_retroactively() {
    // 1. Create 6 atoms, embed without boilerplate filtering (pre-migration state)
    // 2. Trigger retry_embedding on one of the atoms
    // 3. Verify shared header chunk's vec_chunks entry is deleted
}

#[tokio::test]
async fn test_boilerplate_below_threshold_not_stripped() {
    // 1. Create 4 atoms (< 5) sharing a header
    // 2. Embed all 4
    // 3. Verify shared header IS in vec_chunks (below threshold)
}
```

Verification commands:
```bash
cargo test -p atomic-core -- boilerplate
cargo test -p atomic-core -- health
cargo check -p atomic-core -p atomic-server
npx tsc --noEmit
```

---

## Risks, Assumptions, and Open Questions

| # | Risk / Assumption | Severity | Mitigation |
|---|-------------------|----------|------------|
| 1 | Backfill of `content_hash` for large DBs may be slow | Medium | Run in background task, not at request time |
| 2 | Threshold of 5 may strip legitimately shared content (e.g. a wiki-style infobox used in exactly 5 articles) | Low | Make configurable; default conservative |
| 3 | After stripping, atoms with 100% boilerplate content get zero semantic vectors — they disappear from search | Medium | Fallback: if all chunks filtered, embed all (already in plan) |
| 4 | `sha2` crate may not be in workspace dependencies | Low | Check `Cargo.toml`; fallback to `ring` if already present |
| 5 | The `delete after save` approach creates a brief window where boilerplate chunks have vectors | Negligible | Single-atom pipeline is synchronous; window is sub-millisecond |
| 6 | Postgres backend (`storage/postgres/chunks.rs`) also needs the same changes | Medium | Mirror all new methods in Postgres implementation |

**Open question:** Should the health check `boilerplate_pollution` score improve automatically once boilerplate chunks are stripped from `vec_chunks`? Yes — the check queries `semantic_edges WHERE similarity_score >= 0.99`. After re-embedding, similarity scores for these atoms should drop below 0.99 for non-boilerplate content, removing them from the query results.

---

## LOE / Effort Estimate

| Phase | Task | Hours |
|-------|------|-------|
| 0 | `boilerplate.rs` module | 2h |
| 1 | Schema migration (V11) | 1h |
| 2 | Store `content_hash` on save | 1h |
| 3 | Storage helper: count occurrences | 1h |
| 4 | Inject filtering: single-atom path | 1.5h |
| 5 | Inject filtering: re-embed batch path | 1.5h |
| 6 | Backfill task | 0.5h |
| 7 | UX update (Re-embed button) | 0.5h |
| Tests | Unit + integration | 2h |
| Postgres parity | Mirror new methods | 1.5h |
| **Total** | | **~12.5h** |

---

## Decision Log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-05-01 | Strip before embedding, keep in `atom_chunks` | Preserves FTS, display, and stored atom content intact |
| 2026-05-01 | Add `content_hash` column vs. full-text comparison | Hash index is orders of magnitude faster than full-text equality scan |
| 2026-05-01 | Threshold = 5 atoms (configurable) | Conservative default; avoids stripping shared stylistic choices in small corpora |
| 2026-05-01 | "Delete after save" for vec_chunks | Minimal blast radius vs. changing `save_chunks_and_embeddings_sync` signature |
| 2026-05-01 | Fallback: embed all if all chunks are boilerplate | Prevents atoms from becoming invisible in semantic search |
