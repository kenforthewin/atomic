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
            atom_a_created_at: None,
            atom_b_created_at: None,
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

    #[test]
    fn test_content_overlap_created_at_in_json() {
        let mut raw = base_raw();
        raw.duplicate_pairs.push(DuplicatePair {
            pair_id: "p2".to_string(),
            atom_a_id: "a2".to_string(),
            atom_a_title: "Article A".to_string(),
            atom_a_source: None,
            atom_b_id: "b2".to_string(),
            atom_b_title: "Article B".to_string(),
            atom_b_source: None,
            similarity: 0.70,
            shared_tag_count: 2,
            atom_a_created_at: Some("2026-01-01T00:00:00Z".to_string()),
            atom_b_created_at: Some("2026-02-01T00:00:00Z".to_string()),
        });
        let result = checks::content_overlap(&raw);
        let pairs = result.data["pairs"].as_array().unwrap();
        assert_eq!(pairs[0]["atom_a"]["created_at"], "2026-01-01T00:00:00Z");
        assert_eq!(pairs[0]["atom_b"]["created_at"], "2026-02-01T00:00:00Z");
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
                created_at: None,
            },
            atom_b: ContradictionAtom {
                id: "cb1".to_string(),
                title: "Article on Topic X - Version 2".to_string(),
                source: Some("https://site2.com/x".to_string()),
                created_at: None,
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


    #[test]
    fn test_contradiction_created_at_in_json() {
        let mut raw = base_raw();
        raw.contradiction_pairs.push(ContradictionPairEntry {
            pair_id: "cp2".to_string(),
            atom_a: ContradictionAtom {
                id: "ca2".to_string(),
                title: "Topic A".to_string(),
                source: None,
                created_at: Some("2026-01-15T00:00:00Z".to_string()),
            },
            atom_b: ContradictionAtom {
                id: "cb2".to_string(),
                title: "Topic B".to_string(),
                source: None,
                created_at: Some("2026-03-15T00:00:00Z".to_string()),
            },
            similarity: 0.88,
            shared_tag_count: 1,
        });
        let result = checks::contradiction_detection(&raw);
        let pairs = result.data["pairs"].as_array().unwrap();
        assert_eq!(pairs[0]["atom_a"]["created_at"], "2026-01-15T00:00:00Z");
        assert_eq!(pairs[0]["atom_b"]["created_at"], "2026-03-15T00:00:00Z");
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


    #[test]
    fn test_tag_health_similar_name_pairs_list() {
        let mut raw = base_raw();
        raw.similar_name_pairs_list = vec![
            ("id-a".to_string(), "Machine Learning".to_string(), "id-b".to_string(), "Learning".to_string()),
        ];
        raw.similar_name_pair_count = 1;
        let result = checks::tag_health(&raw);
        assert_eq!(result.status, "warning");
        let pair_list = result.data["similar_name_pair_list"].as_array().unwrap();
        assert_eq!(pair_list.len(), 1);
        assert_eq!(pair_list[0]["a_name"], "Machine Learning");
        assert_eq!(pair_list[0]["b_name"], "Learning");
        assert_eq!(pair_list[0]["pair_id"], "id-a__id-b");
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

    // --- pair_key and apply_dismissals ---

    #[test]
    fn test_pair_key_sorted() {
        use crate::health::pair_key;
        assert_eq!(pair_key("a", "b"), "a__b");
        assert_eq!(pair_key("b", "a"), "a__b");
        assert_eq!(pair_key("z1", "z2"), "z1__z2");
    }

    #[test]
    fn test_apply_dismissals_filters_content_overlap_pairs() {
        use crate::health::{apply_dismissals, pair_key, HealthCheckResult};
        use std::collections::HashSet;
        let mut result = HealthCheckResult {
            status: "warning".into(),
            score: 60,
            auto_fixable: false,
            requires_review: true,
            fix_action: None,
            data: serde_json::json!({
                "count": 2,
                "cross_source_overlaps": 2,
                "pairs": [
                    {"atom_a": {"id": "a1"}, "atom_b": {"id": "b1"}},
                    {"atom_a": {"id": "a2"}, "atom_b": {"id": "b2"}},
                ]
            }),
        };
        let mut dismissed = HashSet::new();
        dismissed.insert(pair_key("a1", "b1"));
        apply_dismissals("content_overlap", &mut result, &dismissed);
        let pairs = result.data["pairs"].as_array().unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0]["atom_a"]["id"], "a2");
        assert_eq!(result.data["count"], 1);
    }

    #[test]
    fn test_apply_dismissals_filters_no_source() {
        use crate::health::{apply_dismissals, HealthCheckResult};
        use std::collections::HashSet;
        let mut result = HealthCheckResult {
            status: "warning".into(),
            score: 70,
            auto_fixable: false,
            requires_review: true,
            fix_action: None,
            data: serde_json::json!({
                "issues": {
                    "no_source": {
                        "count": 2,
                        "atoms": [
                            {"id": "a1", "title": "A"},
                            {"id": "a2", "title": "B"}
                        ]
                    }
                }
            }),
        };
        let mut dismissed = HashSet::new();
        dismissed.insert("a1".to_string());
        apply_dismissals("content_quality", &mut result, &dismissed);
        let atoms = result.data["issues"]["no_source"]["atoms"].as_array().unwrap();
        assert_eq!(atoms.len(), 1);
        assert_eq!(atoms[0]["id"], "a2");
        assert_eq!(result.data["issues"]["no_source"]["count"], 1);
    }

    #[test]
    fn test_apply_dismissals_filters_rootless_tags() {
        use crate::health::{apply_dismissals, HealthCheckResult};
        use std::collections::HashSet;
        let mut result = HealthCheckResult {
            status: "warning".into(),
            score: 80,
            auto_fixable: false,
            requires_review: true,
            fix_action: None,
            data: serde_json::json!({
                "rootless_tags": 2,
                "rootless_tag_list": [
                    {"id": "t1", "name": "Foo", "atom_count": 3},
                    {"id": "t2", "name": "Bar", "atom_count": 1}
                ]
            }),
        };
        let mut dismissed = HashSet::new();
        dismissed.insert("t1".to_string());
        apply_dismissals("tag_health", &mut result, &dismissed);
        let tags = result.data["rootless_tag_list"].as_array().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0]["id"], "t2");
        assert_eq!(result.data["rootless_tags"], 1);
    }

    #[test]
    fn test_apply_dismissals_empty_set_noop() {
        use crate::health::{apply_dismissals, HealthCheckResult};
        use std::collections::HashSet;
        let mut result = HealthCheckResult {
            status: "warning".into(),
            score: 60,
            auto_fixable: false,
            requires_review: true,
            fix_action: None,
            data: serde_json::json!({"count": 1, "pairs": [{"atom_a": {"id": "a"}, "atom_b": {"id": "b"}}]}),
        };
        apply_dismissals("content_overlap", &mut result, &HashSet::new());
        assert_eq!(result.data["pairs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_apply_dismissals_clears_requires_review_when_empty() {
        use crate::health::{apply_dismissals, HealthCheckResult};
        use std::collections::HashSet;
        let mut result = HealthCheckResult {
            status: "warning".into(),
            score: 60,
            auto_fixable: false,
            requires_review: true,
            fix_action: None,
            data: serde_json::json!({
                "count": 1,
                "affected_atoms": [{"id": "a1", "title": "x", "clone_count": 3}]
            }),
        };
        let mut d = HashSet::new();
        d.insert("a1".to_string());
        apply_dismissals("boilerplate_pollution", &mut result, &d);
        assert!(!result.requires_review);
        assert_eq!(result.data["count"], 0);
    }

    // --- tag_health: single_atom_tag_list ---

    #[test]
    fn test_tag_health_single_atom_tag_list() {
        use crate::health::SingleAtomTagEntry;
        let mut raw = base_raw();
        // Tag A: 1 atom, autotag=true
        raw.single_atom_tag_list.push(SingleAtomTagEntry {
            id: "tag-a".to_string(),
            name: "AutoTag".to_string(),
            is_autotag: true,
        });
        // Tag B: 1 atom, autotag=false (user-created)
        raw.single_atom_tag_list.push(SingleAtomTagEntry {
            id: "tag-b".to_string(),
            name: "UserTag".to_string(),
            is_autotag: false,
        });
        raw.single_atom_tags = 2;

        let result = checks::tag_health(&raw);

        // Expect the list in JSON data
        let list = result.data["single_atom_tag_list"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["id"], "tag-a");
        assert_eq!(list[0]["is_autotag"], true);
        assert_eq!(list[1]["id"], "tag-b");
        assert_eq!(list[1]["is_autotag"], false);
    }

    #[test]
    fn test_tag_health_auto_fixable_requires_autotag_threshold() {
        use crate::health::SingleAtomTagEntry;
        let mut raw = base_raw();
        // Only 2 autotag single-atom tags — below threshold of 3
        for i in 0..2 {
            raw.single_atom_tag_list.push(SingleAtomTagEntry {
                id: format!("tag-{}", i),
                name: format!("Tag{}", i),
                is_autotag: true,
            });
        }
        raw.single_atom_tags = 2;
        let result = checks::tag_health(&raw);
        // auto_fixable = false because count <= 3
        assert!(!result.auto_fixable);

        // Now add enough to exceed threshold
        let mut raw2 = base_raw();
        for i in 0..4 {
            raw2.single_atom_tag_list.push(SingleAtomTagEntry {
                id: format!("tag-{}", i),
                name: format!("Tag{}", i),
                is_autotag: true,
            });
        }
        raw2.single_atom_tags = 4;
        let result2 = checks::tag_health(&raw2);
        assert!(result2.auto_fixable);
    }

    #[test]
    fn test_apply_dismissals_filters_single_atom_tag_list() {
        use crate::health::{apply_dismissals, HealthCheckResult, SingleAtomTagEntry};
        use std::collections::HashSet;

        let mut raw = base_raw();
        raw.single_atom_tag_list.push(SingleAtomTagEntry { id: "tag-x".to_string(), name: "X".to_string(), is_autotag: false });
        raw.single_atom_tag_list.push(SingleAtomTagEntry { id: "tag-y".to_string(), name: "Y".to_string(), is_autotag: true });
        raw.single_atom_tags = 2;
        let mut result = checks::tag_health(&raw);

        let mut dismissed = HashSet::new();
        dismissed.insert("tag-x".to_string());
        apply_dismissals("tag_health", &mut result, &dismissed);

        let list = result.data["single_atom_tag_list"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], "tag-y");
        // Count updated
        assert_eq!(result.data["single_atom_tags"], 1);
    }
    // --- requires_review covers similar too ---

    #[test]
    fn test_tag_health_requires_review_when_similar() {
        let mut raw = base_raw();
        raw.similar_name_pairs_list = vec![(
            "id-a".to_string(), "AI".to_string(),
            "id-b".to_string(), "Artificial Intelligence".to_string(),
        )];
        raw.similar_name_pair_count = 1;
        let result = checks::tag_health(&raw);
        assert!(result.requires_review);
    }
}

#[cfg(test)]
mod integration_tests {
    use tempfile::TempDir;
    use crate::AtomicCore;
    use crate::health::{compute_health, fixes};

    fn open_core() -> (AtomicCore, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("health_test.db")).unwrap();
        (core, dir)
    }

    #[tokio::test]
    async fn test_broken_link_check_detects_unresolved_markdown_link() {
        let (core, _dir) = open_core();

        // Atom A — exists with a known source URL
        core.create_atom(crate::CreateAtomRequest {
            content: "Alpha content".to_string(),
            source_url: Some("vault://notes/alpha.md".to_string()),
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        }, |_| {}).await.expect("create atom A");

        // Atom B — has a broken link to ./bravo.md which doesn't exist
        let atom_b = core.create_atom(crate::CreateAtomRequest {
            content: "see [bravo](./bravo.md) for more".to_string(),
            source_url: Some("vault://notes/beta.md".to_string()),
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        }, |_| {}).await.expect("create atom B").expect("atom B created");

        let report = compute_health(&core).await.expect("compute_health");
        let link_check = report.checks.get("broken_internal_links").expect("check present");

        assert_eq!(link_check.status, "warning", "should be warning");
        let list = link_check.data["broken_link_list"].as_array().expect("broken_link_list array");
        assert_eq!(list.len(), 1, "one atom with broken link");
        assert_eq!(list[0]["atom_id"].as_str().unwrap(), atom_b.atom.id);
        let links = list[0]["links"].as_array().expect("links array");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0]["raw"].as_str().unwrap(), "[bravo](./bravo.md)");
        assert_eq!(links[0]["kind"].as_str().unwrap(), "markdown");
    }

    #[tokio::test]
    async fn test_remove_broken_link_strips_markdown_link() {
        let (core, _dir) = open_core();

        let atom = core.create_atom(crate::CreateAtomRequest {
            content: "see [bravo](./bravo.md) for details".to_string(),
            source_url: Some("vault://notes/beta.md".to_string()),
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        }, |_| {}).await.expect("create atom").expect("atom created");

        fixes::remove_broken_link(&core, &atom.atom.id, "[bravo](./bravo.md)")
            .await
            .expect("remove_broken_link");

        let updated = core.get_atom(&atom.atom.id).await.expect("get_atom").expect("atom exists");
        assert_eq!(updated.atom.content, "see bravo for details");
    }

    #[tokio::test]
    async fn test_dismiss_broken_link_filters_from_check() {
        let (core, _dir) = open_core();

        let atom_b = core.create_atom(crate::CreateAtomRequest {
            content: "see [bravo](./bravo.md)".to_string(),
            source_url: Some("vault://notes/beta.md".to_string()),
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        }, |_| {}).await.expect("create atom").expect("atom created");

        // Verify it appears as broken first
        let report = compute_health(&core).await.expect("compute_health");
        let check = report.checks.get("broken_internal_links").expect("check");
        assert_eq!(check.status, "warning");

        // Dismiss the atom
        core.dismiss_health_item("broken_internal_links", &atom_b.atom.id, "ignored_broken_links", None)
            .await
            .expect("dismiss");

        // Re-run — broken_link_list for B should be filtered out
        let report2 = compute_health(&core).await.expect("compute_health 2");
        let check2 = report2.checks.get("broken_internal_links").expect("check2");
        let list2 = check2.data["broken_link_list"].as_array().expect("list");
        assert!(
            list2.iter().all(|e| e["atom_id"].as_str().unwrap() != atom_b.atom.id),
            "dismissed atom should be filtered out"
        );
        assert_eq!(check2.data["broken_count"].as_i64().unwrap(), 0);
    }

    #[tokio::test]
    async fn test_suggest_atoms_by_query_source_url_exact() {
        let (core, _dir) = open_core();

        // Atom A with known source_url
        let atom_a = core.create_atom(crate::CreateAtomRequest {
            content: "# Bravo Notes\n\nContent here".to_string(),
            source_url: Some("vault://notes/bravo.md".to_string()),
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        }, |_| {}).await.expect("create A").expect("A created");

        // Atom B — no source_url
        core.create_atom(crate::CreateAtomRequest {
            content: "# Other Atom".to_string(),
            source_url: None,
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        }, |_| {}).await.expect("create B");

        let results = core
            .suggest_atoms_for_broken_link("bravo.md", 5)
            .await
            .expect("suggest");

        assert!(!results.is_empty(), "should return at least one result");
        let top = &results[0];
        assert_eq!(top.0, atom_a.atom.id, "top hit should be atom A");
        assert!((top.3 - 1.0f32).abs() < 0.01, "score should be 1.0 for exact suffix match");
    }

    #[tokio::test]
    async fn test_relink_broken_link_rewrites_markdown() {
        let (core, _dir) = open_core();

        // Atom A — the target
        let atom_a = core.create_atom(crate::CreateAtomRequest {
            content: "# Bravo Notes".to_string(),
            source_url: Some("vault://notes/bravo.md".to_string()),
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        }, |_| {}).await.expect("create A").expect("A created");

        // Atom C — has the broken link
        let atom_c = core.create_atom(crate::CreateAtomRequest {
            content: "see [bravo](./bravo.md) for details".to_string(),
            source_url: Some("vault://notes/c.md".to_string()),
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        }, |_| {}).await.expect("create C").expect("C created");

        fixes::relink_broken_link(&core, &atom_c.atom.id, "[bravo](./bravo.md)", &atom_a.atom.id)
            .await
            .expect("relink_broken_link");

        let updated = core.get_atom(&atom_c.atom.id).await.expect("get C").expect("C exists");
        let expected = format!("see [bravo](atom://{}) for details", atom_a.atom.id);
        assert_eq!(updated.atom.content, expected, "link should be rewritten to atom://");
    }
}

#[cfg(test)]
mod llm_tests {
    //! Unit tests for `verify_overlap_pair`, `verify_contradiction_pair`, and
    //! `merge_contradicting_pair`.  Each test spins up a `wiremock::MockServer`
    //! acting as an OpenAI-compatible endpoint, configures the core settings to
    //! use it, then asserts the expected behaviour without a real LLM.

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use crate::AtomicCore;
    use crate::health::llm_fixes;

    async fn open_core_with_llm(mock_url: &str) -> (AtomicCore, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("llm_test.db")).unwrap();
        // Point the core's LLM provider at the mock server via openai_compat.
        for (k, v) in [
            ("provider", "openai_compat"),
            ("openai_compat_base_url", mock_url),
            ("openai_compat_llm_model", "test-model"),
            ("wiki_model", "test-model"),
        ] {
            core.storage()
                .set_setting_sync(k, v)
                .await.expect("set setting");
        }
        (core, dir)
    }

    fn chat_completion_body(content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1699000000u64,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        })
    }

    #[tokio::test]
    async fn test_verify_overlap_pair_false_positive_is_dismissed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(chat_completion_body(
                    r#"{"duplicate": false, "reason": "different topics"} "#,
                )),
            )
            .mount(&server)
            .await;

        let (core, _dir) = open_core_with_llm(&server.uri()).await;
        let atom_a = core.create_atom(crate::CreateAtomRequest {
            content: "Rust ownership rules".to_string(),
            source_url: None, published_at: None, tag_ids: vec![], skip_if_source_exists: false,
        }, |_| {}).await.unwrap().unwrap();
        let atom_b = core.create_atom(crate::CreateAtomRequest {
            content: "Python GIL internals".to_string(),
            source_url: None, published_at: None, tag_ids: vec![], skip_if_source_exists: false,
        }, |_| {}).await.unwrap().unwrap();

        let (is_dup, reason) =
            llm_fixes::verify_overlap_pair(&core, &atom_a.atom.id, &atom_b.atom.id)
                .await
                .expect("verify_overlap_pair");

        assert!(!is_dup, "should report not duplicate");
        assert!(!reason.is_empty());
    }

    #[tokio::test]
    async fn test_verify_contradiction_pair_false_positive_is_dismissed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(chat_completion_body(
                    r#"{"contradiction": false, "reason": "no conflict found"} "#,
                )),
            )
            .mount(&server)
            .await;

        let (core, _dir) = open_core_with_llm(&server.uri()).await;
        let atom_a = core.create_atom(crate::CreateAtomRequest {
            content: "The sky is blue".to_string(),
            source_url: None, published_at: None, tag_ids: vec![], skip_if_source_exists: false,
        }, |_| {}).await.unwrap().unwrap();
        let atom_b = core.create_atom(crate::CreateAtomRequest {
            content: "Water is H2O".to_string(),
            source_url: None, published_at: None, tag_ids: vec![], skip_if_source_exists: false,
        }, |_| {}).await.unwrap().unwrap();

        let (is_real, reason) =
            llm_fixes::verify_contradiction_pair(&core, &atom_a.atom.id, &atom_b.atom.id)
                .await
                .expect("verify_contradiction_pair");

        assert!(!is_real, "should report no real contradiction");
        assert!(!reason.is_empty());
    }

    #[tokio::test]
    async fn test_merge_contradicting_pair_dry_run_no_llm() {
        // dry_run returns immediately without calling LLM
        let dir = TempDir::new().expect("tempdir");
        let core = AtomicCore::open_or_create(dir.path().join("merge_test.db")).unwrap();
        let atom_a = core.create_atom(crate::CreateAtomRequest {
            content: "Speed of light is 300,000 km/s".to_string(),
            source_url: None, published_at: None, tag_ids: vec![], skip_if_source_exists: false,
        }, |_| {}).await.unwrap().unwrap();
        let atom_b = core.create_atom(crate::CreateAtomRequest {
            content: "Speed of light is 299,792 km/s".to_string(),
            source_url: None, published_at: None, tag_ids: vec![], skip_if_source_exists: false,
        }, |_| {}).await.unwrap().unwrap();

        let action = llm_fixes::merge_contradicting_pair(
            &core, &atom_a.atom.id, &atom_b.atom.id, true,
        )
        .await
        .expect("merge_contradicting_pair dry_run");

        let fa = action.expect("dry_run returns Some(FixAction)");
        assert_eq!(fa.id, "dry_run");
        assert_eq!(fa.check, "contradiction_detection");
        assert_eq!(fa.action, "merge_with_llm");
    }

    #[tokio::test]
    async fn test_propose_tag_restructure_parses_and_persists() {
        let proposal_json = r#"{
  "summary": "Merge near-duplicate technology tags.",
  "actions": [
    {"kind": "merge", "from_id": "t1", "into_id": "t2", "from_name": "rust-lang", "into_name": "rust", "reason": "same concept"},
    {"kind": "rename", "tag_id": "t3", "old_name": "ML", "new_name": "machine-learning", "reason": "spell out abbreviation"}
  ]
}"#;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(chat_completion_body(proposal_json)),
            )
            .mount(&server)
            .await;

        let (core, _dir) = open_core_with_llm(&server.uri()).await;

        let proposal = llm_fixes::propose_tag_restructure(&core)
            .await
            .expect("propose_tag_restructure");

        assert_eq!(proposal.summary, "Merge near-duplicate technology tags.");
        assert_eq!(proposal.actions.len(), 2);

        // Verify it was persisted.
        let latest = core
            .get_latest_tag_proposal()
            .await
            .expect("get_latest_tag_proposal")
            .expect("should have a pending proposal");
        assert_eq!(latest.id, proposal.id);
        assert_eq!(latest.actions.len(), 2);
    }
}