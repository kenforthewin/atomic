//! Source- and context-scope resolution for report runs.
//!
//! Pure functions of `(report, now, last_run)`. Returns the atoms the
//! agent will see as its source batch, and a `ContextFilter` value the
//! agent's `semantic_search` tool is parameterized by. No state lives
//! here — the runner owns the call, this module just translates a
//! `Report` row into concrete filters.
//!
//! The "since_last_run" / ISO-8601 duration / null window encoding is
//! the storage-boundary shape; this module is where it gets resolved into
//! an RFC3339 cutoff timestamp the storage layer can compare against.

use crate::error::AtomicCoreError;
use crate::models::{
    AtomKind, AtomWithTags, ContextScopeMode, ContextScopeWindow, KindFilter, Report,
    SourceScopeWindow,
};
use crate::AtomicCore;
use chrono::{DateTime, Duration as ChronoDuration, Utc};

/// Frozen, run-scoped view of the report's context-side configuration.
///
/// Computed at run start from the report row and the resolved source
/// scope. Passed into the agent's `semantic_search` tool so every search
/// inside the loop applies the same filter; the agent itself never
/// decides scope.
///
/// `excluded_atom_ids` is the union of source-atom ids (so a search
/// doesn't echo the source batch back) and prior-finding ids from this
/// same report (so the report doesn't search its own output).
#[derive(Debug, Clone)]
pub struct ContextFilter {
    /// Tag ids to scope the search to. Empty = no tag filter (modulo
    /// `SameAsSource`, which is materialized as the source's tag ids).
    pub tag_ids: Vec<String>,
    /// Optional `created_at < before` cutoff for `OlderThanSource`, or
    /// `> after` for ISO-8601 durations.
    pub time_window: Option<TimeWindow>,
    pub kinds: KindFilter,
    pub excluded_atom_ids: Vec<String>,
}

/// One-side cutoff for the context corpus.
#[derive(Debug, Clone)]
pub enum TimeWindow {
    /// `atoms.created_at < before_rfc3339` — used by OlderThanSource.
    Before(String),
    /// `atoms.created_at > after_rfc3339` — used by ISO-8601 durations.
    After(String),
}

/// Result of [`resolve_source`]. Empty `atoms` means the run should be a
/// terminal-success no-op (no finding atom, no LLM call); `total_in_scope`
/// is the pre-cap count so the agent prompt can mention truncation.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    pub atoms: Vec<AtomWithTags>,
    pub total_in_scope: i32,
    /// RFC3339 cutoff actually used for the window predicate. Re-used
    /// downstream when building `OlderThanSource` for the context filter.
    pub since_cutoff: Option<String>,
}

/// Resolve the source-scope atom set for `report` at `now`.
///
/// Window evaluation:
/// - `SourceScopeWindow::SinceLastRun`: cutoff = `report.last_run_at`, or
///   epoch (treated as "the very beginning") on first run.
/// - `SourceScopeWindow::Duration(iso)`: cutoff = `now - parse(iso)`.
/// - `None`: no time bound.
pub async fn resolve_source(
    core: &AtomicCore,
    report: &Report,
    now: DateTime<Utc>,
) -> Result<ResolvedSource, AtomicCoreError> {
    let since_cutoff = resolve_source_since(report, now)?;
    let kinds = if report.source_include_kinds.is_empty() {
        // Empty list defensively means "match nothing", not "match all" —
        // same convention as the storage `KindFilter::Only(vec![])` path.
        KindFilter::Only(Vec::new())
    } else {
        KindFilter::Only(report.source_include_kinds.clone())
    };
    let total_in_scope = core
        .storage()
        .count_atoms_for_report_scope_sync(
            &report.source_scope_tag_ids,
            since_cutoff.as_deref(),
            &kinds,
        )
        .await?;
    let atoms = core
        .storage()
        .list_atoms_for_report_scope_sync(
            &report.source_scope_tag_ids,
            since_cutoff.as_deref(),
            &kinds,
            report.max_source_atoms,
        )
        .await?;
    // Apply `max_source_tokens` after `max_source_atoms` so the token
    // budget is the final guardrail before the prompt is built.
    // Truncates the tail in created_at-DESC order — newer atoms keep
    // their priority, older ones drop out under heavy load.
    let atoms = truncate_to_token_budget(atoms, report.max_source_tokens);
    Ok(ResolvedSource {
        atoms,
        total_in_scope,
        since_cutoff,
    })
}

/// Trim `atoms` so cumulative `count_tokens(content)` does not exceed
/// `budget`. `None` budget is a no-op. The first atom is always kept
/// even if it exceeds the budget on its own — otherwise an over-budget
/// configuration would silently produce an empty scope.
fn truncate_to_token_budget(atoms: Vec<AtomWithTags>, budget: Option<i32>) -> Vec<AtomWithTags> {
    let Some(budget) = budget.filter(|b| *b > 0) else {
        return atoms;
    };
    let budget = budget as usize;
    let mut out = Vec::with_capacity(atoms.len());
    let mut used: usize = 0;
    for a in atoms {
        let tokens = crate::chunking::count_tokens(&a.atom.content);
        // Always keep at least one source atom so a tiny budget can't
        // produce an empty-scope run that the empty-scope short-circuit
        // would terminate as success.
        if out.is_empty() {
            used += tokens;
            out.push(a);
            continue;
        }
        if used + tokens > budget {
            break;
        }
        used += tokens;
        out.push(a);
    }
    out
}

/// Translate `source_scope_window` into an RFC3339 cutoff timestamp.
pub fn resolve_source_since(
    report: &Report,
    now: DateTime<Utc>,
) -> Result<Option<String>, AtomicCoreError> {
    match &report.source_scope_window {
        None => Ok(None),
        Some(SourceScopeWindow::SinceLastRun) => {
            // First run: "since_last_run" without a last_run_at means the
            // very first run sees every in-scope atom. Treated as epoch
            // so the predicate `created_at > cutoff` matches everything.
            Ok(Some(
                report
                    .last_run_at
                    .clone()
                    .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
            ))
        }
        Some(SourceScopeWindow::Duration(iso)) => {
            let dur = parse_iso8601_duration(iso)?;
            let cutoff = now - dur;
            Ok(Some(cutoff.to_rfc3339()))
        }
    }
}

/// Build the [`ContextFilter`] the agent's `semantic_search` tool will
/// use. Depends on the resolved source (for `SameAsSource` mode and for
/// excluding source ids), the report's prior findings (for self-exclusion),
/// and `now` (for ISO-8601 duration windows).
pub async fn build_context_filter(
    core: &AtomicCore,
    report: &Report,
    source: &ResolvedSource,
    now: DateTime<Utc>,
) -> Result<ContextFilter, AtomicCoreError> {
    let tag_ids = match report.context_scope_mode {
        ContextScopeMode::SameAsSource => report.source_scope_tag_ids.clone(),
        ContextScopeMode::All => Vec::new(),
        ContextScopeMode::Explicit => report.context_scope_tag_ids.clone(),
    };

    let time_window = match &report.context_scope_window {
        None => None,
        Some(ContextScopeWindow::OlderThanSource) => source
            .since_cutoff
            .as_ref()
            .map(|c| TimeWindow::Before(c.clone())),
        Some(ContextScopeWindow::Duration(iso)) => {
            let dur = parse_iso8601_duration(iso)?;
            Some(TimeWindow::After((now - dur).to_rfc3339()))
        }
    };

    let kinds = if report.context_include_kinds.is_empty() {
        KindFilter::Only(Vec::new())
    } else {
        KindFilter::Only(report.context_include_kinds.clone())
    };

    let mut excluded_atom_ids: Vec<String> =
        source.atoms.iter().map(|a| a.atom.id.clone()).collect();
    let prior_findings = core
        .storage()
        .list_finding_atom_ids_for_report_sync(&report.id)
        .await?;
    excluded_atom_ids.extend(prior_findings);

    Ok(ContextFilter {
        tag_ids,
        time_window,
        kinds,
        excluded_atom_ids,
    })
}

/// Parse the ISO-8601 duration subset relevant for reports:
/// - `P{n}D` — days
/// - `P{n}W` — weeks
/// - `PT{n}H` — hours
/// - `PT{n}M` — minutes
/// - `PT{n}S` — seconds
///
/// Year/month durations are intentionally NOT supported because they're
/// calendar-dependent and the predicate is a single timestamp comparison.
/// Authoring tools should expose "X days" and convert.
pub fn parse_iso8601_duration(s: &str) -> Result<ChronoDuration, AtomicCoreError> {
    let err = || AtomicCoreError::DatabaseOperation(format!("invalid ISO-8601 duration: {s}"));
    let rest = s.strip_prefix('P').ok_or_else(err)?;
    let (date_part, time_part) = match rest.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (rest, None),
    };

    let mut total = ChronoDuration::zero();
    if !date_part.is_empty() {
        let (n, unit) = split_unit(date_part).ok_or_else(err)?;
        match unit {
            'D' => total += ChronoDuration::days(n),
            'W' => total += ChronoDuration::weeks(n),
            _ => return Err(err()),
        }
    }
    if let Some(tp) = time_part {
        if tp.is_empty() {
            return Err(err());
        }
        let (n, unit) = split_unit(tp).ok_or_else(err)?;
        match unit {
            'H' => total += ChronoDuration::hours(n),
            'M' => total += ChronoDuration::minutes(n),
            'S' => total += ChronoDuration::seconds(n),
            _ => return Err(err()),
        }
    }
    if total.is_zero() {
        return Err(err());
    }
    Ok(total)
}

fn split_unit(s: &str) -> Option<(i64, char)> {
    let unit_char = s.chars().last()?;
    if !unit_char.is_ascii_alphabetic() {
        return None;
    }
    let num: i64 = s[..s.len() - unit_char.len_utf8()].parse().ok()?;
    Some((num, unit_char))
}

/// Default for callers that want "everything captured" without
/// constructing the enum. Convenience for tests and one-shot CLI paths.
#[allow(dead_code)]
pub fn captured_only() -> Vec<AtomKind> {
    vec![AtomKind::Captured]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_days() {
        let d = parse_iso8601_duration("P7D").unwrap();
        assert_eq!(d, ChronoDuration::days(7));
    }

    #[test]
    fn parse_weeks() {
        let d = parse_iso8601_duration("P2W").unwrap();
        assert_eq!(d, ChronoDuration::weeks(2));
    }

    #[test]
    fn parse_hours() {
        let d = parse_iso8601_duration("PT24H").unwrap();
        assert_eq!(d, ChronoDuration::hours(24));
    }

    #[test]
    fn parse_minutes() {
        let d = parse_iso8601_duration("PT30M").unwrap();
        assert_eq!(d, ChronoDuration::minutes(30));
    }

    #[test]
    fn parse_rejects_year_and_month() {
        assert!(parse_iso8601_duration("P1Y").is_err());
        assert!(parse_iso8601_duration("P1M").is_err());
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_iso8601_duration("P").is_err());
        assert!(parse_iso8601_duration("PT").is_err());
        assert!(parse_iso8601_duration("").is_err());
    }

    fn mock_atom(content: &str) -> AtomWithTags {
        use crate::models::{Atom, AtomKind};
        AtomWithTags {
            atom: Atom {
                id: format!("a-{}", uuid::Uuid::new_v4()),
                content: content.to_string(),
                title: "t".to_string(),
                snippet: content.chars().take(80).collect(),
                source_url: None,
                source: None,
                published_at: None,
                created_at: "2026-05-20T00:00:00Z".to_string(),
                updated_at: "2026-05-20T00:00:00Z".to_string(),
                embedding_status: "complete".to_string(),
                tagging_status: "complete".to_string(),
                embedding_error: None,
                tagging_error: None,
                kind: AtomKind::Captured,
            },
            tags: vec![],
        }
    }

    #[test]
    fn truncate_to_token_budget_passes_through_when_unset() {
        let atoms = vec![mock_atom("hello"), mock_atom("world")];
        let out = truncate_to_token_budget(atoms.clone(), None);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn truncate_to_token_budget_stops_when_budget_exhausted() {
        // Each "lorem ipsum" line is a couple tokens; a budget of 5
        // should keep only the first one or two.
        let atoms: Vec<_> = (0..10)
            .map(|i| mock_atom(&format!("lorem ipsum dolor sit amet {i}")))
            .collect();
        let out = truncate_to_token_budget(atoms, Some(5));
        assert!(out.len() < 10, "budget should truncate, kept {}", out.len());
        let used: usize = out
            .iter()
            .map(|a| crate::chunking::count_tokens(&a.atom.content))
            .sum();
        // We let one over-budget atom land first to avoid an empty scope,
        // but no further over-budget additions should occur.
        if out.len() > 1 {
            assert!(used <= 5 + crate::chunking::count_tokens(&out[0].atom.content));
        }
    }

    #[test]
    fn truncate_to_token_budget_always_keeps_at_least_one_atom() {
        // Tiny budget + giant first atom: still returns one atom so the
        // empty-scope short-circuit doesn't misfire as terminal success.
        let big = "word ".repeat(1000);
        let atoms = vec![mock_atom(&big), mock_atom("small")];
        let out = truncate_to_token_budget(atoms, Some(1));
        assert_eq!(out.len(), 1);
    }
}
