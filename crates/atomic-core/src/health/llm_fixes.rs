//! LLM-powered fix implementations.
//!
//! These fixes call the configured LLM provider to make judgment calls that
//! deterministic SQL cannot.  All are logged for undo.
//!
//! Currently implemented:
//! - `fix_untagged_atoms` — re-run tagging pipeline on zero-tag complete atoms.
//! - `merge_duplicate_pair` — synthesise two high-similarity atoms into one.

use crate::error::AtomicCoreError;
use crate::health::{audit, FixAction};
use crate::providers::{create_llm_provider, ProviderConfig};

use crate::providers::{LlmConfig};
use crate::providers::types::Message;
use crate::AtomicCore;
use serde_json::json;

/// Re-run the tagging pipeline on atoms that completed tagging but got 0 tags.
pub async fn fix_untagged_complete_atoms(
    core: &AtomicCore,
    untagged_ids: &[String],
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    if untagged_ids.is_empty() {
        return Ok(None);
    }

    let count = untagged_ids.len() as i32;

    let id = if dry_run {
        "dry_run".to_string()
    } else {
        // Reset tagging status to pending so the pipeline picks them up
        for atom_id in untagged_ids {
            let _ = core
                .storage()
                .set_tagging_status_sync(atom_id, "pending", None)
                .await;
        }

        // Trigger the tagging pipeline
        let processed = core.process_pending_tagging(|_| {}).await.unwrap_or(0);
        tracing::info!(count, processed, "llm_fixes: re-queued untagged atoms for tagging");

        audit::log_fix(
            core,
            "tagging_coverage",
            "requeued_untagged_for_tagging",
            "low",
            Some(untagged_ids),
            None,
            json!({"atom_ids": untagged_ids}),
            json!({"requeued": count, "processed": processed}),
            None,
            None,
        )
        .await?
    };

    Ok(Some(FixAction {
        id,
        check: "tagging_coverage".to_string(),
        action: "requeued_untagged_for_tagging".to_string(),
        count,
        details: untagged_ids.iter().take(10).cloned().collect(),
    }))
}

/// Merge two highly-similar atoms using the LLM.
///
/// The LLM synthesises both atoms into one coherent document, then:
/// 1. Updates the newer atom with the merged content.
/// 2. Deletes the older atom.
/// 3. Re-queues the merged atom for embedding + tagging.
///
/// This is a High-tier action and must be explicitly requested via
/// `POST /api/health/fix/{check}/{item_id}`.
pub async fn merge_duplicate_pair(
    core: &AtomicCore,
    atom_a_id: &str,
    atom_b_id: &str,
    dry_run: bool,
) -> Result<Option<FixAction>, AtomicCoreError> {
    let Some(atom_a) = core.get_atom(atom_a_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_a_id} not found")));
    };
    let Some(atom_b) = core.get_atom(atom_b_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_b_id} not found")));
    };

    // Determine which is newer (keep) and which is older (delete)
    let (keep, delete) = if atom_a.atom.updated_at >= atom_b.atom.updated_at {
        (atom_a, atom_b)
    } else {
        (atom_b, atom_a)
    };

    let merge_prompt = format!(
        "You are merging two duplicate knowledge base atoms into one definitive version.\n\n\
        ATOM A (source: {source_a}, created: {date_a}):\n{content_a}\n\n\
        ATOM B (source: {source_b}, created: {date_b}):\n{content_b}\n\n\
        Rules:\n\
        - Combine all unique information from both atoms into one coherent document\n\
        - If they contradict each other, prefer the more recent source\n\
        - Preserve all actionable details (URLs, commands, config values)\n\
        - Use clean markdown with proper headings\n\
        - Add a '## Sources' section at the bottom listing both original source URLs\n\
        - Do not add commentary — just produce the merged document\n\n\
        Output the merged markdown only.",
        source_a = keep.atom.source_url.as_deref().unwrap_or("manual"),
        date_a = keep.atom.created_at,
        content_a = &keep.atom.content,
        source_b = delete.atom.source_url.as_deref().unwrap_or("manual"),
        date_b = delete.atom.created_at,
        content_b = &delete.atom.content,
    );

    if dry_run {
        return Ok(Some(FixAction {
            id: "dry_run".to_string(),
            check: "duplicate_detection".to_string(),
            action: "merge_with_llm".to_string(),
            count: 1,
            details: vec![format!("Would merge {} into {}", delete.atom.id, keep.atom.id)],
        }));
    }

    // Get LLM provider
    let settings = core.get_settings_map().await.unwrap_or_default();
    let provider_config = ProviderConfig::from_settings(&settings);
    let llm = create_llm_provider(&provider_config).map_err(|e| {
        AtomicCoreError::Configuration(format!("LLM provider unavailable for merge: {e}"))
    })?;

    let model = settings
        .get("wiki_model")
        .cloned()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string());

    let messages = vec![Message::user(merge_prompt.clone())];
    let config = LlmConfig::new(model).with_params(
        crate::providers::types::GenerationParams::new().with_max_tokens(4096),
    );

    let response = llm.complete(&messages, &config).await?;
    let merged_content = response.content.clone();

    if merged_content.is_empty() {
        return Err(AtomicCoreError::Validation(
            "LLM returned empty merged content".to_string(),
        ));
    }

    // Capture before state
    let before_state = json!([
        {
            "id": keep.atom.id,
            "content": keep.atom.content,
            "source_url": keep.atom.source_url,
            "tag_ids": keep.tags.iter().map(|t| t.id.clone()).collect::<Vec<_>>()
        },
        {
            "id": delete.atom.id,
            "content": delete.atom.content,
            "source_url": delete.atom.source_url,
            "tag_ids": delete.tags.iter().map(|t| t.id.clone()).collect::<Vec<_>>()
        }
    ]);

    // Merge tags from deleted atom into keeper
    let delete_tag_ids: Vec<String> = delete.tags.iter().map(|t| t.id.clone()).collect();
    if !delete_tag_ids.is_empty() {
        let _ = core
            .storage()
            .link_tags_to_atom_impl(&keep.atom.id, &delete_tag_ids)
            .await;
    }

    // Update the keeper with merged content
    let upd = crate::UpdateAtomRequest {
        content: merged_content.clone(),
        source_url: keep.atom.source_url.clone(),
        published_at: None,
        tag_ids: None,
    };
    core.update_atom(&keep.atom.id, upd, |_| {}).await?;

    // Delete the older atom
    core.delete_atom(&delete.atom.id).await?;

    let fix_id = audit::log_fix(
        core,
        "duplicate_detection",
        "merge_with_llm",
        "high",
        Some(&[keep.atom.id.clone(), delete.atom.id.clone()]),
        None,
        before_state,
        json!({
            "kept_id": keep.atom.id,
            "deleted_id": delete.atom.id,
            "merged_content_length": merged_content.len()
        }),
        Some(&merge_prompt),
        Some(&merged_content),
    )
    .await?;

    tracing::info!(
        kept = %keep.atom.id,
        deleted = %delete.atom.id,
        "duplicate pair merged with LLM"
    );

    Ok(Some(FixAction {
        id: fix_id,
        check: "duplicate_detection".to_string(),
        action: "merge_with_llm".to_string(),
        count: 1,
        details: vec![
            format!("Kept: {}", keep.atom.id),
            format!("Deleted: {}", delete.atom.id),
        ],
    }))
}
