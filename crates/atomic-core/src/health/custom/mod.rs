//! User-defined custom health checks.
//!
//! Atomic ships with opinionated built-in checks, but different knowledge-base
//! workflows have different conventions. Custom checks let users declare rules
//! that reflect *their* conventions ("every atom tagged `paper` must have a
//! source URL", "flag atoms containing TODO markers", etc.) without shipping
//! arbitrary SQL or JavaScript from the UI.
//!
//! # Safety
//!
//! Rules are structured, not free-form: each [`CustomRule`] variant is a
//! hard-coded predicate the Rust evaluator applies. The UI only controls
//! parameters (tag ids, regex patterns) — never the query shape. This avoids
//! the SQL-injection and resource-exhaustion risks that come with arbitrary
//! user-defined SQL, while still covering the workflows users actually ask
//! for.
//!
//! # Storage
//!
//! The full list is persisted per-DB as JSON under the `custom_health_checks`
//! setting key (NOT in registry — see AGENTS.md § Multi-DB Gotchas). Each DB
//! has its own independent rule set.
//!
//! # Layout
//! - [`types`] — `CustomRule`, `DomainMatchMode`, `CustomCheck`, `PreviewResult`
//! - [`helpers`] — shared preview/word-count/URL-host/for-each-atom helpers
//! - [`rules`] — one `eval_*` fn per `CustomRule` variant

mod helpers;
mod rules;
pub mod types;

use super::HealthCheckResult;
use crate::error::AtomicCoreError;
use crate::storage::sqlite::SqliteStorage;
use helpers::PREVIEW_SAMPLE;
use serde_json::json;

pub use types::{CustomCheck, CustomRule, DomainMatchMode, PreviewResult};
use types::RawOutcome;

/// Pre-fixed key used to identify custom checks inside the `checks` map of a
/// `HealthReport`. Prevents collisions with built-in check names.
pub const CUSTOM_CHECK_PREFIX: &str = "custom.";

/// Compose the map key a custom check appears under in the report.
pub fn result_key(check_id: &str) -> String {
    format!("{CUSTOM_CHECK_PREFIX}{check_id}")
}

/// Evaluate all enabled custom checks against the database and return a
/// `(map_key, HealthCheckResult)` entry per check.
///
/// Runs on the caller's tokio runtime. Each rule executes one or two bounded
/// queries — no N+1, no full-table scans beyond what the built-in checks
/// already do.
pub fn run_all(
    storage: &SqliteStorage,
    checks: &[CustomCheck],
) -> Result<Vec<(String, HealthCheckResult, CustomCheck)>, AtomicCoreError> {
    let conn = storage
        .db
        .conn
        .lock()
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
    let mut out = Vec::with_capacity(checks.len());
    for check in checks.iter().filter(|c| c.enabled) {
        let result = evaluate(&conn, &check.rule)?;
        out.push((result_key(&check.id), finalize(check, result), check.clone()));
    }
    Ok(out)
}

/// Evaluate a single rule against the database WITHOUT persisting it or
/// touching the report. Fails when the rule itself is malformed (e.g.
/// invalid regex); the caller surfaces the error so the user can fix it.
pub fn preview_rule(
    storage: &SqliteStorage,
    rule: &CustomRule,
) -> Result<PreviewResult, AtomicCoreError> {
    let conn = storage
        .db
        .conn
        .lock()
        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
    let raw = evaluate(&conn, rule)?;
    let sample = raw
        .flagged_atoms
        .iter()
        .take(PREVIEW_SAMPLE)
        .map(|f| json!({ "id": f.id, "title_preview": f.title_preview }))
        .collect();
    Ok(PreviewResult {
        total_considered: raw.total_considered,
        flagged_count: raw.flagged_atoms.len() as i32,
        sample,
    })
}

/// Dispatch a rule variant to its evaluator.
fn evaluate(
    conn: &rusqlite::Connection,
    rule: &CustomRule,
) -> Result<RawOutcome, AtomicCoreError> {
    match rule {
        CustomRule::TagRequires { any_of, required } => {
            rules::eval_tag_requires(conn, any_of, required)
        }
        CustomRule::RequireSource { tag_filter } => {
            rules::eval_require_source(conn, tag_filter.as_deref())
        }
        CustomRule::ContentRegex { pattern, invert } => {
            rules::eval_content_regex(conn, pattern, *invert)
        }
        CustomRule::RequireTag { any_of, tag_filter } => {
            rules::eval_require_tag(conn, any_of, tag_filter.as_deref())
        }
        CustomRule::ContentLength { min_words, max_words, tag_filter } => {
            rules::eval_content_length(conn, *min_words, *max_words, tag_filter.as_deref())
        }
        CustomRule::CitationCount { min_citations, tag_filter } => {
            rules::eval_citation_count(conn, *min_citations, tag_filter.as_deref())
        }
        CustomRule::SourceDomainMatches { domains, mode, tag_filter } => {
            rules::eval_source_domain(conn, domains, *mode, tag_filter.as_deref())
        }
        CustomRule::StaleAtom { tag, max_age_days } => {
            rules::eval_stale_atom(conn, tag, *max_age_days)
        }
        CustomRule::ForbiddenTagCombo { all_of } => {
            rules::eval_forbidden_combo(conn, all_of)
        }
        CustomRule::MissingHeading { min_length_chars, tag_filter } => {
            rules::eval_missing_heading(conn, *min_length_chars, tag_filter.as_deref())
        }
        CustomRule::TagCardinality { min, max, tag_filter } => {
            rules::eval_tag_cardinality(conn, *min, *max, tag_filter.as_deref())
        }
    }
}

/// Wrap the raw predicate outcome as a `HealthCheckResult` honoring the check's
/// weight semantics (0 → informational, > 0 → contributes to overall score).
fn finalize(check: &CustomCheck, raw: RawOutcome) -> HealthCheckResult {
    let flagged = raw.flagged_atoms.len() as i32;
    let score = if raw.total_considered == 0 {
        100
    } else {
        let bad = flagged.min(raw.total_considered);
        let ratio = 1.0 - (bad as f64 / raw.total_considered as f64);
        (ratio * 100.0).round().clamp(0.0, 100.0) as u32
    };
    let status = if flagged == 0 {
        "ok"
    } else if score >= 80 {
        "warning"
    } else {
        "error"
    }
    .to_string();
    let informational = check.weight <= 0.0;

    HealthCheckResult {
        status,
        score,
        auto_fixable: false,
        requires_review: flagged > 0,
        informational,
        fix_action: None,
        data: json!({
            "custom": true,
            "label": check.label,
            "description": check.description,
            "rule": &check.rule,
            "total_considered": raw.total_considered,
            "flagged_count": flagged,
            "flagged": raw.flagged_atoms,
        }),
    }
}

#[cfg(test)]
mod tests;
