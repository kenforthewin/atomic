//! Phase-3 collapse: seed the default Daily Briefing report on every data
//! database and migrate historical briefings into finding atoms.
//!
//! `seed_default_briefing_report` and `migrate_briefings_to_findings` run at
//! `atomic-server` startup, before the HTTP listener binds. Both are
//! idempotent — `seed` is keyed on `dashboard.featured_report_id` pointing
//! at an extant report, `migrate` on a per-DB settings flag — so multiple
//! starts are safe.
//!
//! The phase-3 plan (`docs/plans/reports-phase-3-briefing-collapse.md`) gates
//! the briefing teardown on these two helpers running end-to-end. The legacy
//! briefings tables are dropped at the tail of the migration commit so a
//! crash mid-copy preserves source data.

use crate::error::AtomicCoreError;
use crate::models::{
    AtomKind, CitationPolicy, ContextScopeMode, CreateReportRequest, ReportFinding,
    ReportFindingCitation, SourceScopeWindow,
};
use crate::scheduler;
use crate::{AtomicCore, CreateAtomRequest};

const DEFAULT_REPORT_NAME: &str = "Daily Briefing";
const DEFAULT_REPORT_DESCRIPTION: &str =
    "Synthesizes recently captured atoms each morning into a short briefing.";

/// What we substitute for an empty `briefing_prompt` when seeding. Carries
/// the same intent as the legacy `briefing::SYSTEM_PROMPT` but phrased as
/// a research prompt (the "what to investigate"), since the reports runner
/// supplies its own agent-loop scaffold.
const DEFAULT_RESEARCH_PROMPT: &str = "Synthesize the source atoms — notes the user has captured since the last briefing — into a 2-3 paragraph briefing that highlights what's noteworthy, what themes emerge, and where these new notes connect to existing knowledge. Use [N] inline citation markers to point at specific source atoms. Skip atoms that aren't noteworthy. Write in the user's voice: concise, direct, mildly analytical, no filler.";

const REPORTS_PARENT_TAG: &str = "Reports";
const BRIEFINGS_TAG: &str = "Briefings";

use crate::FEATURED_REPORT_SETTING;

const MIGRATION_FLAG_SETTING: &str = "briefings.migrated_to_findings";

const LEGACY_FREQUENCY_KEY: &str = "task.daily_briefing.frequency";
const LEGACY_TIME_KEY: &str = "task.daily_briefing.time";
const LEGACY_WEEKDAY_KEY: &str = "task.daily_briefing.weekday";
const LEGACY_ENABLED_KEY: &str = "task.daily_briefing.enabled";
const LEGACY_PROMPT_KEY: &str = "briefing_prompt";

/// One row of the legacy `briefings` table, joined with its citations.
/// Returned by `LegacyBriefingsMigrationStore::fetch_legacy_briefings` in
/// `briefing.created_at ASC, citation.citation_index ASC` order so the
/// migration produces deterministic positions.
#[derive(Debug, Clone)]
pub struct LegacyBriefingRow {
    pub id: String,
    pub content: String,
    pub created_at: String,
    pub citations: Vec<LegacyBriefingCitation>,
}

#[derive(Debug, Clone)]
pub struct LegacyBriefingCitation {
    /// 1-indexed `[N]` position from the legacy `briefing_citations` table.
    pub citation_index: i32,
    pub atom_id: String,
    pub excerpt: String,
}

/// Idempotent default-report seed. Pulls the legacy briefing schedule and
/// prompt from the per-DB settings table, builds the equivalent reports row,
/// stamps `Reports/Briefings`, points the dashboard at it, and clears the
/// legacy prompt key so the report row is the new source of truth.
///
/// Idempotency: if `dashboard.featured_report_id` is set and the report it
/// references still exists, the function returns immediately. A stale
/// pointer (set to a deleted report) falls through and re-seeds.
pub async fn seed_default_briefing_report(core: &AtomicCore) -> Result<(), AtomicCoreError> {
    let storage = core.storage();
    let settings = storage.get_all_settings_sync().await?;

    if let Some(existing_id) = settings.get(FEATURED_REPORT_SETTING) {
        if !existing_id.is_empty() && core.get_report(existing_id).await?.is_some() {
            return Ok(());
        }
    }

    let research_prompt = settings
        .get(LEGACY_PROMPT_KEY)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RESEARCH_PROMPT.to_string());

    let (schedule, schedule_tz, enabled) = legacy_schedule_to_cron(&settings);

    let tag_id = ensure_reports_briefings_tag(core).await?;

    let req = CreateReportRequest {
        name: DEFAULT_REPORT_NAME.to_string(),
        description: Some(DEFAULT_REPORT_DESCRIPTION.to_string()),
        research_prompt,
        source_scope_tag_ids: Vec::new(),
        source_scope_window: Some(SourceScopeWindow::SinceLastRun),
        source_include_kinds: vec![AtomKind::Captured],
        context_scope_mode: ContextScopeMode::All,
        context_scope_tag_ids: Vec::new(),
        context_scope_window: None,
        context_include_kinds: vec![AtomKind::Captured],
        citation_policy: CitationPolicy::SourceOnly,
        max_source_atoms: None,
        max_source_tokens: None,
        max_tool_iterations: None,
        schedule,
        schedule_tz: Some(schedule_tz),
        enabled,
        output_atom_tags: vec![tag_id],
    };

    let report = core.create_report(req).await?;

    // Critique #1: carry the briefing's last-run timestamp onto the seeded
    // report so the first scheduled run after collapse picks up only the
    // atoms the briefing hadn't already covered. Without this, a busy DB
    // would re-brief weeks of already-briefed content.
    if let Some(last_run) = scheduler::state::get_last_run(core, "daily_briefing").await? {
        storage
            .update_report_cache_sync(&report.id, Some(&last_run.to_rfc3339()), None, None)
            .await?;
    }

    storage
        .set_setting_sync(FEATURED_REPORT_SETTING, &report.id)
        .await?;

    // The seeded report is the new source of truth for the prompt; clearing
    // the legacy key avoids drift across edits to the report row.
    storage.delete_setting_sync(LEGACY_PROMPT_KEY).await?;

    tracing::info!(
        report_id = %report.id,
        enabled = report.enabled,
        schedule = %report.schedule,
        "[reports/seed] Default Daily Briefing report seeded"
    );

    Ok(())
}

/// Idempotent historical-data migration. Streams legacy briefings into
/// finding atoms with `kind = 'report'`, provenance pointing at the seeded
/// Daily Briefing report, and citation rows preserving the original [N]
/// positions and excerpts. Drops the legacy tables once every row has
/// committed.
///
/// Gated by the per-DB `briefings.migrated_to_findings` settings flag.
/// Returns the number of finding atoms written on this call (0 on a
/// no-op re-run).
pub async fn migrate_briefings_to_findings(core: &AtomicCore) -> Result<usize, AtomicCoreError> {
    let storage = core.storage();
    let settings = storage.get_all_settings_sync().await?;
    if matches!(
        settings.get(MIGRATION_FLAG_SETTING).map(|s| s.as_str()),
        Some("true")
    ) {
        return Ok(0);
    }

    let Some(report_id) = settings
        .get(FEATURED_REPORT_SETTING)
        .filter(|s| !s.is_empty())
        .cloned()
    else {
        // No featured report (seed didn't run, or pointer was cleared).
        // We refuse to write findings without provenance — the next boot
        // will re-attempt after seed lands.
        tracing::warn!("[reports/seed] Skipping briefing migration: no featured report set");
        return Ok(0);
    };

    let Some(report) = core.get_report(&report_id).await? else {
        tracing::warn!(
            report_id = %report_id,
            "[reports/seed] Skipping briefing migration: featured report missing"
        );
        return Ok(0);
    };

    let rows = storage.fetch_legacy_briefings_sync().await?;
    let mut written = 0usize;
    let mut skipped = 0usize;

    for (index, row) in rows.iter().enumerate() {
        // Resumability: derive the finding atom's id from the legacy
        // briefing's id so a crash between any per-row commit and the
        // final flag flip is recoverable. On restart we re-process the
        // same legacy rows (the source tables haven't been dropped yet),
        // see that their target atom already exists, and skip — instead
        // of writing duplicate findings under fresh UUIDs.
        //
        // The `legacy-briefing-` prefix protects against the
        // (effectively zero) chance of collision with a user-authored
        // captured atom whose id happens to match a legacy briefing id.
        let atom_id = format!("legacy-briefing-{}", row.id);

        if storage.get_atom_impl(&atom_id).await?.is_some() {
            skipped += 1;
            continue;
        }

        let req = CreateAtomRequest {
            content: row.content.clone(),
            source_url: None,
            published_at: None,
            tag_ids: report.output_atom_tags.clone(),
            skip_if_source_exists: false,
        };
        let provenance = ReportFinding {
            finding_atom_id: atom_id.clone(),
            report_id: Some(report.id.clone()),
            run_id: None,
            report_name_snapshot: report.name.clone(),
            created_at: row.created_at.clone(),
        };
        let citations: Vec<ReportFindingCitation> = row
            .citations
            .iter()
            .map(|c| ReportFindingCitation {
                finding_atom_id: atom_id.clone(),
                cited_atom_id: c.atom_id.clone(),
                position: c.citation_index,
                excerpt: c.excerpt.clone(),
            })
            .collect();

        // `write_finding_transactionally` stamps `kind = 'report'`,
        // `tagging_status = 'skipped'`, inserts the provenance + citation
        // rows, all in one transaction. Per-row commit + the stable atom
        // id above give us resumability: re-running the loop after a
        // partial crash skips already-migrated rows via the existence
        // check above.
        storage
            .write_finding_transactionally_sync(
                &req,
                &atom_id,
                &row.created_at,
                &provenance,
                &citations,
            )
            .await?;
        written += 1;

        if (index + 1) % 100 == 0 {
            tracing::info!(
                migrated = written,
                resumed_skip = skipped,
                total = rows.len(),
                "[reports/seed] Briefing migration progress"
            );
        }
    }

    if skipped > 0 {
        tracing::info!(
            resumed_skip = skipped,
            "[reports/seed] Resumed briefing migration; skipped already-migrated rows"
        );
    }

    // Flag + DROP happen after every row has committed. Both are tiny
    // settings/DDL writes; the migration's risk surface is the per-row
    // copy above, not this tail.
    storage
        .set_setting_sync(MIGRATION_FLAG_SETTING, "true")
        .await?;
    storage.drop_legacy_briefing_tables_sync().await?;

    tracing::info!(
        migrated = written,
        report_id = %report.id,
        "[reports/seed] Briefing migration complete"
    );
    Ok(written)
}

/// Map the legacy `task.daily_briefing.*` settings into a (cron, tz, enabled)
/// triple suitable for a `reports` row. The cron format matches what the
/// `cron` crate the reports runner uses accepts: 6 fields, Sun = 0.
///
/// Critique #10: `Off` preserves the user's saved time so a later
/// re-enable resumes their schedule. Only `enabled` is gated.
fn legacy_schedule_to_cron(
    settings: &std::collections::HashMap<String, String>,
) -> (String, String, bool) {
    let frequency = settings
        .get(LEGACY_FREQUENCY_KEY)
        .map(|s| s.as_str())
        .unwrap_or_else(|| {
            // Legacy enabled/disabled with no frequency key falls back to
            // `daily` when on, `off` when off, mirroring the pre-collapse
            // briefing-status read.
            match settings.get(LEGACY_ENABLED_KEY).map(|s| s.as_str()) {
                Some("false") | Some("0") | Some("off") => "off",
                _ => "daily",
            }
        });

    let (hour, minute) = settings
        .get(LEGACY_TIME_KEY)
        .and_then(|raw| parse_hh_mm(raw))
        .unwrap_or((9, 0));

    let weekday_dow = settings
        .get(LEGACY_WEEKDAY_KEY)
        .and_then(|raw| weekday_to_dow(raw))
        .unwrap_or(1); // Monday — matches BriefingSchedule's normalize default

    let cron = match frequency {
        "weekly" => format!("0 {minute} {hour} * * {weekday_dow}"),
        // `off` keeps the user's preferred time in the cron so re-enabling
        // resumes their schedule. `daily` falls into the same branch.
        _ => format!("0 {minute} {hour} * * *"),
    };

    let enabled = frequency != "off"
        && !matches!(
            settings.get(LEGACY_ENABLED_KEY).map(|s| s.as_str()),
            Some("false") | Some("0") | Some("off")
        );

    let tz = settings
        .get("timezone")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string()));

    (cron, tz, enabled)
}

/// Idempotently ensure `Reports` (top-level) and `Reports/Briefings` (child)
/// exist, and return the child tag's id. Uses `create_tag` so the system
/// can authorize a new top-level category — `get_or_create_tag` refuses to
/// (that path is LLM-guard rail for the auto-tagger and must keep saying no
/// to new categories invented from agent prose).
async fn ensure_reports_briefings_tag(core: &AtomicCore) -> Result<String, AtomicCoreError> {
    let all_tags = core.get_all_tags().await?;
    let parent_id = match all_tags
        .iter()
        .find(|t| t.tag.parent_id.is_none() && t.tag.name == REPORTS_PARENT_TAG)
    {
        Some(t) => t.tag.id.clone(),
        None => core.create_tag(REPORTS_PARENT_TAG, None).await?.id,
    };
    if let Some(existing) = all_tags
        .iter()
        .find(|t| t.tag.parent_id.as_deref() == Some(&parent_id) && t.tag.name == BRIEFINGS_TAG)
    {
        return Ok(existing.tag.id.clone());
    }
    let child = core.create_tag(BRIEFINGS_TAG, Some(&parent_id)).await?;
    Ok(child.id)
}

fn parse_hh_mm(raw: &str) -> Option<(u32, u32)> {
    let mut parts = raw.split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    if h > 23 || m > 59 || parts.next().is_some() {
        return None;
    }
    Some((h, m))
}

/// Map the legacy `BriefingWeekday` snake_case strings into the cron crate's
/// DoW range (Sun = 0 .. Sat = 6).
fn weekday_to_dow(raw: &str) -> Option<u32> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "sunday" => Some(0),
        "monday" => Some(1),
        "tuesday" => Some(2),
        "wednesday" => Some(3),
        "thursday" => Some(4),
        "friday" => Some(5),
        "saturday" => Some(6),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn s(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn daily_07_00_round_trips_to_cron() {
        let settings: HashMap<String, String> = [
            s(LEGACY_FREQUENCY_KEY, "daily"),
            s(LEGACY_TIME_KEY, "07:00"),
        ]
        .into_iter()
        .collect();
        let (cron, _, enabled) = legacy_schedule_to_cron(&settings);
        assert_eq!(cron, "0 0 7 * * *");
        assert!(enabled);
    }

    #[test]
    fn weekly_monday_09_30_round_trips_to_cron() {
        let settings: HashMap<String, String> = [
            s(LEGACY_FREQUENCY_KEY, "weekly"),
            s(LEGACY_TIME_KEY, "09:30"),
            s(LEGACY_WEEKDAY_KEY, "monday"),
        ]
        .into_iter()
        .collect();
        let (cron, _, enabled) = legacy_schedule_to_cron(&settings);
        assert_eq!(cron, "0 30 9 * * 1");
        assert!(enabled);
    }

    #[test]
    fn off_disables_and_preserves_user_time() {
        let settings: HashMap<String, String> =
            [s(LEGACY_FREQUENCY_KEY, "off"), s(LEGACY_TIME_KEY, "14:30")]
                .into_iter()
                .collect();
        let (cron, _, enabled) = legacy_schedule_to_cron(&settings);
        // Re-enabling later should resume at 14:30, not jump to a default.
        assert_eq!(cron, "0 30 14 * * *");
        assert!(!enabled);
    }

    #[test]
    fn legacy_enabled_false_yields_disabled_with_default_daily_cron() {
        // A user who never touched the new frequency key but disabled the
        // briefing via the legacy boolean: we honor the disabled bit and
        // shape the cron as daily at the default 09:00 (or saved time).
        let settings: HashMap<String, String> =
            [s(LEGACY_ENABLED_KEY, "false")].into_iter().collect();
        let (cron, _, enabled) = legacy_schedule_to_cron(&settings);
        assert_eq!(cron, "0 0 9 * * *");
        assert!(!enabled);
    }

    #[test]
    fn parse_hh_mm_rejects_garbage() {
        assert_eq!(parse_hh_mm("07:00"), Some((7, 0)));
        assert_eq!(parse_hh_mm("23:59"), Some((23, 59)));
        assert_eq!(parse_hh_mm("24:00"), None);
        assert_eq!(parse_hh_mm("07:60"), None);
        assert_eq!(parse_hh_mm("07"), None);
        assert_eq!(parse_hh_mm("07:00:00"), None);
        assert_eq!(parse_hh_mm("morning"), None);
    }

    #[test]
    fn weekday_mapping_covers_full_week() {
        assert_eq!(weekday_to_dow("Sunday"), Some(0));
        assert_eq!(weekday_to_dow("monday"), Some(1));
        assert_eq!(weekday_to_dow(" SATURDAY "), Some(6));
        assert_eq!(weekday_to_dow("dimanche"), None);
    }
}
