//! Individual health check implementations.
//!
//! Each check takes a `&HealthRawData` snapshot (fetched once) and returns a
//! `HealthCheckResult` with a 0–100 score and check-specific JSON data.

use super::{DuplicatePair, HealthCheckResult, WikiGap, WikiStaleEntry};
use crate::storage::sqlite::health::HealthRawData;
use serde_json::json;
use std::collections::HashMap;

/// Run all 10 checks against pre-fetched raw data.
pub fn run_all(raw: &HealthRawData) -> HashMap<String, HealthCheckResult> {
    let mut map = HashMap::new();
    map.insert("embedding_coverage".to_string(), embedding_coverage(raw));
    map.insert("tagging_coverage".to_string(), tagging_coverage(raw));
    map.insert("source_uniqueness".to_string(), source_uniqueness(raw));
    map.insert("orphan_tags".to_string(), orphan_tags(raw));
    map.insert(
        "semantic_graph_freshness".to_string(),
        semantic_graph_freshness(raw),
    );
    map.insert("wiki_coverage".to_string(), wiki_coverage(raw));
    map.insert("content_quality".to_string(), content_quality(raw));
    map.insert("tag_health".to_string(), tag_health(raw));
    map.insert("content_overlap".to_string(), content_overlap(raw));
    map.insert(
        "contradiction_detection".to_string(),
        contradiction_detection(raw),
    );
    // Diagnostic check — not included in CHECK_WEIGHTS, doesn't affect score.
    // Surfaces boilerplate-dominated atoms to the UI without penalising the KB.
    map.insert(
        "boilerplate_pollution".to_string(),
        boilerplate_pollution(raw),
    );
    map
}

// ==================== Individual checks ====================

pub fn embedding_coverage(raw: &HealthRawData) -> HealthCheckResult {
    let total = raw.total_atoms;
    let complete = raw.embedding_complete;
    let pending = raw.embedding_pending;
    let processing = raw.embedding_processing;
    let failed = raw.embedding_failed;

    let score = if total == 0 {
        100
    } else {
        let pct = (complete as f64 / total as f64 * 100.0) as u32;
        if failed > 0 {
            pct.min(50)
        } else {
            pct
        }
    };
    let status = if score == 100 {
        "ok"
    } else if score >= 80 {
        "warning"
    } else {
        "error"
    };
    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: failed > 0 || pending > 0,
        requires_review: false,
        fix_action: Some("retry_failed_and_process_pending".to_string()),
        data: json!({
            "total": total,
            "complete": complete,
            "pending": pending,
            "processing": processing,
            "failed": failed
        }),
    }
}

pub fn tagging_coverage(raw: &HealthRawData) -> HealthCheckResult {
    let total = raw.total_atoms;
    let failed = raw.tagging_failed;
    let pending = raw.tagging_pending;
    let untagged = raw.untagged_complete;
    // skipped_untagged: the tagger skipped these atoms AND they have 0 tags.
    // Atoms with tagging_status='skipped' that DO have tags are fine —
    // they were imported with existing tags and the tagger deliberately skipped.
    let skipped_untagged = raw.skipped_untagged;

    // Only count actually-problematic atoms: failed, truly pending, complete-but-untagged,
    // and skipped-with-no-tags. Skipped atoms that HAVE tags are fine.
    let bad = (failed + pending + untagged + skipped_untagged).min(total);
    let tagged = (total - bad).max(0);

    let score = if total == 0 {
        100
    } else {
        (tagged as f64 / total as f64 * 100.0) as u32
    };
    let status = if score == 100 {
        "ok"
    } else if score >= 80 {
        "warning"
    } else {
        "error"
    };
    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: failed > 0 || pending > 0 || untagged > 0 || skipped_untagged > 0,
        requires_review: false,
        fix_action: Some("retry_tagging_pipeline".to_string()),
        data: json!({
            "total": total,
            "tagged": tagged,
            "untagged_complete": untagged,
            "skipped_untagged": skipped_untagged,
            "failed": failed,
            "pending": pending,
            "skipped_with_tags": raw.tagging_skipped - skipped_untagged
        }),
    }
}

pub fn source_uniqueness(raw: &HealthRawData) -> HealthCheckResult {
    let dup_count = raw.duplicate_sources.len() as i32;
    let score = (100i32 - dup_count * 15).max(0) as u32;
    let status = if dup_count == 0 { "ok" } else { "warning" };
    let pairs: Vec<serde_json::Value> = raw
        .duplicate_sources
        .iter()
        .map(|(url, ids)| {
            json!({
                "source_url": url,
                "atom_count": ids.len(),
                "atom_ids": ids
            })
        })
        .collect();
    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: dup_count > 0,
        requires_review: false,
        fix_action: Some("merge_exact_source_duplicates".to_string()),
        data: json!({
            "count": dup_count,
            "pairs": pairs
        }),
    }
}

pub fn orphan_tags(raw: &HealthRawData) -> HealthCheckResult {
    let count = raw.orphan_tags.len() as i32;
    let score = (100i32 - count * 2).max(0) as u32;
    let status = if count == 0 { "ok" } else { "warning" };
    let tag_list: Vec<serde_json::Value> = raw
        .orphan_tags
        .iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect();
    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: count > 0,
        requires_review: false,
        fix_action: Some("delete_orphan_tags".to_string()),
        data: json!({ "count": count, "tags": tag_list }),
    }
}

pub fn semantic_graph_freshness(raw: &HealthRawData) -> HealthCheckResult {
    let atoms_since = raw.atoms_since_edge_rebuild;
    let score = (100i32 - atoms_since * 2).max(0) as u32;
    let status = if atoms_since == 0 {
        "ok"
    } else if atoms_since <= 20 {
        "warning"
    } else {
        "error"
    };
    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: atoms_since > 0,
        requires_review: false,
        fix_action: Some("rebuild_semantic_edges".to_string()),
        data: json!({
            "last_rebuilt": raw.newest_edge_created_at,
            "newest_atom": raw.newest_atom_updated_at,
            "atoms_since_rebuild": atoms_since
        }),
    }
}

pub fn wiki_coverage(raw: &HealthRawData) -> HealthCheckResult {
    let eligible = raw.wiki_eligible_count;
    let with_wiki = raw.wiki_present_count;
    let stale = raw.wiki_stale_count;
    let without_wiki = eligible - with_wiki;

    let score = if eligible == 0 {
        100
    } else {
        let coverage_pct = (with_wiki as f64 / eligible as f64) * 70.0;
        let freshness_pct = if with_wiki == 0 {
            30.0
        } else {
            let non_stale = (with_wiki - stale).max(0);
            (non_stale as f64 / with_wiki as f64) * 30.0
        };
        (coverage_pct + freshness_pct).round() as u32
    };
    let status = if score >= 90 {
        "ok"
    } else if score >= 60 {
        "warning"
    } else {
        "error"
    };

    let gaps: Vec<serde_json::Value> = raw
        .wiki_gaps
        .iter()
        .map(|g: &WikiGap| json!({ "tag_id": g.tag_id, "tag_name": g.tag_name, "atom_count": g.atom_count }))
        .collect();
    let stale_list: Vec<serde_json::Value> = raw
        .wiki_stale
        .iter()
        .map(|s: &WikiStaleEntry| {
            json!({ "tag_id": s.tag_id, "tag_name": s.tag_name, "new_atoms": s.new_atom_count })
        })
        .collect();

    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: without_wiki > 0 || stale > 0,
        requires_review: false,
        fix_action: Some("generate_missing_wikis".to_string()),
        data: json!({
            "eligible_tags": eligible,
            "with_wiki": with_wiki,
            "without_wiki": without_wiki,
            "stale_wikis": stale,
            "gaps": gaps,
            "stale": stale_list
        }),
    }
}

pub fn content_quality(raw: &HealthRawData) -> HealthCheckResult {
    let mut issues = 0;
    if !raw.very_short_atoms.is_empty() {
        issues += 1;
    }
    if !raw.very_long_atoms.is_empty() {
        issues += 1;
    }
    if !raw.no_heading_atoms.is_empty() {
        issues += 1;
    }
    if !raw.no_source_atoms.is_empty() {
        issues += 1;
    }

    let score = (85u32).saturating_sub(issues * 5);
    let status = if issues == 0 { "ok" } else { "info" };

    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: !raw.very_short_atoms.is_empty()
            || !raw.very_long_atoms.is_empty()
            || !raw.no_heading_atoms.is_empty(),
        requires_review: !raw.no_source_atoms.is_empty(),
        fix_action: None,
        data: json!({
            "total": raw.total_atoms,
            "issues": {
                "very_short": {
                    "count": raw.very_short_atoms.len(),
                    "auto_fixable": true,
                    "atoms": raw.very_short_atoms
                },
                "very_long": {
                    "count": raw.very_long_atoms.len(),
                    "auto_fixable": true,
                    "atoms": raw.very_long_atoms
                },
                "no_headings": {
                    "count": raw.no_heading_atoms.len(),
                    "auto_fixable": true,
                    "atoms": raw.no_heading_atoms
                },
                "no_source": {
                    "count": raw.no_source_atoms.len(),
                    "auto_fixable": false,
                    "atoms": raw.no_source_atoms.iter().map(|a| json!({
                        "id": a.id,
                        "title": a.title,
                        "created_at": a.created_at
                    })).collect::<Vec<_>>()
                }
            }
        }),
    }
}

pub fn tag_health(raw: &HealthRawData) -> HealthCheckResult {
    let single = raw.single_atom_tags;
    let rootless = raw.rootless_tags;
    let similar = raw.similar_name_pair_count;

    let issues = (single > 3) as u32 + (rootless > 0) as u32 + (similar > 0) as u32;
    let score = (100u32).saturating_sub(issues * 10);
    let status = if issues == 0 { "ok" } else { "warning" };

    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: similar > 0,
        requires_review: rootless > 0,
        fix_action: None,
        data: json!({
            "single_atom_tags": single,
            "rootless_tags": rootless,
            "similar_name_pairs": similar,
            "rootless_tag_list": raw.rootless_tag_list.iter().map(|t| json!({
                "id": t.id,
                "name": t.name,
                "atom_count": t.atom_count
            })).collect::<Vec<_>>()
        }),
    }
}

pub fn content_overlap(raw: &HealthRawData) -> HealthCheckResult {
    let overlaps = raw.duplicate_pairs.len() as i32;
    let exact_dupes = raw.duplicate_sources.len() as i32;
    let template_clones = raw.boilerplate_affected_atoms.len() as i32;

    // Score: deduct for unreviewed cross-source overlaps.
    // Exact dupes handled by source_uniqueness, template clones by boilerplate_pollution.
    let score = (100i32 - overlaps * 8).max(0) as u32;
    let status = if overlaps == 0 { "ok" } else { "warning" };

    let pairs: Vec<serde_json::Value> = raw
        .duplicate_pairs
        .iter()
        .map(|p: &DuplicatePair| {
            json!({
                "pair_id": p.pair_id,
                "atom_a": { "id": p.atom_a_id, "title": p.atom_a_title, "source": p.atom_a_source },
                "atom_b": { "id": p.atom_b_id, "title": p.atom_b_title, "source": p.atom_b_source },
                "similarity": p.similarity,
                "shared_tag_count": p.shared_tag_count,
                "available_actions": ["merge_with_llm", "keep_both", "delete_older", "mark_complementary"]
            })
        })
        .collect();

    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: false,
        requires_review: overlaps > 0,
        fix_action: None,
        data: json!({
            "exact_duplicates": exact_dupes,
            "template_clones": template_clones,
            "cross_source_overlaps": overlaps,
            "count": overlaps,
            "pairs": pairs
        }),
    }
}

pub fn contradiction_detection(raw: &HealthRawData) -> HealthCheckResult {
    let pair_count = raw.contradiction_pairs.len() as i32;
    let score = (100i32 - pair_count * 8).max(0) as u32;
    let status = if pair_count == 0 { "ok" } else { "warning" };

    HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: false,
        requires_review: pair_count > 0,
        fix_action: None,
        data: json!({
            "pairs_checked": raw.contradiction_pairs_checked,
            "potential_contradictions": pair_count,
            "pairs": raw.contradiction_pairs.iter().map(|p| json!({
                "pair_id": p.pair_id,
                "atom_a": { "id": p.atom_a.id, "title": p.atom_a.title, "source": p.atom_a.source },
                "atom_b": { "id": p.atom_b.id, "title": p.atom_b.title, "source": p.atom_b.source },
                "similarity": p.similarity,
                "shared_tag_count": p.shared_tag_count
            })).collect::<Vec<_>>()
        }),
    }
}


/// Diagnostic check: atoms whose embeddings are dominated by shared boilerplate.
///
/// An atom is flagged when it has >= 2 semantic edges at similarity >= 0.99.
/// That means the vector space can't distinguish it from multiple other atoms,
/// so semantic search will return the wrong runbook / article for those queries.
///
/// This check does NOT affect the overall score (not in CHECK_WEIGHTS).
/// Fix: re-chunk excluding boilerplate sections, or re-embed with a unique-content prefix.
pub fn boilerplate_pollution(raw: &HealthRawData) -> HealthCheckResult {
    let count = raw.boilerplate_affected_atoms.len() as i32;
    let status = if count == 0 { "ok" } else { "warning" };

    HealthCheckResult {
        status: status.to_string(),
        // Always 100 — diagnostic only, does not affect overall KB score.
        score: 100,
        auto_fixable: false,
        requires_review: count > 0,
        fix_action: None,
        data: json!({
            "count": count,
            "affected_atoms": raw.boilerplate_affected_atoms.iter().map(|a| json!({
                "id": a.id,
                "title": a.title,
                "clone_count": a.clone_count
            })).collect::<Vec<_>>(),
            "description": "Atoms with >= 2 near-identical edges (similarity >= 0.99). \
                             Shared boilerplate text drowns out unique content in their \
                             embeddings. Semantic search cannot reliably distinguish \
                             these atoms from each other."
        }),
    }
}