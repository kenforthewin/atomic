//! Knowledge-base health monitoring and auto-remediation.
//!
//! This module computes a scored health report across 10 checks, each targeting
//! a distinct class of data-quality issue. Deterministic fixes (orphan-tag
//! deletion, retry pipelines, graph rebuild) run automatically at "safe" or
//! "low" tier.  LLM-powered fixes (merge duplicates, enrich stubs, structure
//! content) are available but always logged and undoable.
//!
//! # Flow
//! 1. `compute_health(core)` → runs all checks → returns `HealthReport`
//! 2. `run_fix(core, req)` → applies fixes by tier → returns `FixResponse`
//! 3. `undo_fix(core, fix_id)` → restores pre-fix state from audit log
//!
//! # Check weights (must sum to 1.0)
//! | Check                     | Weight |
//! |---------------------------|--------|
//! | duplicate_detection       | 15 %   |
//! | embedding_coverage        | 15 %   |
//! | tagging_coverage          | 20 %   |
//! | source_uniqueness         | 10 %   |
//! | wiki_coverage             | 10 %   |
//! | semantic_graph_freshness  | 10 %   |
//! | content_quality           |  5 %   |
//! | orphan_tags               |  5 %   |
//! | tag_health                |  5 %   |
//! | contradiction_detection   |  5 %   |

pub mod audit;
pub mod checks;
pub mod custom;
pub mod fixes;
pub mod gc_task;
pub mod link_resolution;
pub mod llm_fixes;
pub mod score;
pub mod task;
pub mod types;

use crate::error::AtomicCoreError;
use crate::AtomicCore;
use std::collections::HashMap;

// Re-export the public surface so existing callers (`use crate::health::HealthReport`)
// keep working.
pub use score::aggregate_score;
pub use types::{
    AtomPreview, BoilerplateAtomEntry, ContradictionAtom, ContradictionPairEntry,
    DuplicatePair, FixAction, FixRequest, FixResponse, FixTier, HealthCheckOverride,
    HealthCheckResult, HealthConfig, HealthReport, HealthStatus, RootlessTagEntry,
    SingleAtomTagEntry, SkippedFix, TagProposal, TagProposalAction, WikiGap,
    WikiStaleEntry,
};

/// Run all health checks and return a complete `HealthReport`.
///
/// Completes in < 2s for databases with up to ~1,000 atoms.  Contradiction
/// detection is a stub (no LLM call) so it won't time out on large graphs.
pub async fn compute_health(core: &AtomicCore) -> Result<HealthReport, AtomicCoreError> {
    let computed_at = chrono::Utc::now().to_rfc3339();

    // Load per-DB health config (fall back to defaults on error — health
    // should never fail because of a corrupt config).
    let config = core.get_health_config().await.unwrap_or_default();

    // Fetch all raw data in a single spawn_blocking pass
    let raw = core.storage().health_check_data_sync().await?;

    // Run all synchronous checks
    let mut checks = checks::run_all(&raw);

    // Run async link-resolution check (needs DB lookups per candidate atom)
    match compute_link_check(core).await {
        Ok(link_check) => {
            checks.insert("broken_internal_links".to_string(), link_check);
        }
        Err(e) => {
            tracing::warn!(error = %e, "broken_internal_links check failed");
        }
    }

    // Apply persistent dismissals to review-producing checks
    let reviewable = ["content_overlap", "contradiction_detection", "boilerplate_pollution", "content_quality", "tag_health", "broken_internal_links"];
    for check_name in reviewable {
        let dismissed_pairs = core.storage().list_dismissed_keys_sync(check_name).await.unwrap_or_default();
        if dismissed_pairs.is_empty() {
            continue;
        }
        let dismissed: std::collections::HashSet<String> =
            dismissed_pairs.into_iter().map(|(k, _)| k).collect();
        if let Some(result) = checks.get_mut(check_name) {
            apply_dismissals(check_name, result, &dismissed);
        }
    }

    // Run user-defined custom checks (per-DB, synchronous, SqliteStorage-only).
    // Failures are logged, not propagated — a bad custom rule must not take down
    // the built-in health report.
    let mut effective_config = config.clone();
    if let Some(sqlite) = core.storage.as_sqlite() {
        match core.get_custom_health_checks().await {
            Ok(custom_checks) if !custom_checks.is_empty() => {
                match custom::run_all(sqlite, &custom_checks) {
                    Ok(results) => {
                        for (key, result, check) in results {
                            // Feed each custom check's weight into the aggregate-score
                            // config so zero-weight rules stay informational and
                            // positive-weight rules contribute at the requested weight.
                            effective_config.overrides.insert(
                                key.clone(),
                                HealthCheckOverride {
                                    enabled: check.enabled,
                                    weight: Some(check.weight),
                                },
                            );
                            checks.insert(key, result);
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "custom health checks failed"),
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "load custom health checks failed"),
        }
    }


    // Drop disabled checks entirely (config-driven).
    checks.retain(|name, _| {
        effective_config
            .overrides
            .get(name)
            .map(|o| o.enabled)
            .unwrap_or(true)
    });

    // Aggregate score
    let overall_score = aggregate_score(&checks, Some(&effective_config));
    let overall_status = HealthStatus::from_score(overall_score).as_str().to_string();

    // Count auto-fixable vs requires-review
    let auto_fixable = checks
        .values()
        .filter(|c| c.auto_fixable && c.status != "ok")
        .count() as i32;
    let requires_review = checks
        .values()
        .filter(|c| c.requires_review && c.status != "ok")
        .count() as i32;

    let atom_count = raw.total_atoms;

    // Fetch previous report for trending (before storing the current one)
    let (previous_score, previous_check_scores) =
        match core.get_latest_health_report().await {
            Ok(Some(prev)) => {
                let check_scores: HashMap<String, u32> =
                    prev.checks.iter().map(|(k, v)| (k.clone(), v.score)).collect();
                (Some(prev.overall_score), Some(check_scores))
            }
            _ => (None, None),
        };

    let report = HealthReport {
        overall_score,
        overall_status,
        computed_at: computed_at.clone(),
        atom_count,
        checks,
        auto_fixable,
        requires_review,
        previous_score,
        previous_check_scores,
    };

    // Persist for trending (fire-and-forget; ignore errors)
    let _ = store_report(core, &report).await;

    Ok(report)
}

/// Compute a single named health check in isolation.
///
/// Accepts any check name from the standard set. For the async
/// `broken_internal_links` check, runs `compute_link_check` directly.
/// Returns `(check_name, HealthCheckResult)` so callers can update
/// a cached `HealthReport` in place.
pub async fn compute_single_check(
    core: &AtomicCore,
    check_name: &str,
) -> Result<(String, HealthCheckResult), AtomicCoreError> {
    let mut result = match check_name {
        // Async check — requires per-atom DB lookups
        "broken_internal_links" => compute_link_check(core).await?,
        // Sync checks — fetch raw data once, dispatch to the appropriate fn
        "embedding_coverage"
        | "tagging_coverage"
        | "content_overlap"
        | "source_uniqueness"
        | "wiki_coverage"
        | "semantic_graph_freshness"
        | "content_quality"
        | "orphan_tags"
        | "tag_health"
        | "contradiction_detection"
        | "boilerplate_pollution" => {
            let raw = core.storage().health_check_data_sync().await?;
            match check_name {
                "embedding_coverage"       => checks::embedding_coverage(&raw),
                "tagging_coverage"         => checks::tagging_coverage(&raw),
                "content_overlap"          => checks::content_overlap(&raw),
                "source_uniqueness"        => checks::source_uniqueness(&raw),
                "wiki_coverage"            => checks::wiki_coverage(&raw),
                "semantic_graph_freshness" => checks::semantic_graph_freshness(&raw),
                "content_quality"          => checks::content_quality(&raw),
                "orphan_tags"              => checks::orphan_tags(&raw),
                "tag_health"               => checks::tag_health(&raw),
                "contradiction_detection"  => checks::contradiction_detection(&raw),
                "boilerplate_pollution"    => checks::boilerplate_pollution(&raw),
                _ => unreachable!(),
            }
        }
        _ => {
            return Err(AtomicCoreError::Validation(format!(
                "Unknown health check: {check_name}"
            )))
        }
    };
    // Apply persistent dismissals
    if matches!(check_name, "content_overlap" | "contradiction_detection" | "boilerplate_pollution" | "content_quality" | "tag_health") {
        let dismissed_pairs = core.storage().list_dismissed_keys_sync(check_name).await.unwrap_or_default();
        if !dismissed_pairs.is_empty() {
            let dismissed: std::collections::HashSet<String> =
                dismissed_pairs.into_iter().map(|(k, _)| k).collect();
            apply_dismissals(check_name, &mut result, &dismissed);
        }
    }
    Ok((check_name.to_string(), result))
}

/// Store a completed report in the health_reports table.
async fn store_report(
    core: &AtomicCore,
    report: &HealthReport,
) -> Result<(), AtomicCoreError> {
    use crate::health::audit::StoredHealthReport;
    let check_scores: HashMap<String, u32> = report
        .checks
        .iter()
        .map(|(k, v)| (k.clone(), v.score))
        .collect();
    let stored = StoredHealthReport {
        id: uuid::Uuid::new_v4().to_string(),
        computed_at: report.computed_at.clone(),
        overall_score: report.overall_score,
        check_scores: serde_json::to_string(&check_scores).unwrap_or_default(),
        atom_count: report.atom_count,
        auto_fixes_applied: 0,
        report_json: serde_json::to_string(report).unwrap_or_default(),
    };
    core.storage().store_health_report_sync(&stored).await
}

/// Per-link detail within a broken atom.
#[derive(serde::Serialize, Clone)]
struct BrokenLinkDetail {
    raw: String,
    target: String,
    kind: String,
}

/// Atom-level summary of broken links.
#[derive(serde::Serialize, Clone)]
struct BrokenLinkItem {
    atom_id: String,
    atom_title: String,
    links: Vec<BrokenLinkDetail>,
}

pub(crate) fn title_preview(content: &str) -> String {
    for line in content.lines() {
        let clean = line.trim().trim_start_matches('#').trim();
        if !clean.is_empty() {
            return if clean.len() > 80 {
                format!("{}\u{2026}", &clean[..80])
            } else {
                clean.to_string()
            };
        }
    }
    String::new()
}

async fn compute_link_check(core: &AtomicCore) -> Result<HealthCheckResult, AtomicCoreError> {
    use link_resolution::{extract_internal_links, vault_root};

    let candidates = core.storage().get_link_candidate_atoms_sync().await?;
    if candidates.is_empty() {
        return Ok(HealthCheckResult {
            status: "ok".to_string(),
            score: 100,
            auto_fixable: false,
            requires_review: false,
            informational: false,
            fix_action: None,
            data: serde_json::json!({ "broken_count": 0, "affected_atoms": 0, "broken_link_list": [], "broken_link_list_truncated": false }),
        });
    }

    let mut broken_count = 0i32;
    let mut affected_atoms = 0i32;
    let mut broken_items: Vec<BrokenLinkItem> = Vec::new();

    for (atom_id, content, source_url) in &candidates {
        let links = extract_internal_links(content, source_url.as_deref());
        if links.is_empty() {
            continue;
        }

        let candidate_urls: Vec<String> = links
            .iter()
            .flat_map(|l| l.candidate_source_urls.iter().cloned())
            .collect();

        let url_map = core
            .storage()
            .find_atoms_by_source_urls_sync(candidate_urls)
            .await
            .unwrap_or_default();

        let vault_pfx = source_url
            .as_deref()
            .and_then(vault_root)
            .map(|s| s.to_string());

        let mut atom_broken = false;
        let mut atom_link_details: Vec<BrokenLinkDetail> = Vec::new();
        for link in &links {
            let resolved_by_url = link
                .candidate_source_urls
                .iter()
                .any(|u| url_map.contains_key(u));

            if resolved_by_url {
                continue;
            }

            let resolved_by_name = if let (Some(name), Some(pfx)) = (&link.wikilink_name, &vault_pfx) {
                core.storage()
                    .find_atom_by_wikilink_name_sync(name.clone(), pfx.clone())
                    .await
                    .unwrap_or(None)
                    .is_some()
            } else {
                false
            };

            if !resolved_by_name {
                broken_count += 1;
                atom_broken = true;
                let kind = if link.wikilink_name.is_some() {
                    "wikilink".to_string()
                } else {
                    "markdown".to_string()
                };
                let target = link
                    .wikilink_name
                    .as_deref()
                    .unwrap_or(&link.href)
                    .to_string();
                atom_link_details.push(BrokenLinkDetail {
                    raw: link.original.clone(),
                    target,
                    kind,
                });
            }
        }
        if atom_broken {
            affected_atoms += 1;
            if broken_items.len() < 50 {
                broken_items.push(BrokenLinkItem {
                    atom_id: atom_id.clone(),
                    atom_title: title_preview(content),
                    links: atom_link_details,
                });
            }
        }
    }

    let total_atoms = core
        .count_atoms()
        .await
        .unwrap_or(candidates.len() as i32);
    let clean_atoms = (total_atoms - affected_atoms).max(0);
    let score = if total_atoms == 0 {
        100
    } else {
        (clean_atoms as f64 / total_atoms as f64 * 100.0) as u32
    };
    let status = if broken_count == 0 { "ok" } else { "warning" };
    let truncated = affected_atoms > 50;

    Ok(HealthCheckResult {
        status: status.to_string(),
        score,
        auto_fixable: broken_count > 0,
        requires_review: broken_count > 0,
        informational: false,
        fix_action: Some("resolve_internal_links".to_string()),
        data: serde_json::json!({
            "broken_count": broken_count,
            "affected_atoms": affected_atoms,
            "broken_link_list": broken_items,
            "broken_link_list_truncated": truncated,
        }),
    })
}
/// Run auto-fixes up to the requested tier.
pub async fn run_fix(
    core: &AtomicCore,
    req: &FixRequest,
) -> Result<FixResponse, AtomicCoreError> {
    let raw = core.storage().health_check_data_sync().await?;
    let checks = checks::run_all(&raw);
    let max_tier = req.max_tier();
    let dry_run = req.is_dry_run();

    let mut actions_taken: Vec<FixAction> = Vec::new();
    let mut skipped: Vec<SkippedFix> = Vec::new();

    // Helper: should we run this check's fix?
    let should_run = |check_name: &str| -> bool {
        if let Some(filter) = &req.checks {
            filter.iter().any(|c| c == check_name)
        } else {
            true
        }
    };

    // --- Safe tier ---

    if should_run("embedding_coverage") {
        if let Some(check) = checks.get("embedding_coverage") {
            if check.auto_fixable && check.status != "ok" {
                match fixes::fix_embedding_coverage(core, dry_run).await {
                    Ok(Some(action)) => actions_taken.push(action),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "embedding_coverage fix failed");
                    }
                }
            }
        }
    }

    if should_run("semantic_graph_freshness") {
        if let Some(check) = checks.get("semantic_graph_freshness") {
            if check.auto_fixable && check.status != "ok" {
                match fixes::fix_graph_freshness(core, dry_run).await {
                    Ok(Some(action)) => actions_taken.push(action),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "semantic_graph_freshness fix failed");
                    }
                }
            }
        }
    }

    if should_run("tagging_coverage") {
        if let Some(check) = checks.get("tagging_coverage") {
            if check.auto_fixable && check.status != "ok" {
                let skipped_untagged = raw.skipped_untagged;
                match fixes::fix_tagging_coverage(core, skipped_untagged, dry_run).await {
                    Ok(Some(action)) => actions_taken.push(action),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "tagging_coverage fix failed");
                    }
                }
            }
        }
    }

    // --- Low tier ---

    if matches!(max_tier, FixTier::Low | FixTier::Medium | FixTier::High) {
        if should_run("orphan_tags") {
            if let Some(check) = checks.get("orphan_tags") {
                if check.auto_fixable && check.status != "ok" {
                    match fixes::fix_orphan_tags(core, &raw, dry_run).await {
                        Ok(Some(action)) => actions_taken.push(action),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "orphan_tags fix failed"),
                    }
                }
            }
        }

        if should_run("tag_health") {
            if let Some(check) = checks.get("tag_health") {
                if check.auto_fixable && check.status != "ok" {
                    match fixes::fix_tag_health_single_atom(core, &raw, dry_run).await {
                        Ok(Some(action)) => actions_taken.push(action),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "tag_health single-atom fix failed"),
                    }
                }
            }
        }

        if should_run("wiki_coverage") {
            if let Some(check) = checks.get("wiki_coverage") {
                if check.auto_fixable && check.status != "ok" {
                    match fixes::fix_wiki_coverage(core, &raw, dry_run).await {
                        Ok(Some(action)) => actions_taken.push(action),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "wiki_coverage fix failed"),
                    }
                }
            }
        }

        if should_run("broken_internal_links")
            && matches!(checks.get("broken_internal_links"), Some(c) if c.auto_fixable && c.status != "ok") {
                match fixes::fix_broken_internal_links(core, dry_run).await {
                    Ok(Some(action)) => actions_taken.push(action),
                    Ok(None) => tracing::debug!("broken_internal_links: no links to fix"),
                    Err(e) => tracing::warn!(error = %e, "broken_internal_links fix failed"),
                }
            }
    }

    // --- Medium tier ---

    if matches!(max_tier, FixTier::Medium | FixTier::High)
        && should_run("source_uniqueness") {
            if let Some(check) = checks.get("source_uniqueness") {
                if check.auto_fixable && check.status != "ok" {
                    match fixes::fix_source_uniqueness(core, &raw, dry_run).await {
                        Ok(Some(action)) => actions_taken.push(action),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "source_uniqueness fix failed"),
                    }
                }
            }
        }
    // Mark high-tier issues as skipped with reason
    for (check_name, check) in &checks {
        if check.requires_review && check.status != "ok" && !should_run(check_name) {
            skipped.push(SkippedFix {
                check: check_name.clone(),
                reason: "requires_review".to_string(),
                count: check.data.get("count").and_then(|v| v.as_i64()).unwrap_or(0)
                    as i32,
            });
        }
    }

    // Recompute score after fixes (if not dry run) — always weight with
    // the caller DB's current HealthConfig so the number matches compute_health.
    let config = core.get_health_config().await.unwrap_or_default();
    let new_score = if !dry_run && !actions_taken.is_empty() {
        let new_raw = core.storage().health_check_data_sync().await?;
        let new_checks = checks::run_all(&new_raw);
        aggregate_score(&new_checks, Some(&config))
    } else {
        aggregate_score(&checks, Some(&config))
    };

    Ok(FixResponse {
        mode: req.mode.clone(),
        actions_taken,
        skipped,
        new_score,
    })
}




/// Build a stable item key for a pair. Sorts atom IDs lexicographically so
/// key ordering is independent of which atom is A vs B.
pub fn pair_key(a: &str, b: &str) -> String {
    if a <= b {
        format!("{}__{}", a, b)
    } else {
        format!("{}__{}", b, a)
    }
}

/// Filter a check result's JSON data to exclude dismissed entries.
pub(crate) fn apply_dismissals(
    check_name: &str,
    result: &mut HealthCheckResult,
    dismissed_keys: &std::collections::HashSet<String>,
) {
    if dismissed_keys.is_empty() {
        return;
    }

    use serde_json::Value;
    let data = &mut result.data;

    match check_name {
        "content_overlap" => {
            if let Some(pairs) = data.get_mut("pairs").and_then(Value::as_array_mut) {
                pairs.retain(|p| {
                    let a = p.get("atom_a").and_then(|o| o.get("id")).and_then(Value::as_str).unwrap_or("");
                    let b = p.get("atom_b").and_then(|o| o.get("id")).and_then(Value::as_str).unwrap_or("");
                    !dismissed_keys.contains(&pair_key(a, b))
                });
                let new_count = pairs.len();
                if let Some(c) = data.get_mut("count") {
                    *c = Value::from(new_count);
                }
                if let Some(c) = data.get_mut("cross_source_overlaps") {
                    *c = Value::from(new_count);
                }
            }
        }
        "contradiction_detection" => {
            if let Some(pairs) = data.get_mut("pairs").and_then(Value::as_array_mut) {
                pairs.retain(|p| {
                    let a = p.get("atom_a").and_then(|o| o.get("id")).and_then(Value::as_str).unwrap_or("");
                    let b = p.get("atom_b").and_then(|o| o.get("id")).and_then(Value::as_str).unwrap_or("");
                    !dismissed_keys.contains(&pair_key(a, b))
                });
                let new_count = pairs.len();
                if let Some(c) = data.get_mut("potential_contradictions") {
                    *c = Value::from(new_count);
                }
                if new_count == 0 {
                    result.requires_review = false;
                }
            }
        }
        "boilerplate_pollution" => {
            if let Some(arr) = data.get_mut("affected_atoms").and_then(Value::as_array_mut) {
                arr.retain(|entry| {
                    let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                    !dismissed_keys.contains(id)
                });
                let new_count = arr.len();
                if let Some(c) = data.get_mut("count") {
                    *c = Value::from(new_count);
                }
                if new_count == 0 {
                    result.requires_review = false;
                }
            }
        }
        "content_quality" => {
            if let Some(ns) = data
                .pointer_mut("/issues/no_source/atoms")
                .and_then(Value::as_array_mut)
            {
                ns.retain(|entry| {
                    let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                    !dismissed_keys.contains(id)
                });
                let new_count = ns.len();
                if let Some(c) = data.pointer_mut("/issues/no_source/count") {
                    *c = Value::from(new_count);
                }
                if new_count == 0 {
                    result.requires_review = false;
                }
            }
        }
        "tag_health" => {
            if let Some(arr) = data.get_mut("rootless_tag_list").and_then(Value::as_array_mut) {
                arr.retain(|t| {
                    let id = t.get("id").and_then(Value::as_str).unwrap_or("");
                    !dismissed_keys.contains(id)
                });
                let new_count = arr.len();
                if let Some(c) = data.get_mut("rootless_tags") {
                    *c = Value::from(new_count);
                }
                if new_count == 0 {
                    result.requires_review = false;
                }
            }
            if let Some(arr) = data.get_mut("similar_name_pair_list").and_then(Value::as_array_mut) {
                arr.retain(|p| {
                    let pair_id = p.get("pair_id").and_then(Value::as_str).unwrap_or("");
                    !dismissed_keys.contains(pair_id)
                });
                let new_similar = arr.len();
                if let Some(c) = data.get_mut("similar_name_pairs") {
                    *c = Value::from(new_similar);
                }
            }
            if let Some(arr) = data.get_mut("single_atom_tag_list").and_then(Value::as_array_mut) {
                arr.retain(|t| {
                    let id = t.get("id").and_then(Value::as_str).unwrap_or("");
                    !dismissed_keys.contains(id)
                });
                let new_count = arr.len() as i32;
                if let Some(c) = data.get_mut("single_atom_tags") {
                    *c = Value::from(new_count);
                }
            }
        }
        "broken_internal_links" => {
            if let Some(arr) = data.get_mut("broken_link_list").and_then(Value::as_array_mut) {
                arr.retain(|entry| {
                    let id = entry.get("atom_id").and_then(Value::as_str).unwrap_or("");
                    !dismissed_keys.contains(id)
                });
                let new_count = arr.len() as i64;
                // Recompute broken_count as sum of remaining link counts
                let new_broken: i64 = arr.iter().map(|entry| {
                    entry.get("links").and_then(|l| l.as_array()).map_or(0, |v| v.len() as i64)
                }).sum();
                if let Some(c) = data.get_mut("affected_atoms") {
                    *c = Value::from(new_count);
                }
                if let Some(c) = data.get_mut("broken_count") {
                    *c = Value::from(new_broken);
                }
                if new_count == 0 {
                    result.requires_review = false;
                    result.auto_fixable = false;
                }
            }
        }
        _ => {}
    }
}
#[cfg(test)]
mod tests;