//! Boilerplate-aware embedding filter.
//!
//! Detects chunks shared across multiple atoms and excludes them from
//! semantic search vectors (vec_chunks). The stored atom content
//! (atom_chunks.content) is never modified — only the embeddings change.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// Normalize chunk text for boilerplate fingerprinting.
/// Strips markdown heading markers, collapses whitespace, lowercases.
pub(crate) fn normalize_for_dedup(text: &str) -> String {
    let stripped: String = text
        .lines()
        .map(|l| l.trim_start_matches('#').trim())
        .collect::<Vec<_>>()
        .join(" ");
    stripped
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Compute SHA-256 hex digest of the normalized chunk text.
pub(crate) fn content_hash(text: &str) -> String {
    let normalized = normalize_for_dedup(text);
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Given a map of `hash → distinct_atom_count`, return the set of chunk
/// indices that are boilerplate (count >= min_atom_threshold).
///
/// **Fallback:** if every chunk would be filtered, returns an empty set
/// so atoms with 100% boilerplate content still get embedded.
pub(crate) fn boilerplate_indices(
    chunks: &[String],
    counts: &HashMap<String, i64>,
    min_atom_threshold: i64,
) -> HashSet<usize> {
    if min_atom_threshold <= 0 {
        return HashSet::new();
    }
    let indices: HashSet<usize> = chunks
        .iter()
        .enumerate()
        .filter_map(|(i, chunk)| {
            let h = content_hash(chunk);
            let count = counts.get(&h).copied().unwrap_or(0);
            (count >= min_atom_threshold).then_some(i)
        })
        .collect();
    // Fallback: never strip all chunks
    if indices.len() == chunks.len() && !chunks.is_empty() {
        HashSet::new()
    } else {
        indices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_strips_heading_markers() {
        assert_eq!(normalize_for_dedup("# My Header"), "my header");
        assert_eq!(normalize_for_dedup("## Section"), "section");
    }

    #[test]
    fn test_normalize_collapses_whitespace() {
        assert_eq!(normalize_for_dedup("  hello   world  "), "hello world");
    }

    #[test]
    fn test_normalize_lowercases() {
        assert_eq!(normalize_for_dedup("Hello World"), "hello world");
    }

    #[test]
    fn test_content_hash_deterministic() {
        let h1 = content_hash("# My Header");
        let h2 = content_hash("# My Header");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
    }

    #[test]
    fn test_content_hash_normalizes_heading_variants() {
        // Different markdown levels with same text → same hash after normalization
        let h1 = content_hash("# Terms of Service");
        let h2 = content_hash("## Terms of Service");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_boilerplate_indices_all_unique() {
        let chunks = vec![
            "unique content a".to_string(),
            "unique content b".to_string(),
        ];
        let counts: HashMap<String, i64> = HashMap::new();
        let indices = boilerplate_indices(&chunks, &counts, 5);
        assert!(indices.is_empty());
    }

    #[test]
    fn test_boilerplate_indices_shared_chunks() {
        let chunks = vec![
            "shared header".to_string(),
            "unique body content".to_string(),
            "shared footer".to_string(),
        ];
        let mut counts = HashMap::new();
        counts.insert(content_hash("shared header"), 10i64);
        counts.insert(content_hash("shared footer"), 8i64);
        let indices = boilerplate_indices(&chunks, &counts, 5);
        assert_eq!(indices, HashSet::from([0, 2]));
    }

    #[test]
    fn test_boilerplate_indices_fallback_all_boilerplate() {
        let chunks = vec![
            "shared chunk a".to_string(),
            "shared chunk b".to_string(),
        ];
        let mut counts = HashMap::new();
        counts.insert(content_hash("shared chunk a"), 20i64);
        counts.insert(content_hash("shared chunk b"), 15i64);
        // All chunks are boilerplate → fallback: return empty set
        let indices = boilerplate_indices(&chunks, &counts, 5);
        assert!(indices.is_empty(), "should fall back to empty when all chunks are boilerplate");
    }

    #[test]
    fn test_boilerplate_below_threshold_not_filtered() {
        let chunks = vec!["shared header".to_string()];
        let mut counts = HashMap::new();
        counts.insert(content_hash("shared header"), 3i64); // below threshold of 5
        let indices = boilerplate_indices(&chunks, &counts, 5);
        assert!(indices.is_empty());
    }

    #[test]
    fn test_boilerplate_threshold_zero_disabled() {
        let chunks = vec!["any content".to_string()];
        let mut counts = HashMap::new();
        counts.insert(content_hash("any content"), 100i64);
        // threshold = 0 means disabled → nothing filtered
        let indices = boilerplate_indices(&chunks, &counts, 0);
        assert!(indices.is_empty());
    }
}
