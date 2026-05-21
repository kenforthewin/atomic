//! Cron-based report scheduling.
//!
//! Reports store a cron expression plus an optional IANA timezone string.
//! This module evaluates "is this report due right now?" against
//! `report.last_run_at` and computes the next fire time after a given
//! instant for UI / debug output.
//!
//! `cron`'s `Schedule` type emits successive fire times in chrono-zoned
//! local time; we accept a `chrono_tz::Tz` and convert in/out of UTC at
//! the boundary so everything else in the crate stays UTC-only.

use crate::error::AtomicCoreError;
use crate::models::Report;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use std::str::FromStr;

/// Default zone when a report omits `schedule_tz`. UTC keeps the
/// next-fire calculation independent of the host system's clock setting.
const DEFAULT_TZ: Tz = chrono_tz::UTC;

/// Parse the report's `schedule` + `schedule_tz` fields, returning typed
/// values the caller can pass to [`next_after`] and [`is_due`].
pub fn parse(report: &Report) -> Result<(Schedule, Tz), AtomicCoreError> {
    let schedule = Schedule::from_str(&report.schedule)
        .map_err(|e| AtomicCoreError::DatabaseOperation(format!("invalid cron expression: {e}")))?;
    let tz = match &report.schedule_tz {
        Some(name) => name.parse::<Tz>().map_err(|e| {
            AtomicCoreError::DatabaseOperation(format!("invalid timezone '{name}': {e}"))
        })?,
        None => DEFAULT_TZ,
    };
    Ok((schedule, tz))
}

/// Compute the next scheduled fire time strictly greater than `after`.
/// Returns `None` if the schedule has no future fire times within the
/// `cron` crate's evaluation horizon (effectively never for normal cron
/// expressions).
pub fn next_after(schedule: &Schedule, tz: Tz, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let local_after = after.with_timezone(&tz);
    schedule
        .after(&local_after)
        .next()
        .map(|t| t.with_timezone(&Utc))
}

/// `true` iff the report should run at `now`: either it has never run
/// (no `last_run_at`) or the most recent scheduled fire time at or
/// before `now` falls strictly after the last successful run.
///
/// Errors propagate from cron / timezone parsing — invalid schedules
/// never report as due, so a malformed report cannot wedge the scheduler.
pub fn is_due(report: &Report, now: DateTime<Utc>) -> bool {
    match is_due_inner(report, now) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                report_id = %report.id,
                error = %e,
                "[reports/schedule] invalid schedule; treating as not-due"
            );
            false
        }
    }
}

fn is_due_inner(report: &Report, now: DateTime<Utc>) -> Result<bool, AtomicCoreError> {
    let (schedule, tz) = parse(report)?;
    let last = match &report.last_run_at {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|e| AtomicCoreError::DatabaseOperation(format!("invalid last_run_at: {e}")))?,
        None => {
            // Never run — anchor on the report's `created_at`. Using
            // epoch here was wrong: `next_after(epoch, daily-at-07:00)`
            // returns 1970-01-01T07:00:00, which is `<= now` for any
            // newly-created report, so the scheduler would fire
            // immediately on creation even if today's 07:00 hasn't
            // arrived yet. Anchoring on `created_at` means the first
            // run happens at the next scheduled fire *after creation*.
            let anchor = DateTime::parse_from_rfc3339(&report.created_at)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| {
                    AtomicCoreError::DatabaseOperation(format!(
                        "invalid report.created_at '{}': {e}",
                        report.created_at
                    ))
                })?;
            return Ok(next_after(&schedule, tz, anchor)
                .map(|first| first <= now)
                .unwrap_or(false));
        }
    };
    // "Due" means: the next fire after `last_run_at` has already happened
    // by `now`. This handles missed ticks gracefully — a scheduler that
    // sleeps through several fire windows still fires once on resume.
    Ok(next_after(&schedule, tz, last)
        .map(|next| next <= now)
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CitationPolicy, ContextScopeMode, Report, SourceScopeWindow};
    use chrono::TimeZone;

    fn make_report(schedule: &str, tz: Option<&str>, last_run: Option<&str>) -> Report {
        Report {
            id: "test".into(),
            name: "T".into(),
            description: None,
            research_prompt: "".into(),
            source_scope_tag_ids: vec![],
            source_scope_window: Some(SourceScopeWindow::SinceLastRun),
            source_include_kinds: vec![crate::models::AtomKind::Captured],
            context_scope_mode: ContextScopeMode::All,
            context_scope_tag_ids: vec![],
            context_scope_window: None,
            context_include_kinds: vec![crate::models::AtomKind::Captured],
            citation_policy: CitationPolicy::SourceOnly,
            max_source_atoms: None,
            max_source_tokens: None,
            max_tool_iterations: None,
            schedule: schedule.into(),
            schedule_tz: tz.map(String::from),
            enabled: true,
            output_atom_tags: vec![],
            last_run_at: last_run.map(String::from),
            last_finding_atom_id: None,
            last_error: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn make_report_with_created_at(
        schedule: &str,
        tz: Option<&str>,
        last_run: Option<&str>,
        created_at: &str,
    ) -> Report {
        Report {
            created_at: created_at.into(),
            ..make_report(schedule, tz, last_run)
        }
    }

    #[test]
    fn first_run_is_due_when_schedule_has_already_fired_since_creation() {
        // Every-minute schedule, report created two minutes ago — due.
        let r = make_report_with_created_at("0 * * * * *", None, None, "2026-05-20T12:00:00Z");
        let now = Utc.with_ymd_and_hms(2026, 5, 20, 12, 5, 30).unwrap();
        assert!(is_due(&r, now));
    }

    #[test]
    fn first_run_not_due_when_period_not_yet_reached() {
        // Report created at 14:00 with `daily at 07:00` schedule must
        // NOT fire at 14:30 the same day — the next fire after creation
        // is tomorrow 07:00. Before the created_at anchor fix this
        // returned true because next_after(epoch, ...) yields a 1970
        // timestamp that's trivially before now.
        let r = make_report_with_created_at("0 0 7 * * *", None, None, "2026-05-20T14:00:00Z");
        let now = Utc.with_ymd_and_hms(2026, 5, 20, 14, 30, 0).unwrap();
        assert!(
            !is_due(&r, now),
            "daily 07:00 should not fire at 14:30 same day"
        );
    }

    #[test]
    fn first_run_due_at_next_scheduled_fire_after_creation() {
        // Same report; check `now = next-day 07:30` → fired this morning.
        let r = make_report_with_created_at("0 0 7 * * *", None, None, "2026-05-20T14:00:00Z");
        let now = Utc.with_ymd_and_hms(2026, 5, 21, 7, 30, 0).unwrap();
        assert!(is_due(&r, now));
    }

    #[test]
    fn not_due_within_same_minute_as_last_run() {
        let r = make_report("0 * * * * *", None, Some("2026-05-20T12:00:00Z"));
        let now = Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 30).unwrap();
        assert!(!is_due(&r, now));
    }

    #[test]
    fn due_after_next_fire_minute() {
        let r = make_report("0 * * * * *", None, Some("2026-05-20T12:00:00Z"));
        let now = Utc.with_ymd_and_hms(2026, 5, 20, 12, 1, 30).unwrap();
        assert!(is_due(&r, now));
    }

    #[test]
    fn timezone_shifts_fire_time() {
        // 07:00 daily in America/New_York during EST (winter) = 12:00 UTC.
        let r = make_report(
            "0 0 7 * * *",
            Some("America/New_York"),
            Some("2026-01-15T06:00:00Z"),
        );
        // 11:30 UTC = 06:30 EST — before the 07:00 EST fire, not due.
        let before = Utc.with_ymd_and_hms(2026, 1, 15, 11, 30, 0).unwrap();
        assert!(
            !is_due(&r, before),
            "07:00 EST hasn't fired yet at 11:30 UTC"
        );
        // 12:30 UTC = 07:30 EST — past the 07:00 EST fire, due.
        let after = Utc.with_ymd_and_hms(2026, 1, 15, 12, 30, 0).unwrap();
        assert!(is_due(&r, after), "07:00 EST has fired by 12:30 UTC");
    }

    #[test]
    fn invalid_cron_is_not_due_and_does_not_panic() {
        let r = make_report("not a cron", None, None);
        let now = Utc::now();
        assert!(!is_due(&r, now));
    }
}
