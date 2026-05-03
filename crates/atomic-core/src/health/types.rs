//! Health data types.
//!
//! Split out of `mod.rs` to keep the orchestrator module focused on control
//! flow. All public types crossing the `atomic-core` → server boundary live
//! here; check-specific rows (`DuplicatePair`, `WikiGap`, …) also live here
//! because they're part of the JSON payload returned by the health API.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ==================== Core types ====================

/// Overall status derived from the numeric score.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    NeedsAttention,
    Degraded,
    Unhealthy,
}

impl HealthStatus {
    pub fn from_score(score: u32) -> Self {
        match score {
            90..=100 => Self::Healthy,
            70..=89 => Self::NeedsAttention,
            50..=69 => Self::Degraded,
            _ => Self::Unhealthy,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::NeedsAttention => "needs_attention",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// Result for one individual health check.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResult {
    /// "ok" | "warning" | "error"
    pub status: String,
    /// 0–100 contribution to the overall score
    pub score: u32,
    pub auto_fixable: bool,
    pub requires_review: bool,
    /// When true, this check is opinionated ("completeness-style") and does
    /// NOT contribute to the overall score. Shown as a diagnostic. The user
    /// can opt-in via health config to give it a non-zero weight.
    #[serde(default)]
    pub informational: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_action: Option<String>,
    /// Check-specific numbers, lists, pairs, etc.
    pub data: serde_json::Value,
}

/// Complete health report returned by `GET /api/health/knowledge`.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub overall_score: u32,
    pub overall_status: String,
    pub computed_at: String,
    pub atom_count: i32,
    pub checks: HashMap<String, HealthCheckResult>,
    pub auto_fixable: i32,
    pub requires_review: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_score: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_check_scores: Option<HashMap<String, u32>>,
}

/// A single action taken (or that would be taken) during a fix run.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixAction {
    /// ID of the `health_fix_log` row (for undo).
    pub id: String,
    pub check: String,
    pub action: String,
    pub count: i32,
    pub details: Vec<String>,
}

/// An issue that was skipped (too high tier, or no-op).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFix {
    pub check: String,
    pub reason: String,
    pub count: i32,
}

/// Response from `POST /api/health/fix`.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixResponse {
    pub mode: String,
    pub actions_taken: Vec<FixAction>,
    pub skipped: Vec<SkippedFix>,
    pub new_score: u32,
}

/// Fix safety tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixTier {
    /// Retry pipelines, process pending — zero risk.
    Safe,
    /// Delete orphan tags, generate missing wikis — logged, undoable.
    Low,
    /// Modify content (add headings, merge exact-source dupes) — dry-run first.
    Medium,
    /// Merges, splits, deletes — always requires user confirmation.
    High,
}

impl FixTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// What the caller wants the fix run to do.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FixRequest {
    /// Which checks to fix; `None` = all auto-fixable checks.
    pub checks: Option<Vec<String>>,
    /// "auto" = execute changes; "dry_run" = report without executing.
    pub mode: String,
    /// Include Medium-tier fixes (default false).
    #[serde(default)]
    pub include_medium: bool,
}

impl FixRequest {
    pub fn is_dry_run(&self) -> bool {
        self.mode == "dry_run"
    }
    pub fn max_tier(&self) -> FixTier {
        if self.include_medium {
            FixTier::Medium
        } else {
            FixTier::Low
        }
    }
}

// ==================== Raw data types used across checks ====================

/// Atom pair with high similarity (potential duplicate).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicatePair {
    pub pair_id: String,
    pub atom_a_id: String,
    pub atom_a_title: String,
    pub atom_a_source: Option<String>,
    pub atom_b_id: String,
    pub atom_b_title: String,
    pub atom_b_source: Option<String>,
    pub similarity: f32,
    /// Number of tags shared between the two atoms (higher = more likely related).
    pub shared_tag_count: i32,
    pub atom_a_created_at: Option<String>,
    pub atom_b_created_at: Option<String>,
}

/// Tag eligible for wiki that doesn't have one yet.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiGap {
    pub tag_id: String,
    pub tag_name: String,
    pub atom_count: i32,
}

/// Wiki that exists but is out of date.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiStaleEntry {
    pub tag_id: String,
    pub tag_name: String,
    pub new_atom_count: i32,
}

/// Atom preview for review sections that need title + date without full content.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AtomPreview {
    pub id: String,
    pub title: String,
    pub created_at: String,
}

/// Boilerplate-affected atom with clone count for prioritised review.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoilerplateAtomEntry {
    pub id: String,
    pub title: String,
    /// Number of semantic edges at similarity ≥0.99 from this atom.
    pub clone_count: i32,
}

/// Atom stub used inside contradiction pair entries.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContradictionAtom {
    pub id: String,
    pub title: String,
    pub source: Option<String>,
    pub created_at: Option<String>,
}

/// Pair of high-similarity atoms surfaced for manual contradiction review.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContradictionPairEntry {
    pub pair_id: String,
    pub atom_a: ContradictionAtom,
    pub atom_b: ContradictionAtom,
    /// Similarity score 0.0–1.0 (expected range 0.75–0.92 for contradictions).
    pub similarity: f32,
    pub shared_tag_count: i32,
}

/// Rootless tag entry for the tag-health review list.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RootlessTagEntry {
    pub id: String,
    pub name: String,
    pub atom_count: i32,
}

#[derive(Debug, Clone, Default)]
pub struct SingleAtomTagEntry {
    pub id: String,
    pub name: String,
    pub is_autotag: bool,
}

// ==================== Tag Proposal Types ====================

/// One proposed structural change to the tag tree.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TagProposalAction {
    Merge {
        from_id: String,
        into_id: String,
        from_name: String,
        into_name: String,
        reason: String,
    },
    Rename {
        tag_id: String,
        old_name: String,
        new_name: String,
        reason: String,
    },
    Reparent {
        tag_id: String,
        tag_name: String,
        new_parent_id: Option<String>,
        new_parent_name: Option<String>,
        reason: String,
    },
    Delete {
        tag_id: String,
        tag_name: String,
        reason: String,
    },
}

/// An LLM-generated proposal to reorganise the tag tree.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TagProposal {
    /// UUID used to apply the proposal later.
    pub id: String,
    /// One-paragraph LLM rationale.
    pub summary: String,
    pub actions: Vec<TagProposalAction>,
    /// RFC-3339 timestamp of generation.
    pub generated_at: String,
}

/// Per-DB health configuration.
///
/// Stored as JSON under the `health_config` setting key in each data DB.
/// Empty / missing → all defaults (informational checks score-excluded,
/// default-weighted checks use CHECK_WEIGHTS).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct HealthConfig {
    /// Per-check overrides. `enabled: false` suppresses the check entirely;
    /// `weight: Some(w)` contributes it to the overall score at that weight
    /// (sum of effective weights is renormalized).
    #[serde(default)]
    pub overrides: std::collections::HashMap<String, HealthCheckOverride>,

    /// Detection thresholds shared across the synchronous health checks.
    /// Missing / partial values fall back to `HealthThresholds::default()`.
    #[serde(default)]
    pub thresholds: HealthThresholds,
}

/// Tunable detection thresholds. Every field has a sane default baked in via
/// `Default` so a fresh DB works without any config. Serialised with
/// `#[serde(default)]` on each field so adding new thresholds is forward-
/// compatible — older configs deserialise into current defaults.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct HealthThresholds {
    // ---- boilerplate_pollution ----
    /// Edges at/above this similarity are treated as template clones. Default 0.99.
    #[serde(default = "default_boilerplate_similarity")]
    pub boilerplate_similarity: f32,
    /// Minimum clone-edge count before an atom is flagged. Default 2.
    #[serde(default = "default_boilerplate_min_clones")]
    pub boilerplate_min_clones: i32,

    // ---- contradiction_detection ----
    /// Lower bound (inclusive) of the contradiction similarity window. Default 0.80.
    #[serde(default = "default_contradiction_sim_min")]
    pub contradiction_similarity_min: f32,
    /// Upper bound (exclusive) of the contradiction similarity window. Default 0.92.
    #[serde(default = "default_contradiction_sim_max")]
    pub contradiction_similarity_max: f32,
    /// Minimum shared-tag count for a pair to surface. Default 1.
    #[serde(default = "default_contradiction_shared_tags")]
    pub contradiction_shared_tags_min: i32,
    /// Token-Jaccard upper bound for contradiction pairs. Pairs whose atom
    /// contents overlap at/above this fraction of unique tokens are treated
    /// as template/boilerplate clones and filtered out of the contradiction
    /// list — real contradictions express *different* claims and therefore
    /// different token sets. Default 0.70.
    #[serde(default = "default_contradiction_max_jaccard")]
    pub contradiction_max_content_jaccard: f32,

    // ---- content_overlap (cross-source near-duplicates) ----
    /// Lower bound (inclusive) of the cross-source overlap window. Default 0.55.
    #[serde(default = "default_overlap_sim_min")]
    pub content_overlap_similarity_min: f32,
    /// Upper bound (inclusive) of the cross-source overlap window. Default 0.85.
    #[serde(default = "default_overlap_sim_max")]
    pub content_overlap_similarity_max: f32,
    /// Minimum shared-tag count for a pair to surface. Default 2.
    #[serde(default = "default_overlap_shared_tags")]
    pub content_overlap_shared_tags_min: i32,

    // ---- content_quality ----
    /// Atoms shorter than this are flagged as `very_short`. Default 100.
    #[serde(default = "default_short_chars")]
    pub content_quality_short_chars: i32,
    /// Atoms longer than this are flagged as `very_long`. Default 15_000.
    #[serde(default = "default_long_chars")]
    pub content_quality_long_chars: i32,

    // ---- wiki_coverage ----
    /// Minimum atoms per tag for the tag to be "wiki-eligible". Default 5.
    #[serde(default = "default_wiki_min_atoms")]
    pub wiki_min_atoms_per_tag: i32,

    // ---- tag_health ----
    /// Max autotag single-atom tags before the check penalises. Default 3.
    #[serde(default = "default_single_atom_tag_threshold")]
    pub tag_health_single_atom_threshold: i32,

    // ---- semantic_graph_freshness ----
    /// Atoms added since last rebuild before score drops from warning to error. Default 20.
    #[serde(default = "default_graph_freshness_warning")]
    pub semantic_graph_freshness_warning: i32,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            boilerplate_similarity: default_boilerplate_similarity(),
            boilerplate_min_clones: default_boilerplate_min_clones(),
            contradiction_similarity_min: default_contradiction_sim_min(),
            contradiction_similarity_max: default_contradiction_sim_max(),
            contradiction_shared_tags_min: default_contradiction_shared_tags(),
            contradiction_max_content_jaccard: default_contradiction_max_jaccard(),
            content_overlap_similarity_min: default_overlap_sim_min(),
            content_overlap_similarity_max: default_overlap_sim_max(),
            content_overlap_shared_tags_min: default_overlap_shared_tags(),
            content_quality_short_chars: default_short_chars(),
            content_quality_long_chars: default_long_chars(),
            wiki_min_atoms_per_tag: default_wiki_min_atoms(),
            tag_health_single_atom_threshold: default_single_atom_tag_threshold(),
            semantic_graph_freshness_warning: default_graph_freshness_warning(),
        }
    }
}

fn default_boilerplate_similarity() -> f32 { 0.99 }
fn default_boilerplate_min_clones() -> i32 { 2 }
fn default_contradiction_sim_min() -> f32 { 0.80 }
fn default_contradiction_sim_max() -> f32 { 0.92 }
fn default_contradiction_shared_tags() -> i32 { 1 }
fn default_contradiction_max_jaccard() -> f32 { 0.70 }
fn default_overlap_sim_min() -> f32 { 0.55 }
fn default_overlap_sim_max() -> f32 { 0.85 }
fn default_overlap_shared_tags() -> i32 { 2 }
fn default_short_chars() -> i32 { 100 }
fn default_long_chars() -> i32 { 15_000 }
fn default_wiki_min_atoms() -> i32 { 5 }
fn default_single_atom_tag_threshold() -> i32 { 3 }
fn default_graph_freshness_warning() -> i32 { 20 }


impl HealthThresholds {
    /// Validate user-supplied thresholds. Returns a list of problems, empty on success.
    ///
    /// Rules are deliberately lenient — we only reject values that would cause the
    /// SQL / score math to misbehave (NaN, negative counts, similarities outside [0,1],
    /// inverted min/max windows). Tightening beyond that is an editorial choice, left
    /// to the UI.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();

        // ---- similarities must be finite and within [0.0, 1.0] ----
        let sims: [(&str, f32); 6] = [
            ("boilerplate_similarity", self.boilerplate_similarity),
            ("contradiction_similarity_min", self.contradiction_similarity_min),
            ("contradiction_similarity_max", self.contradiction_similarity_max),
            ("contradiction_max_content_jaccard", self.contradiction_max_content_jaccard),
            ("content_overlap_similarity_min", self.content_overlap_similarity_min),
            ("content_overlap_similarity_max", self.content_overlap_similarity_max),
        ];
        for (name, v) in sims {
            if !v.is_finite() {
                errs.push(format!("{name} must be a finite number"));
            } else if !(0.0..=1.0).contains(&v) {
                errs.push(format!("{name} must be in [0.0, 1.0] (got {v})"));
            }
        }

        // ---- min/max windows must not be inverted ----
        if self.contradiction_similarity_min >= self.contradiction_similarity_max {
            errs.push(format!(
                "contradiction_similarity_min ({}) must be < contradiction_similarity_max ({})",
                self.contradiction_similarity_min, self.contradiction_similarity_max,
            ));
        }
        if self.content_overlap_similarity_min > self.content_overlap_similarity_max {
            errs.push(format!(
                "content_overlap_similarity_min ({}) must be ≤ content_overlap_similarity_max ({})",
                self.content_overlap_similarity_min, self.content_overlap_similarity_max,
            ));
        }

        // ---- non-negative integer counts ----
        let non_neg: [(&str, i32); 7] = [
            ("boilerplate_min_clones", self.boilerplate_min_clones),
            ("contradiction_shared_tags_min", self.contradiction_shared_tags_min),
            ("content_overlap_shared_tags_min", self.content_overlap_shared_tags_min),
            ("content_quality_short_chars", self.content_quality_short_chars),
            ("content_quality_long_chars", self.content_quality_long_chars),
            ("tag_health_single_atom_threshold", self.tag_health_single_atom_threshold),
            ("semantic_graph_freshness_warning", self.semantic_graph_freshness_warning),
        ];
        for (name, v) in non_neg {
            if v < 0 {
                errs.push(format!("{name} must be ≥ 0 (got {v})"));
            }
        }

        // ---- wiki min_atoms must be ≥ 1 (0 would make every tag "wiki-eligible") ----
        if self.wiki_min_atoms_per_tag < 1 {
            errs.push(format!(
                "wiki_min_atoms_per_tag must be ≥ 1 (got {})",
                self.wiki_min_atoms_per_tag,
            ));
        }

        // ---- short_chars must be < long_chars (else every atom is both) ----
        if self.content_quality_short_chars >= self.content_quality_long_chars {
            errs.push(format!(
                "content_quality_short_chars ({}) must be < content_quality_long_chars ({})",
                self.content_quality_short_chars, self.content_quality_long_chars,
            ));
        }

        errs
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct HealthCheckOverride {
    /// When false, the check is not run and not displayed. Default: true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// When `Some`, use this weight in the overall score (overrides default
    /// and lifts informational checks into scoring if > 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

fn default_enabled() -> bool {
    true
}

impl Default for HealthCheckOverride {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: None,
        }
    }
}
