//! Auto-fix orchestration.
//!
//! `run_fix` dispatches deterministic fixes by tier (Safe / Low / Medium).
//! High-tier fixes are surfaced as `SkippedFix` entries with a reason, never
//! executed automatically. LLM-powered fixes (merge, summary, reorg) live in
//! `llm_fixes.rs` and are invoked explicitly by the UI.

use super::score::aggregate_score;
use super::types::{FixAction, FixRequest, FixResponse, FixTier, SkippedFix};
use super::{checks, fixes};
use crate::error::AtomicCoreError;
use crate::AtomicCore;

/// Run auto-fixes up to the requested tier.
pub async fn run_fix(
    core: &AtomicCore,
    req: &FixRequest,
) -> Result<FixResponse, AtomicCoreError> {
    let config = core.get_health_config().await.unwrap_or_default();
    let raw = core.storage().health_check_data_sync(config.thresholds.clone()).await?;
    let checks = checks::run_all(&raw, &config.thresholds);
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
    let new_score = if !dry_run && !actions_taken.is_empty() {
        let new_raw = core
            .storage()
            .health_check_data_sync(config.thresholds.clone())
            .await?;
        let new_checks = checks::run_all(&new_raw, &config.thresholds);
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
