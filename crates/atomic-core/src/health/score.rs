//! Health score aggregation.
//!
//! The overall health score is a weighted mean of individual check scores.
//! Weights can be overridden per database via `HealthConfig`. Informational
//! checks (e.g. `content_quality`) are excluded from the score by default
//! and can be opted in by setting an explicit weight.

use super::types::{HealthCheckResult, HealthConfig};
use std::collections::HashMap;

/// Default check weights. Must sum to 1.0.
///
/// Design: defaults include only checks that represent near-universal data
/// integrity problems (coverage of the pipeline, orphaned references, broken
/// links, accidental duplicates). Opinionated "completeness" checks
/// (wiki_coverage, content_quality, contradiction_detection, boilerplate_pollution)
/// are returned with `informational: true` and excluded from the overall score
/// by default. Users can opt-in to weighting them via per-DB HealthConfig.
pub(crate) const CHECK_WEIGHTS: &[(&str, f64)] = &[
    ("embedding_coverage", 0.20),
    ("tagging_coverage", 0.20),
    ("orphan_tags", 0.15),
    ("source_uniqueness", 0.10),
    ("semantic_graph_freshness", 0.10),
    ("tag_health", 0.10),
    ("broken_internal_links", 0.10),
    ("content_overlap", 0.05),
];

/// Aggregate individual check scores into a single 0-100 overall score.
///
/// Rules:
/// - Informational checks contribute **only** when the user supplied an
///   explicit `weight` override via `HealthConfig`.
/// - Disabled checks (`enabled: false`) contribute nothing.
/// - Default-weighted checks fall back to `CHECK_WEIGHTS` when the config is
///   empty, matching the no-config behaviour.
pub fn aggregate_score(
    checks: &HashMap<String, HealthCheckResult>,
    config: Option<&HealthConfig>,
) -> u32 {
    let default_weights: HashMap<&str, f64> = CHECK_WEIGHTS.iter().copied().collect();
    let mut total = 0.0_f64;
    let mut weight_sum = 0.0_f64;
    for (name, check) in checks {
        // Respect enabled flag.
        let override_entry = config.and_then(|c| c.overrides.get(name));
        if let Some(o) = override_entry {
            if !o.enabled {
                continue;
            }
        }
        // Effective weight: explicit override wins; else default (0 for informational).
        let weight = match override_entry.and_then(|o| o.weight) {
            Some(w) => w,
            None => {
                if check.informational {
                    0.0
                } else {
                    default_weights.get(name.as_str()).copied().unwrap_or(0.0)
                }
            }
        };
        if weight <= 0.0 {
            continue;
        }
        total += (check.score as f64) * weight;
        weight_sum += weight;
    }
    if weight_sum == 0.0 {
        return 100;
    }
    ((total / weight_sum).round() as u32).min(100)
}
