//! Deterministic (non-LLM) auto-fix implementations.
//!
//! Each function either executes the fix immediately (if `dry_run = false`) or
//! describes what it would do (if `dry_run = true`).  Every executed fix logs
//! a `HealthFixLog` row for undo support.

use super::{audit, FixAction};
use crate::error::AtomicCoreError;
use crate::storage::sqlite::health::HealthRawData;
use crate::AtomicCore;
use serde_json::json;


/// Retry failed embeddings and process pending ones.  Safe tier.
pub async fn fix_embedding_coverage(
    core: &AtomicCore,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    let status = core.get_pipeline_status().await?;
    let pending = status.pending;
    let failed = status.failed_count;
    let count = pending + failed;

    if count == 0 {
        return Ok(None);
    }

    let id = if dry_run {
        "dry_run".to_string()
    } else {
        let retried = core.retry_failed_embeddings(|_| {}).await.unwrap_or(0);
        let processed = core.process_pending_embeddings(|_| {}).await.unwrap_or(0);
        tracing::info!(retried, processed, "embedding_coverage fix applied");

        audit::log_fix(
            core,
            "embedding_coverage",
            "retry_failed_and_process_pending",
            "safe",
            None,
            None,
            json!({"failed": failed, "pending": pending}),
            json!({"retried": retried, "processed": processed}),
            None,
            None,
        )
        .await?
    };

    Ok(Some(FixAction {
        id,
        check: "embedding_coverage".to_string(),
        action: "retry_failed_and_process_pending".to_string(),
        count,
        details: vec![
            format!("{} failed retried", failed),
            format!("{} pending processed", pending),
        ],
    }))
}

/// Queue a semantic edge graph rebuild.  Safe tier.
pub async fn fix_graph_freshness(
    core: &AtomicCore,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    let id = if dry_run {
        "dry_run".to_string()
    } else {
        let edges = core.rebuild_semantic_edges().await.unwrap_or(0);
        tracing::info!(edges, "semantic_graph_freshness fix: edges rebuilt");

        audit::log_fix(
            core,
            "semantic_graph_freshness",
            "queued_rebuild",
            "safe",
            None,
            None,
            json!({}),
            json!({"edges_rebuilt": edges}),
            None,
            None,
        )
        .await?
    };

    Ok(Some(FixAction {
        id,
        check: "semantic_graph_freshness".to_string(),
        action: "queued_rebuild".to_string(),
        count: 1,
        details: vec!["Semantic edge graph rebuild queued".to_string()],
    }))
}

/// Reset skipped-with-no-tags atoms to pending and run the tagging pipeline.  Safe tier.
///
/// These are atoms whose `tagging_status = 'skipped'` AND have zero tags assigned.
/// They were typically imported before auto-tagging was configured and never retried.
pub async fn fix_tagging_coverage(
    core: &AtomicCore,
    skipped_untagged_count: i32,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    if skipped_untagged_count == 0 {
        return Ok(None);
    }

    let id = if dry_run {
        "dry_run".to_string()
    } else {
        let reset = core
            .storage()
            .reset_skipped_untagged_to_pending_sync()
            .await
            .unwrap_or(0);
        let processed = core.process_pending_tagging(|_| {}).await.unwrap_or(0);
        tracing::info!(reset, processed, "tagging_coverage fix: skipped atoms re-queued");

        audit::log_fix(
            core,
            "tagging_coverage",
            "reset_skipped_untagged_to_pending",
            "safe",
            None,
            None,
            json!({"skipped_untagged": skipped_untagged_count}),
            json!({"reset": reset, "processed": processed}),
            None,
            None,
        )
        .await?
    };

    Ok(Some(FixAction {
        id,
        check: "tagging_coverage".to_string(),
        action: "reset_skipped_untagged_to_pending".to_string(),
        count: skipped_untagged_count,
        details: vec![format!("{} atoms reset to pending for re-tagging", skipped_untagged_count)],
    }))
}

/// Delete orphan tags (tags with 0 atoms and no children).  Low tier.
pub async fn fix_orphan_tags(
    core: &AtomicCore,
    raw: &HealthRawData,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    if raw.orphan_tags.is_empty() {
        return Ok(None);
    }

    let count = raw.orphan_tags.len() as i32;
    let names: Vec<String> = raw.orphan_tags.iter().map(|(_, n)| n.clone()).collect();
    let ids: Vec<String> = raw.orphan_tags.iter().map(|(id, _)| id.clone()).collect();

    let before_state = json!(raw
        .orphan_tags
        .iter()
        .map(|(id, name)| json!({"id": id, "name": name, "parent_id": null}))
        .collect::<Vec<_>>());

    let id = if dry_run {
        "dry_run".to_string()
    } else {
        for tag_id in &ids {
            if let Err(e) = core.delete_tag(tag_id, false).await {
                tracing::warn!(tag_id, error = %e, "failed to delete orphan tag");
            }
        }
        tracing::info!(count, "orphan_tags fix: deleted tags");

        audit::log_fix(
            core,
            "orphan_tags",
            "deleted_tags",
            "low",
            None,
            Some(&ids),
            before_state,
            json!({"deleted": count}),
            None,
            None,
        )
        .await?
    };

    Ok(Some(FixAction {
        id,
        check: "orphan_tags".to_string(),
        action: "deleted_tags".to_string(),
        count,
        details: names,
    }))
}

/// Generate missing wiki articles for eligible tags.  Low tier.
/// Rate-limited to 3 generations per fix run to avoid long waits.
pub async fn fix_wiki_coverage(
    core: &AtomicCore,
    raw: &HealthRawData,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    let gaps = &raw.wiki_gaps;
    let stale = &raw.wiki_stale;

    if gaps.is_empty() && stale.is_empty() {
        return Ok(None);
    }

    // Prioritise by atom count (highest first), max 3 total
    let mut to_generate: Vec<(String, String)> = gaps
        .iter()
        .map(|g| (g.tag_id.clone(), g.tag_name.clone()))
        .collect();
    // Then stale wikis
    for s in stale {
        to_generate.push((s.tag_id.clone(), s.tag_name.clone()));
    }
    to_generate.truncate(3);

    let count = to_generate.len() as i32;
    let detail_names: Vec<String> = to_generate.iter().map(|(_, n)| n.clone()).collect();

    let id = if dry_run {
        "dry_run".to_string()
    } else {
        for (tag_id, tag_name) in &to_generate {
            match core.generate_wiki(tag_id, tag_name).await {
                Ok(_) => tracing::info!(tag_id, "wiki generated"),
                Err(e) => tracing::warn!(tag_id, error = %e, "wiki generation failed"),
            }
        }

        audit::log_fix(
            core,
            "wiki_coverage",
            "generated_wikis",
            "low",
            None,
            None,
            json!({"gaps": gaps.len(), "stale": stale.len()}),
            json!({"generated": count}),
            None,
            None,
        )
        .await?
    };

    Ok(Some(FixAction {
        id,
        check: "wiki_coverage".to_string(),
        action: "generated_wikis".to_string(),
        count,
        details: detail_names,
    }))
}

/// Deduplicate atoms with the exact same source_url.  Medium tier.
/// Keeps newest; merges tags from all duplicates; deletes older copies.
pub async fn fix_source_uniqueness(
    core: &AtomicCore,
    raw: &HealthRawData,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    if raw.duplicate_sources.is_empty() {
        return Ok(None);
    }

    let mut deleted_ids: Vec<String> = Vec::new();
    let mut before_atoms: Vec<serde_json::Value> = Vec::new();

    for (source_url, atom_ids) in &raw.duplicate_sources {
        if atom_ids.len() < 2 {
            continue;
        }

        // Fetch all atoms in this group to find newest
        let mut atoms_with_dates: Vec<(String, String)> = Vec::new(); // (id, updated_at)
        for id in atom_ids {
            if let Ok(Some(a)) = core.get_atom(id).await {
                atoms_with_dates.push((a.atom.id.clone(), a.atom.updated_at.clone()));

                // Capture before state
                let tag_ids: Vec<String> = a.tags.iter().map(|t| t.id.clone()).collect();
                before_atoms.push(json!({
                    "id": a.atom.id,
                    "content": a.atom.content,
                    "source_url": a.atom.source_url,
                    "tag_ids": tag_ids,
                }));
            }
        }

        // Sort by updated_at desc — newest first
        atoms_with_dates.sort_by(|a, b| b.1.cmp(&a.1));
        let keep_id = atoms_with_dates[0].0.clone();
        let to_delete: Vec<String> = atoms_with_dates[1..].iter().map(|(id, _)| id.clone()).collect();

        if dry_run {
            tracing::info!(
                source_url,
                keep = %keep_id,
                delete = ?to_delete,
                "dry_run: would merge source duplicates"
            );
        } else {
            // Collect all tags from duplicates into the keeper
            let mut all_tag_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            for id in &to_delete {
                if let Ok(tag_ids) = core.storage().get_atom_tag_ids_impl(id).await {
                    all_tag_ids.extend(tag_ids);
                }
            }
            // Merge tags onto keeper
            if !all_tag_ids.is_empty() {
                let tag_list: Vec<String> = all_tag_ids.into_iter().collect();
                let _ = core
                    .storage()
                    .link_tags_to_atom_impl(&keep_id, &tag_list)
                    .await;
            }
            // Delete duplicates — but skip locked atoms so source-of-truth
            // material never gets automatically merged away.
            for id in &to_delete {
                if core.is_atom_locked(id).await.unwrap_or(false) {
                    tracing::info!(id, "skipping locked atom in source-duplicate merge");
                    continue;
                }
                if let Err(e) = core.delete_atom(id).await {
                    tracing::warn!(id, error = %e, "failed to delete source duplicate atom");
                } else {
                    deleted_ids.push(id.clone());
                }
            }
        }
    }

    if deleted_ids.is_empty() && !dry_run {
        return Ok(None);
    }

    let count = if dry_run {
        raw.duplicate_sources
            .iter()
            .map(|(_, ids)| (ids.len() as i32 - 1).max(0))
            .sum::<i32>()
    } else {
        deleted_ids.len() as i32
    };

    let id = if dry_run {
        "dry_run".to_string()
    } else {
        audit::log_fix(
            core,
            "source_uniqueness",
            "deleted_atoms",
            "medium",
            Some(&deleted_ids),
            None,
            serde_json::Value::Array(before_atoms),
            json!({"deleted": count}),
            None,
            None,
        )
        .await?
    };

    Ok(Some(FixAction {
        id,
        check: "source_uniqueness".to_string(),
        action: "deleted_atoms".to_string(),
        count,
        details: if dry_run {
            raw.duplicate_sources
                .iter()
                .map(|(url, _)| url.clone())
                .collect()
        } else {
            deleted_ids
        },
    }))
}


/// Delete single-atom tags where `is_autotag_target = true`.  Low tier.
///
/// Only removes tags that were produced by the auto-tagger (is_autotag_target = 1)
/// AND have exactly 1 atom attached.  User-created single-atom tags (is_autotag_target = 0)
/// are left alone; those require human review.
pub async fn fix_tag_health_single_atom(
    core: &AtomicCore,
    raw: &HealthRawData,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    // Use the pre-fetched list; filter to autotag-only entries.
    let targets: Vec<_> = raw
        .single_atom_tag_list
        .iter()
        .filter(|t| t.is_autotag)
        .collect();

    if targets.is_empty() {
        return Ok(None);
    }

    let count = targets.len() as i32;
    let ids: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
    let names: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();

    let before_state = json!(targets
        .iter()
        .map(|t| json!({"id": t.id, "name": t.name, "is_autotag": t.is_autotag}))
        .collect::<Vec<_>>());

    let id = if dry_run {
        "dry_run".to_string()
    } else {
        for tag_id in &ids {
            if let Err(e) = core.delete_tag(tag_id, false).await {
                tracing::warn!(tag_id, error = %e, "failed to delete single-atom autotag");
            }
        }
        tracing::info!(count, "tag_health single-atom fix: deleted autotag-only tags");

        audit::log_fix(
            core,
            "tag_health",
            "deleted_single_atom_autotags",
            "low",
            None,
            Some(&ids),
            before_state,
            json!({"deleted": count}),
            None,
            None,
        )
        .await?
    };

    Ok(Some(FixAction {
        id,
        check: "tag_health".to_string(),
        action: "deleted_single_atom_autotags".to_string(),
        count,
        details: names,
    }))
}

/// Resolve broken internal links in all atoms to `atom://id` URIs.  Medium tier.
///
/// For each atom with relative markdown links or `[[wikilinks]]`:
/// 1. Resolve the href to a candidate source URL using the atom's vault prefix.
/// 2. Look up the target atom by source URL.
/// 3. Replace the original href with `atom://target_id`.
///
/// Unresolvable links are left untouched and reported in `details`.
pub async fn fix_broken_internal_links(
    core: &AtomicCore,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    use crate::health::link_resolution::{
        apply_link_replacements, extract_internal_links, vault_root, ResolvedLink,
    };

    let candidates = core.storage().get_link_candidate_atoms_sync().await?;
    if candidates.is_empty() {
        return Ok(None);
    }

    let mut fixed_total = 0i32;
    let mut unresolvable: Vec<String> = Vec::new();
    let mut before_state: Vec<serde_json::Value> = Vec::new();
    let mut atom_ids_changed: Vec<String> = Vec::new();

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

        let mut resolved: Vec<ResolvedLink> = Vec::new();

        for link in &links {
            // Try exact source URL match first
            let target_id = link
                .candidate_source_urls
                .iter()
                .find_map(|u| url_map.get(u).cloned());

            let target_id = if target_id.is_none() {
                // For wikilinks: fall back to vault-wide name search
                if let (Some(name), Some(pfx)) = (&link.wikilink_name, &vault_pfx) {
                    core.storage()
                        .find_atom_by_wikilink_name_sync(name.clone(), pfx.clone())
                        .await
                        .unwrap_or(None)
                        .map(|(id, _)| id)
                } else {
                    None
                }
            } else {
                target_id
            };

            match target_id {
                Some(id) => {
                    resolved.push(ResolvedLink {
                        original: link.original.clone(),
                        target_atom_id: id.clone(),
                        replacement: format!("atom://{}", id),
                    });
                    fixed_total += 1;
                }
                None => {
                    unresolvable.push(format!("{} (in {})", link.href, atom_id));
                }
            }
        }

        if resolved.is_empty() {
            continue;
        }

        before_state.push(json!({
            "id": atom_id,
            "content": content,
            "source_url": source_url,
        }));

        if !dry_run {
            let new_content = apply_link_replacements(content, &resolved);
                        core.update_atom_content_only(atom_id, crate::UpdateAtomRequest {
                            content: new_content,
                            source_url: source_url.clone(),
                            published_at: None,
                            tag_ids: None,
                        })
                .await
                .map_err(|e| {
                    tracing::warn!(atom_id, error = %e, "failed to update atom with resolved links");
                    e
                })?;
            atom_ids_changed.push(atom_id.clone());
        }
    }

    if fixed_total == 0 {
        return Ok(None);
    }

    let id = if dry_run {
        "dry_run".to_string()
    } else {
        audit::log_fix(
            core,
            "broken_internal_links",
            "resolve_internal_links",
            "medium",
            Some(&atom_ids_changed),
            None,
            serde_json::Value::Array(before_state),
            json!({
                "resolved": fixed_total,
                "unresolvable": unresolvable.len(),
            }),
            None,
            None,
        )
        .await?
    };

    tracing::info!(
        fixed = fixed_total,
        unresolvable = unresolvable.len(),
        dry_run,
        "broken_internal_links fix completed"
    );

    let mut details: Vec<String> = atom_ids_changed
        .iter()
        .map(|id| format!("Updated: {}", id))
        .collect();
    details.extend(
        unresolvable.iter().take(10).map(|s| format!("Unresolvable: {}", s)),
    );

    Ok(Some(FixAction {
        id,
        check: "broken_internal_links".to_string(),
        action: "resolve_internal_links".to_string(),
        count: fixed_total,
        details,
    }))
}

/// Strip one unresolved link from an atom's content, replacing it with its
/// display text (for markdown links) or the name (for wikilinks).
///
/// `link_raw` must exactly match the text as it appears in the atom content.
pub async fn remove_broken_link(
    core: &AtomicCore,
    atom_id: &str,
    link_raw: &str,
) -> Result<FixAction, AtomicCoreError> {
    let atom = core
        .get_atom(atom_id)
        .await?
        .ok_or_else(|| AtomicCoreError::NotFound(format!("atom {} not found", atom_id)))?;

    let content = &atom.atom.content;

    // Determine replacement text.
    let replacement = if let Some(inner) = parse_markdown_link_text(link_raw) {
        inner
    } else if let Some(name) = parse_wikilink_name(link_raw) {
        name
    } else {
        tracing::warn!(link_raw = %link_raw, "remove_broken_link: unrecognised link format, replacing with empty string");
        String::new()
    };

    let new_content = content.replacen(link_raw, &replacement, 1);

    // Record before state for undo.
    let before_state = serde_json::json!([{
        "id": atom_id,
        "content": content,
        "source_url": atom.atom.source_url,
    }]);
    let after_state = serde_json::json!([{
        "id": atom_id,
        "content": new_content,
        "source_url": atom.atom.source_url,
    }]);

    let tag_ids: Vec<String> = atom.tags.iter().map(|t| t.id.clone()).collect();
    let upd = crate::UpdateAtomRequest {
        content: new_content,
        source_url: atom.atom.source_url.clone(),
        published_at: atom.atom.published_at.clone(),
        tag_ids: Some(tag_ids),
    };
    core.update_atom(atom_id, upd, |_| {}).await?;

    let id = audit::log_fix(
        core,
        "broken_internal_links",
        "remove_link",
        "medium",
        Some(&[atom_id.to_string()]),
        None,
        before_state,
        after_state,
        None,
        None,
    )
    .await
    .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());

    Ok(FixAction {
        id,
        check: "broken_internal_links".to_string(),
        action: "remove_link".to_string(),
        count: 1,
        details: vec![format!("Removed link '{}' from atom {}", link_raw, atom_id)],
    })
}

/// Relink a broken link in an atom to a target atom via `atom://` URI.
pub async fn relink_broken_link(
    core: &AtomicCore,
    atom_id: &str,
    link_raw: &str,
    target_atom_id: &str,
) -> Result<FixAction, AtomicCoreError> {
    tracing::debug!(atom_id, target_atom_id, link_raw, "relink_broken_link: begin");
    let atom = core
        .get_atom(atom_id)
        .await?
        .ok_or_else(|| AtomicCoreError::NotFound(format!("atom {} not found", atom_id)))?;

    let target = core
        .get_atom(target_atom_id)
        .await?
        .ok_or_else(|| AtomicCoreError::NotFound(format!("target atom {} not found", target_atom_id)))?;

    let content = &atom.atom.content;

    // Guard: link_raw must be present in the content.
    if !content.contains(link_raw) {
        return Err(AtomicCoreError::Validation(format!(
            "Link '{link_raw}' not found in atom content; may have been already edited"
        )));
    }

    // Build replacement: markdown form [display_text](atom://<target_id>).
    let display_text = if let Some(text) = parse_markdown_link_text(link_raw) {
        text
    } else if let Some(name) = parse_wikilink_name(link_raw) {
        name
    } else {
        link_raw.to_string()
    };
    let new_link = format!("[{}](atom://{})", display_text, target_atom_id);
    let new_content = content.replacen(link_raw, &new_link, 1);

    if new_content == *content {
        return Err(AtomicCoreError::Validation(format!(
            "Link '{link_raw}' not found in atom content; may have been already edited"
        )));
    }

    let before_state = serde_json::json!([{
        "id": atom_id,
        "content": content,
        "source_url": atom.atom.source_url,
    }]);
    let after_state = serde_json::json!([{
        "id": atom_id,
        "content": new_content,
        "source_url": atom.atom.source_url,
    }]);

    let tag_ids: Vec<String> = atom.tags.iter().map(|t| t.id.clone()).collect();
    let upd = crate::UpdateAtomRequest {
        content: new_content.clone(),
        source_url: atom.atom.source_url.clone(),
        published_at: atom.atom.published_at.clone(),
        tag_ids: Some(tag_ids),
    };
    core.update_atom(atom_id, upd, |_| {}).await?;
    tracing::info!(atom_id, target_atom_id, link_raw, new_link = %new_link, "relink_broken_link: success");

    let target_title = crate::health::title_preview(&target.atom.content);
    let id = audit::log_fix(
        core,
        "broken_internal_links",
        "relink",
        "medium",
        Some(&[atom_id.to_string()]),
        None,
        before_state,
        after_state,
        None,
        None,
    )
    .await
    .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());

    Ok(FixAction {
        id,
        check: "broken_internal_links".to_string(),
        action: "relink".to_string(),
        count: 1,
        details: vec![format!(
            "Relinked '{}' in atom {} → atom://{} ('{}')",
            link_raw, atom_id, target_atom_id, target_title
        )],
    })
}

/// Extract display text from `[text](url)` markdown link.
fn parse_markdown_link_text(s: &str) -> Option<String> {
    if !s.starts_with('[') {
        return None;
    }
    let close_bracket = s.find("](")?;
    Some(s[1..close_bracket].to_string())
}

/// Extract name from `[[name]]` or `[[name|alias]]` wikilink.
fn parse_wikilink_name(s: &str) -> Option<String> {
    let inner = s.strip_prefix("[[")?.strip_suffix("]]")? ;
    let name = inner.split('|').next().unwrap_or(inner).trim();
    Some(name.to_string())
}