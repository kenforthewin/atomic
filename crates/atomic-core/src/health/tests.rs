//! Unit tests for health check functions.
//!
//! Tests use manually constructed `HealthRawData` fixtures to validate
//! scoring, `requires_review` logic, and JSON data shapes — no database required.

#[cfg(test)]
mod tests {
    use super::super::checks;
    use super::super::{
        AtomPreview, BoilerplateAtomEntry, ContradictionAtom, ContradictionPairEntry,
        DuplicatePair, RootlessTagEntry, WikiGap, WikiStaleEntry,
    };
    use crate::storage::sqlite::health::HealthRawData;

    fn base_raw() -> HealthRawData {
        HealthRawData {
            total_atoms: 50,
            embedding_complete: 50,
            tagging_complete: 50,
            ..Default::default()
        }
    }

    // --- embedding_coverage ---

    #[test]
    fn test_embedding_coverage_perfect() {
        let mut raw = base_raw();
        raw.embedding_complete = 50;
        let result = checks::embedding_coverage(&raw);
        assert_eq!(result.status, "ok");
        assert_eq!(result.score, 100);
        assert!(!result.requires_review);
        assert!(!result.auto_fixable);
    }

    #[test]
    fn test_embedding_coverage_with_failures() {
        let mut raw = base_raw();
        raw.embedding_failed = 5;
        let result = checks::embedding_coverage(&raw);
        assert_ne!(result.status, "ok");
        assert!(result.auto_fixable);
        assert!(result.score < 100);
    }

    #[test]
    fn test_embedding_coverage_all_pending() {
        let mut raw = base_raw();
        raw.embedding_pending = 50;
        raw.embedding_complete = 0;
        let result = checks::embedding_coverage(&raw);
        assert!(result.score < 100);
        assert!(result.auto_fixable);
    }

    // --- tagging_coverage ---

    #[test]
    fn test_tagging_coverage_perfect() {
        let raw = base_raw();
        let result = checks::tagging_coverage(&raw);
        assert_eq!(result.status, "ok");
        assert_eq!(result.score, 100);
        assert!(!result.requires_review);
    }

    #[test]
    fn test_tagging_coverage_untagged_atoms() {
        let mut raw = base_raw();
        raw.untagged_complete = 10;
        let result = checks::tagging_coverage(&raw);
        assert_ne!(result.status, "ok");
        assert!(result.auto_fixable);
    }

    // --- content_overlap ---

    #[test]
    fn test_content_overlap_no_pairs() {
        let raw = base_raw();
        let result = checks::content_overlap(&raw);
        assert_eq!(result.status, "ok");
        assert!(!result.requires_review);
    }

    #[test]
    fn test_content_overlap_with_pairs() {
        let mut raw = base_raw();
        raw.duplicate_pairs.push(DuplicatePair {
            pair_id: "p1".to_string(),
            atom_a_id: "a1".to_string(),
            atom_a_title: "Article A".to_string(),
            atom_a_source: Some("https://source1.com/a".to_string()),
            atom_b_id: "b1".to_string(),
            atom_b_title: "Article B".to_string(),
            atom_b_source: Some("https://source2.com/b".to_string()),
            similarity: 0.72,
            shared_tag_count: 3,
        });
        let result = checks::content_overlap(&raw);
        assert_ne!(result.status, "ok");
        assert!(result.requires_review);
        assert!(!result.auto_fixable);
        // Verify pairs appear in data
        let pairs = result.data["pairs"].as_array().unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0]["atom_a"]["id"], "a1");
        assert_eq!(pairs[0]["atom_a"]["title"], "Article A");
    }

    // --- content_quality ---

    #[test]
    fn test_content_quality_perfect() {
        let raw = base_raw();
        let result = checks::content_quality(&raw);
        assert_eq!(result.status, "ok");
        assert!(!result.requires_review);
    }

    #[test]
    fn test_content_quality_no_source_atoms() {
        let mut raw = base_raw();
        raw.no_source_atoms.push(AtomPreview {
            id: "atom-1".to_string(),
            title: "My Note".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        });
        raw.no_source_atoms.push(AtomPreview {
            id: "atom-2".to_string(),
            title: "Another Note".to_string(),
            created_at: "2026-01-02T00:00:00Z".to_string(),
        });
        let result = checks::content_quality(&raw);
        assert!(result.requires_review);
        // Check data shape
        let atoms = &result.data["issues"]["no_source"]["atoms"];
        assert_eq!(atoms.as_array().unwrap().len(), 2);
        assert_eq!(atoms[0]["id"], "atom-1");
        assert_eq!(atoms[0]["title"], "My Note");
        assert_eq!(atoms[0]["created_at"], "2026-01-01T00:00:00Z");
        // auto_fixable should be false for no_source
        assert_eq!(result.data["issues"]["no_source"]["auto_fixable"], false);
    }

    #[test]
    fn test_content_quality_short_atoms() {
        let mut raw = base_raw();
        raw.very_short_atoms.push("short-1".to_string());
        let result = checks::content_quality(&raw);
        assert!(result.auto_fixable);
        assert_eq!(result.data["issues"]["very_short"]["count"], 1);
    }

    // --- boilerplate_pollution ---

    #[test]
    fn test_boilerplate_no_pollution() {
        let raw = base_raw();
        let result = checks::boilerplate_pollution(&raw);
        assert_eq!(result.status, "ok");
        assert!(!result.requires_review);
        assert_eq!(result.data["count"], 0);
    }

    #[test]
    fn test_boilerplate_with_affected_atoms() {
        let mut raw = base_raw();
        raw.boilerplate_affected_atoms.push(BoilerplateAtomEntry {
            id: "atom-bp-1".to_string(),
            title: "Boilerplate Article".to_string(),
            clone_count: 5,
        });
        raw.boilerplate_affected_atoms.push(BoilerplateAtomEntry {
            id: "atom-bp-2".to_string(),
            title: "Template Note".to_string(),
            clone_count: 3,
        });
        let result = checks::boilerplate_pollution(&raw);
        assert_ne!(result.status, "ok");
        assert!(result.requires_review);
        assert_eq!(result.data["count"], 2);
        let atoms = result.data["affected_atoms"].as_array().unwrap();
        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[0]["id"], "atom-bp-1");
        assert_eq!(atoms[0]["title"], "Boilerplate Article");
        assert_eq!(atoms[0]["clone_count"], 5);
    }

    // --- contradiction_detection ---

    #[test]
    fn test_contradiction_no_pairs() {
        let raw = base_raw();
        let result = checks::contradiction_detection(&raw);
        assert_eq!(result.status, "ok");
        assert!(!result.requires_review);
        assert_eq!(result.data["potential_contradictions"], 0);
        assert!(result.data["pairs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_contradiction_with_pairs() {
        let mut raw = base_raw();
        raw.contradiction_pairs.push(ContradictionPairEntry {
            pair_id: "cp1".to_string(),
            atom_a: ContradictionAtom {
                id: "ca1".to_string(),
                title: "Article on Topic X - Version 1".to_string(),
                source: Some("https://site1.com/x".to_string()),
            },
            atom_b: ContradictionAtom {
                id: "cb1".to_string(),
                title: "Article on Topic X - Version 2".to_string(),
                source: Some("https://site2.com/x".to_string()),
            },
            similarity: 0.85,
            shared_tag_count: 2,
        });
        raw.contradiction_candidate_count = 1;
        let result = checks::contradiction_detection(&raw);
        assert_ne!(result.status, "ok");
        assert!(result.requires_review);
        let pairs = result.data["pairs"].as_array().unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0]["pair_id"], "cp1");
        assert_eq!(pairs[0]["atom_a"]["title"], "Article on Topic X - Version 1");
        // f32 serializes with limited precision; compare as f64 with tolerance
        let sim = pairs[0]["similarity"].as_f64().unwrap();
        assert!((sim - 0.85).abs() < 0.001, "expected ~0.85, got {sim}");
    }

    // --- tag_health ---

    #[test]
    fn test_tag_health_perfect() {
        let raw = base_raw();
        let result = checks::tag_health(&raw);
        assert_eq!(result.status, "ok");
        assert!(!result.requires_review);
        let rootless_list = result.data["rootless_tag_list"].as_array().unwrap();
        assert!(rootless_list.is_empty());
    }

    #[test]
    fn test_tag_health_rootless_tags() {
        let mut raw = base_raw();
        raw.rootless_tag_list.push(RootlessTagEntry {
            id: "tag-1".to_string(),
            name: "Orphaned Category".to_string(),
            atom_count: 7,
        });
        raw.rootless_tag_list.push(RootlessTagEntry {
            id: "tag-2".to_string(),
            name: "Floating Topic".to_string(),
            atom_count: 3,
        });
        raw.rootless_tags = 2;
        let result = checks::tag_health(&raw);
        assert!(result.requires_review);
        let list = result.data["rootless_tag_list"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["id"], "tag-1");
        assert_eq!(list[0]["name"], "Orphaned Category");
        assert_eq!(list[0]["atom_count"], 7);
    }

    // --- aggregate_score ---

    #[test]
    fn test_aggregate_score_all_perfect() {
        use std::collections::HashMap;
        use crate::health::HealthCheckResult;
        let mut checks_map = HashMap::new();
        for name in &["content_overlap", "embedding_coverage", "tagging_coverage",
                       "source_uniqueness", "wiki_coverage", "semantic_graph_freshness",
                       "content_quality", "orphan_tags", "tag_health", "broken_internal_links"] {
            checks_map.insert(name.to_string(), HealthCheckResult {
                status: "ok".to_string(),
                score: 100,
                auto_fixable: false,
                requires_review: false,
                fix_action: None,
                data: serde_json::Value::Null,
            });
        }
        let score = crate::health::aggregate_score(&checks_map);
        assert_eq!(score, 100);
    }

    #[test]
    fn test_aggregate_score_mixed() {
        use std::collections::HashMap;
        use crate::health::HealthCheckResult;
        let mut checks_map = HashMap::new();
        // tagging_coverage at 0 (weight 0.20) → expected ~80
        for name in &["content_overlap", "embedding_coverage", "source_uniqueness",
                       "wiki_coverage", "semantic_graph_freshness",
                       "content_quality", "orphan_tags", "tag_health", "broken_internal_links"] {
            checks_map.insert(name.to_string(), HealthCheckResult {
                status: "ok".to_string(),
                score: 100,
                auto_fixable: false,
                requires_review: false,
                fix_action: None,
                data: serde_json::Value::Null,
            });
        }
        checks_map.insert("tagging_coverage".to_string(), HealthCheckResult {
            status: "error".to_string(),
            score: 0,
            auto_fixable: true,
            requires_review: false,
            fix_action: Some("retry_tagging_pipeline".to_string()),
            data: serde_json::Value::Null,
        });
        let score = crate::health::aggregate_score(&checks_map);
        // tagging = 0.0 * 0.20 + others = 1.0 * 0.80 → 80
        assert_eq!(score, 80);
    }

    // --- boilerplate_indices integration ---

    #[test]
    fn test_boilerplate_filtering_preserves_unique_chunks() {
        use crate::boilerplate::{boilerplate_indices, content_hash};
        use std::collections::HashMap;
        let chunks = vec![
            "# Privacy Policy\n\nAll rights reserved.".to_string(),
            "This atom is about machine learning and neural networks.".to_string(),
            "# Privacy Policy\n\nAll rights reserved.".to_string(),
        ];
        let mut counts = HashMap::new();
        let bp_hash = content_hash("# Privacy Policy\n\nAll rights reserved.");
        counts.insert(bp_hash, 20i64);
        let indices = boilerplate_indices(&chunks, &counts, 5);
        assert!(indices.contains(&0));
        assert!(!indices.contains(&1));
        assert!(indices.contains(&2));
    }
}
