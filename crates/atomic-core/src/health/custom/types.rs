//! Types shared across the custom-check implementation.
//!
//! The public types (`CustomRule`, `DomainMatchMode`, `CustomCheck`,
//! `PreviewResult`) are re-exported from `custom::mod`. `RawOutcome` and
//! `FlaggedAtom` are evaluator plumbing and stay crate-private.

use serde::{Deserialize, Serialize};

/// Structural rule a custom check evaluates. One variant per supported
/// predicate shape — keeping this enum small is deliberate: every variant the
/// UI needs requires a Rust implementation, and that pressure keeps the
/// feature safe and predictable.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustomRule {
    /// Atoms tagged with any of `any_of` must also be tagged with every tag
    /// in `required`. Flags atoms that violate the invariant.
    TagRequires {
        any_of: Vec<String>,
        required: Vec<String>,
    },
    /// Atoms carrying any of the `tag_filter` ids (or all atoms when None)
    /// must have a non-empty `source_url`. Flags atoms missing a source.
    RequireSource {
        #[serde(default)]
        tag_filter: Option<String>,
    },
    /// Atoms whose content matches (or doesn't match, when `invert`) the
    /// given regex. Bounded regex size/DFA.
    ContentRegex {
        pattern: String,
        #[serde(default)]
        invert: bool,
    },
    /// Atoms (optionally scoped to `tag_filter`) must carry at least one of
    /// the `any_of` tags. Flags those that don't.
    RequireTag {
        any_of: Vec<String>,
        #[serde(default)]
        tag_filter: Option<String>,
    },
    /// Flags atoms whose word count is outside `[min_words, max_words]`.
    /// Either bound at 0 disables that side of the check.
    ContentLength {
        #[serde(default)]
        min_words: u32,
        #[serde(default)]
        max_words: u32,
        #[serde(default)]
        tag_filter: Option<String>,
    },
    /// Flags atoms whose citation count (markdown + wiki links) is below
    /// `min_citations`.
    CitationCount {
        min_citations: u32,
        #[serde(default)]
        tag_filter: Option<String>,
    },
    /// Flags atoms whose `source_url` host matches the `domains` list
    /// according to `mode`.
    SourceDomainMatches {
        domains: Vec<String>,
        #[serde(default)]
        mode: DomainMatchMode,
        #[serde(default)]
        tag_filter: Option<String>,
    },
    /// Flags atoms tagged with `tag` whose last update is older than
    /// `max_age_days`.
    StaleAtom {
        tag: String,
        max_age_days: u32,
    },
    /// Flags atoms that carry every tag in `all_of` at once. Used to enforce
    /// mutual-exclusion between tag sets (e.g. "draft" + "published").
    ForbiddenTagCombo {
        all_of: Vec<String>,
    },
    /// Flags atoms longer than `min_length_chars` whose content has no
    /// markdown heading.
    MissingHeading {
        #[serde(default = "default_min_heading_len")]
        min_length_chars: u32,
        #[serde(default)]
        tag_filter: Option<String>,
    },
    /// Flags atoms whose tag count is outside `[min, max]`. Either bound at
    /// 0 disables that side.
    TagCardinality {
        #[serde(default)]
        min: u32,
        #[serde(default)]
        max: u32,
        #[serde(default)]
        tag_filter: Option<String>,
    },
}

/// How `SourceDomainMatches` interprets the `domains` list.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DomainMatchMode {
    /// Flag atoms whose source_url domain is NOT in the list.
    #[default]
    Allowlist,
    /// Flag atoms whose source_url domain IS in the list.
    Blocklist,
}

fn default_min_heading_len() -> u32 {
    120
}

/// User-defined health check. `id` is a stable uuid so UI edits don't change
/// the score identity across saves.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomCheck {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 0 = informational (not scored). > 0 contributes at that weight,
    /// normalized alongside built-in checks.
    #[serde(default)]
    pub weight: f64,
    pub rule: CustomRule,
}

fn default_enabled() -> bool {
    true
}

/// Preview output for a single (unsaved) rule. Used by the UI to show
/// users "this would flag N atoms" as they tune rule parameters, before
/// persisting the rule.
#[derive(Serialize, Debug)]
pub struct PreviewResult {
    pub total_considered: i32,
    pub flagged_count: i32,
    /// First few flagged atoms (capped at `PREVIEW_SAMPLE`). Each entry
    /// has `id` and `title_preview`.
    pub sample: Vec<serde_json::Value>,
}

/// Raw per-rule evaluation output before we wrap it as a `HealthCheckResult`.
pub(super) struct RawOutcome {
    pub(super) total_considered: i32,
    pub(super) flagged_atoms: Vec<FlaggedAtom>,
}

#[derive(Serialize)]
pub(super) struct FlaggedAtom {
    pub(super) id: String,
    pub(super) title_preview: String,
}
