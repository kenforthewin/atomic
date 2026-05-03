//! LLM-powered fix implementations.
//!
//! These fixes call the configured LLM provider to make judgment calls that
//! deterministic SQL cannot.  All are logged for undo.
//!
//! Currently implemented:
//! - `fix_untagged_atoms` — re-run tagging pipeline on zero-tag complete atoms.
//! - `merge_duplicate_pair` — synthesise two high-similarity atoms into one.
//! - `verify_overlap_pair` — ask LLM if a flagged pair is a true duplicate.
//! - `verify_contradiction_pair` — ask LLM if a flagged pair truly contradicts.
//! - `merge_contradicting_pair` — LLM-reconcile two contradicting atoms into one.
use crate::error::AtomicCoreError;
use crate::health::{audit, FixAction};
use crate::providers::{create_llm_provider, ProviderConfig};

use crate::providers::{LlmConfig};
use crate::providers::types::Message;
use crate::AtomicCore;
use serde_json::json;

// ==================== User-tunable prompt instructions ====================
//
// Each health LLM fix sends a message with two pieces: an *instruction*
// (what to do, how to format) and a *data block* (the atom content under
// analysis). Only the instruction is user-tunable — the data block is
// assembled in code so placeholders can't be mis-spelled or elided.
//
// Overrides are read from the per-DB `settings` table under the keys
// below. An empty or missing value falls back to the builtin default.

const MERGE_DUPLICATES_SETTING_KEY: &str = "health.merge_duplicates_prompt";
const CONTRADICTION_DETECTION_SETTING_KEY: &str = "health.contradiction_detection_prompt";
const STRIP_BOILERPLATE_SETTING_KEY: &str = "health.strip_boilerplate_prompt";

const DEFAULT_MERGE_DUPLICATES_INSTRUCTION: &str = "You are merging two duplicate knowledge base atoms into one definitive version.\n\n\
    Rules:\n\
    - Combine all unique information from both atoms into one coherent document\n\
    - If they contradict each other, prefer the more recent source\n\
    - Preserve all actionable details (URLs, commands, config values)\n\
    - Use clean markdown with proper headings\n\
    - Add a '## Sources' section at the bottom listing both original source URLs\n\
    - Do not add commentary — just produce the merged document\n\n\
    Output the merged markdown only.";

const DEFAULT_CONTRADICTION_DETECTION_INSTRUCTION: &str = "Two knowledge base atoms may contradict each other. Write ONE sentence \
    (<= 25 words) describing what they disagree about. If they don't disagree, \
    reply exactly: NO_CONFLICT.";

const DEFAULT_STRIP_BOILERPLATE_INSTRUCTION: &str = "You are editing a knowledge base note. The note may contain boilerplate template \
    sections (headers, field labels, empty placeholders) that are not unique to this topic. \
    Remove all boilerplate; keep only the content that is specific to this note's subject. \
    Preserve all factual information. If the whole note is boilerplate, reply exactly: EMPTY. \
    Do not add commentary.";

/// Resolve a per-DB prompt override from the settings map, falling back to
/// the builtin default. Empty strings are treated as "not set" so a user
/// who clears the setting gets the default back.
fn resolve_prompt<'a>(settings: &'a std::collections::HashMap<String, String>, key: &str, default: &'a str) -> &'a str {
    settings
        .get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
}


/// Strip common LLM-response wrappers (markdown code fences, leading/trailing
/// whitespace) and return the JSON-candidate substring.
///
/// Handles: ```json\n{...}\n```, ```\n{...}\n```, plain {...}, and responses
/// with leading/trailing prose by extracting the outermost {...} or [...] block.
pub(crate) fn strip_llm_json_fences(raw: &str) -> &str {
    let s = raw.trim();
    // Strip ```json or ``` fences.
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```JSON"))
        .or_else(|| s.strip_prefix("```"))
        .map(|x| x.trim_start())
        .unwrap_or(s);
    let s = s.strip_suffix("```").map(|x| x.trim_end()).unwrap_or(s);
    let s = s.trim();
    // If there is still leading/trailing prose, extract the outermost JSON
    // object or array.
    if s.starts_with('{') || s.starts_with('[') {
        return s;
    }
    let start_obj = s.find('{');
    let start_arr = s.find('[');
    let start = match (start_obj, start_arr) {
        (Some(o), Some(a)) => Some(o.min(a)),
        (Some(o), None) => Some(o),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    if let Some(start) = start {
        let end_obj = s.rfind('}');
        let end_arr = s.rfind(']');
        let end = match (end_obj, end_arr) {
            (Some(o), Some(a)) => Some(o.max(a)),
            (Some(o), None) => Some(o),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        if let Some(end) = end {
            if end >= start {
                return &s[start..=end];
            }
        }
    }
    s
}

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

    let settings = core.get_settings_map().await.unwrap_or_default();
    let instruction = resolve_prompt(
        &settings,
        MERGE_DUPLICATES_SETTING_KEY,
        DEFAULT_MERGE_DUPLICATES_INSTRUCTION,
    );
    let merge_prompt = format!(
        "{instruction}\n\n\
        ATOM A (source: {source_a}, created: {date_a}):\n{content_a}\n\n\
        ATOM B (source: {source_b}, created: {date_b}):\n{content_b}",
        instruction = instruction,
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


/// Apply a user-edited merge. Caller provides final content; no LLM call.
/// Deletes the loser atom, merges tags into winner, updates winner content.
pub async fn apply_edited_merge(
    core: &AtomicCore,
    winner_id: &str,
    loser_id: &str,
    content: &str,
) -> Result<FixAction, AtomicCoreError> {
    let Some(winner) = core.get_atom(winner_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {winner_id} not found")));
    };
    let Some(loser) = core.get_atom(loser_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {loser_id} not found")));
    };
    if content.trim().is_empty() {
        return Err(AtomicCoreError::Validation("edited content empty".into()));
    }

    let before_state = json!([
        { "id": winner.atom.id, "content": winner.atom.content, "source_url": winner.atom.source_url, "tag_ids": winner.tags.iter().map(|t| t.id.clone()).collect::<Vec<_>>() },
        { "id": loser.atom.id, "content": loser.atom.content, "source_url": loser.atom.source_url, "tag_ids": loser.tags.iter().map(|t| t.id.clone()).collect::<Vec<_>>() },
    ]);

    let loser_tag_ids: Vec<String> = loser.tags.iter().map(|t| t.id.clone()).collect();
    if !loser_tag_ids.is_empty() {
        let _ = core.storage().link_tags_to_atom_impl(&winner.atom.id, &loser_tag_ids).await;
    }

    let upd = crate::UpdateAtomRequest {
        content: content.to_string(),
        source_url: winner.atom.source_url.clone(),
        published_at: None,
        tag_ids: None,
    };
    core.update_atom(&winner.atom.id, upd, |_| {}).await?;
    core.delete_atom(&loser.atom.id).await?;

    let fix_id = audit::log_fix(
        core,
        "content_overlap",
        "merge_with_edited_content",
        "high",
        Some(&[winner.atom.id.clone(), loser.atom.id.clone()]),
        None,
        before_state,
        json!({ "kept_id": winner.atom.id, "deleted_id": loser.atom.id, "content_length": content.len() }),
        None,
        None,
    ).await?;

    Ok(FixAction {
        id: fix_id,
        check: "content_overlap".to_string(),
        action: "merge_with_edited_content".to_string(),
        count: 1,
        details: vec![format!("Kept: {}", winner.atom.id), format!("Deleted: {}", loser.atom.id)],
    })
}

/// Ask the LLM to summarise the conflict between two atoms in one sentence.
pub async fn contradiction_summary(
    core: &AtomicCore,
    atom_a_id: &str,
    atom_b_id: &str,
) -> Result<String, AtomicCoreError> {
    let Some(a) = core.get_atom(atom_a_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_a_id} not found")));
    };
    let Some(b) = core.get_atom(atom_b_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_b_id} not found")));
    };
    let settings = core.get_settings_map().await.unwrap_or_default();
    let instruction = resolve_prompt(
        &settings,
        CONTRADICTION_DETECTION_SETTING_KEY,
        DEFAULT_CONTRADICTION_DETECTION_INSTRUCTION,
    );
    let prompt = format!(
        "{instruction}\n\n\
         ATOM A:\n{}\n\n\
         ATOM B:\n{}\n\n\
         One-sentence summary:",
        a.atom.content, b.atom.content,
        instruction = instruction,
    );
    let provider_config = ProviderConfig::from_settings(&settings);
    let llm = create_llm_provider(&provider_config).map_err(|e| {
        AtomicCoreError::Configuration(format!("LLM provider unavailable: {e}"))
    })?;
    let model = settings.get("chat_model").cloned()
        .or_else(|| settings.get("wiki_model").cloned())
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string());
    let messages = vec![Message::user(prompt)];
    let config = LlmConfig::new(model).with_params(
        crate::providers::types::GenerationParams::new().with_max_tokens(128),
    );
    let response = llm.complete(&messages, &config).await?;
    Ok(response.content.trim().to_string())
}

/// Ask the LLM to strip template boilerplate from an atom, keeping only unique content.
/// Returns the rewritten content. When dry_run=true, no writes happen.
pub async fn strip_boilerplate_atom(
    core: &AtomicCore,
    atom_id: &str,
    dry_run: bool,
) -> Result<(String, Option<FixAction>), AtomicCoreError> {
    let Some(atom) = core.get_atom(atom_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_id} not found")));
    };
    if atom.atom.is_locked {
        return Err(AtomicCoreError::Validation(format!(
            "atom {atom_id} is locked — unlock it before running automated fixes"
        )));
    }
    let settings = core.get_settings_map().await.unwrap_or_default();
    let instruction = resolve_prompt(
        &settings,
        STRIP_BOILERPLATE_SETTING_KEY,
        DEFAULT_STRIP_BOILERPLATE_INSTRUCTION,
    );
    let prompt = format!(
        "{instruction}\n\n\
         NOTE:\n{content}\n\n\
         Rewritten note:",
        instruction = instruction,
        content = atom.atom.content,
    );
    let provider_config = ProviderConfig::from_settings(&settings);
    let llm = create_llm_provider(&provider_config).map_err(|e| {
        AtomicCoreError::Configuration(format!("LLM provider unavailable: {e}"))
    })?;
    let model = settings
        .get("wiki_model")
        .cloned()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string());
    let messages = vec![Message::user(prompt.clone())];
    let config = LlmConfig::new(model).with_params(
        crate::providers::types::GenerationParams::new().with_max_tokens(4096),
    );
    let response = llm.complete(&messages, &config).await?;
    let new_content = response.content.trim().to_string();

    if new_content == "EMPTY" {
        return Err(AtomicCoreError::Validation(
            "LLM reports atom is entirely boilerplate; refusing to clear it".into(),
        ));
    }
    if new_content.is_empty() {
        return Err(AtomicCoreError::Validation("LLM returned empty content".into()));
    }

    if dry_run {
        return Ok((new_content, None));
    }

    let before_state = json!({
        "id": atom.atom.id,
        "content": atom.atom.content,
        "source_url": atom.atom.source_url,
    });
    let upd = crate::UpdateAtomRequest {
        content: new_content.clone(),
        source_url: atom.atom.source_url.clone(),
        published_at: None,
        tag_ids: None,
    };
    core.update_atom(&atom.atom.id, upd, |_| {}).await?;

    let fix_id = audit::log_fix(
        core,
        "boilerplate_pollution",
        "strip_boilerplate",
        "medium",
        Some(std::slice::from_ref(&atom.atom.id)),
        None,
        before_state,
        json!({"new_length": new_content.len()}),
        Some(&prompt),
        Some(&new_content),
    )
    .await?;

    Ok((
        new_content.clone(),
        Some(FixAction {
            id: fix_id,
            check: "boilerplate_pollution".to_string(),
            action: "strip_boilerplate".to_string(),
            count: 1,
            details: vec![format!("Stripped boilerplate from {}", atom.atom.id)],
        }),
    ))
}

// ==================== Broken-link auto-resolution ====================

/// The outcome of an LLM-powered broken-link resolution attempt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AutoResolveOutcome {
    /// The link was rewritten to point at an existing atom.
    Relinked {
        target_atom_id: String,
        title: String,
        confidence: f32,
    },
    /// The link was stripped because no suitable target was found.
    Removed { reason: String },
    /// The LLM was uncertain — link left unchanged.
    Skipped { reason: String },
}

/// Batch result returned by `auto_resolve_all_broken_links`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct AutoResolveBatchResult {
    pub checked: u32,
    pub relinked: u32,
    pub removed: u32,
    pub skipped: u32,
    pub per_atom: Vec<(String, String, AutoResolveOutcome)>,
}

/// Fetch up to `limit` candidate atoms for a broken link query.
/// Extract the display text from a markdown link `[text](href)` or wikilink `[[name]]`.
/// Returns an empty string when the format is unrecognised.
fn extract_link_display_text(original: &str) -> String {
    if let Some(close) = original.find("](")
 {
        if original.starts_with('[') {
            return original[1..close].to_string();
        }
    }
    if let Some(inner) = original.strip_prefix("[[").and_then(|s| s.strip_suffix("]]")) {
        return inner.split('|').next().unwrap_or(inner).trim().to_string();
    }
    String::new()
}

/// Returns `(atom_id, title, source_url, score)`.
pub(crate) async fn suggest_link_targets(
    core: &AtomicCore,
    q: &str,
    limit: i32,
) -> Result<Vec<(String, String, Option<String>, f32)>, AtomicCoreError> {
    core.storage()
        .suggest_atoms_by_query_sync(q.to_string(), limit)
        .await
}

/// Ask the LLM which (if any) candidate is the true target for a broken link,
/// then relink or remove the link accordingly.
pub async fn auto_resolve_broken_link(
    core: &AtomicCore,
    atom_id: &str,
    link_raw: &str,
    link_text: &str,
) -> Result<AutoResolveOutcome, AtomicCoreError> {
    // Locked atoms are not auto-rewritten. Skip without error so batch flows
    // can continue past them.
    if core.is_atom_locked(atom_id).await.unwrap_or(false) {
        return Ok(AutoResolveOutcome::Skipped {
            reason: "atom is locked".to_string(),
        });
    }

    let candidates = suggest_link_targets(core, link_raw, 8).await?;

    if candidates.is_empty() {
        // No candidates — strip the link.
        let reason = "no candidates".to_string();
        let _ = super::fixes::remove_broken_link(core, atom_id, link_raw).await;
        let outcome = AutoResolveOutcome::Removed { reason: reason.clone() };
        audit::log_fix(
            core,
            "broken_internal_links",
            "auto_resolve_removed",
            "medium",
            Some(&[atom_id.to_string()]),
            None,
            json!({"atom_id": atom_id, "link_raw": link_raw}),
            json!({"outcome": "removed", "reason": reason}),
            None,
            None,
        )
        .await?;
        return Ok(outcome);
    }

    // Build candidate list for the prompt.
    let candidate_lines: String = candidates
        .iter()
        .enumerate()
        .map(|(i, (id, title, source_url, _score))| {
            let src = source_url.as_deref().unwrap_or("");
            format!("{n}. id={id} title={title} source={src}", n = i + 1, id = id, title = title, src = src)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "You are resolving a broken markdown link. The source note references a target that no \
longer resolves. Choose the best candidate from the list below OR say NONE if none match.\n\n\
Link text: `{link_text}`\n\
Link href: `{link_raw}`\n\n\
Candidates:\n{candidates}\n\n\
Output JSON only (no markdown fences): {{\"target_atom_id\": \"<id>\" or null, \"confidence\": 0..1, \"reason\": \"...\"}}.",
        link_text = link_text,
        link_raw = link_raw,
        candidates = candidate_lines,
    );

    let settings = core.get_settings_map().await.unwrap_or_default();
    let provider_config = ProviderConfig::from_settings(&settings);
    let llm = create_llm_provider(&provider_config).map_err(|e| {
        AtomicCoreError::Configuration(format!("LLM provider unavailable for auto_resolve: {e}"))
    })?;
    let model = settings
        .get("wiki_model")
        .cloned()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string());
    let messages = vec![Message::user(prompt.clone())];
    let config = LlmConfig::new(model).with_params(
        crate::providers::types::GenerationParams::new().with_max_tokens(512),
    );

    let response = llm.complete(&messages, &config).await?;
    let raw = response.content.trim().to_string();

    let json_str = strip_llm_json_fences(&raw);

    #[derive(serde::Deserialize)]
    struct LlmAnswer {
        target_atom_id: Option<String>,
        confidence: f32,
        reason: String,
    }

    let answer: LlmAnswer = serde_json::from_str(json_str).map_err(|e| {
        AtomicCoreError::Validation(format!("auto_resolve_broken_link: failed to parse LLM JSON: {e} — got: {json_str}"))
    })?;

    let outcome = if let Some(ref target_id) = answer.target_atom_id {
        if answer.confidence >= 0.6 {
            // Relink.
            let _ = super::fixes::relink_broken_link(core, atom_id, link_raw, target_id).await;
            let title = candidates
                .iter()
                .find(|(id, _, _, _)| id == target_id)
                .map(|(_, t, _, _)| t.clone())
                .unwrap_or_else(|| target_id.clone());
            AutoResolveOutcome::Relinked {
                target_atom_id: target_id.clone(),
                title,
                confidence: answer.confidence,
            }
        } else {
            AutoResolveOutcome::Skipped {
                reason: format!("low confidence {:.2}: {}", answer.confidence, answer.reason),
            }
        }
    } else {
        AutoResolveOutcome::Skipped {
            reason: format!("LLM returned null target: {}", answer.reason),
        }
    };

    audit::log_fix(
        core,
        "broken_internal_links",
        "auto_resolve",
        "medium",
        Some(&[atom_id.to_string()]),
        None,
        json!({"atom_id": atom_id, "link_raw": link_raw}),
        serde_json::to_value(&outcome).unwrap_or_default(),
        Some(&prompt),
        Some(&raw),
    )
    .await?;

    Ok(outcome)
}

/// Resolve up to `max` broken (atom, link) pairs using the LLM.
pub async fn auto_resolve_all_broken_links(
    core: &AtomicCore,
    max: usize,
) -> Result<AutoResolveBatchResult, AtomicCoreError> {
    use crate::health::link_resolution::{extract_internal_links};

    let candidates = core.storage().get_link_candidate_atoms_sync().await?;

    // Build a flat list of (atom_id, content, source_url, link_raw, link_text) pairs.
    let mut pairs: Vec<(String, String, String)> = Vec::new(); // (atom_id, link_raw, link_text)

    'outer: for (atom_id, content, source_url) in &candidates {
        let links = extract_internal_links(content, source_url.as_deref());
        for link in &links {
            if pairs.len() >= max {
                break 'outer;
            }
            // Only include unresolved links.
            let candidate_urls: Vec<String> = link.candidate_source_urls.to_vec();
            let url_map = core
                .storage()
                .find_atoms_by_source_urls_sync(candidate_urls)
                .await
                .unwrap_or_default();
            if url_map.is_empty() {
                let link_text = extract_link_display_text(&link.original);
                pairs.push((atom_id.clone(), link.original.clone(), link_text));
            }
        }
    }

    let checked = pairs.len() as u32;
    let mut relinked = 0u32;
    let mut removed = 0u32;
    let mut skipped = 0u32;
    let mut per_atom: Vec<(String, String, AutoResolveOutcome)> = Vec::new();

    for (atom_id, link_raw, link_text) in pairs {
        let outcome = auto_resolve_broken_link(core, &atom_id, &link_raw, &link_text).await?;
        match &outcome {
            AutoResolveOutcome::Relinked { .. } => relinked += 1,
            AutoResolveOutcome::Removed { .. } => removed += 1,
            AutoResolveOutcome::Skipped { .. } => skipped += 1,
        }
        per_atom.push((atom_id, link_raw, outcome));
    }

    Ok(AutoResolveBatchResult {
        checked,
        relinked,
        removed,
        skipped,
        per_atom,
    })
}

/// Ask the LLM whether two atoms flagged as duplicates are a true semantic duplicate
/// or a false positive.  Returns `(is_duplicate, reason)`.
///
/// On false-positive the pair is dismissed under `content_overlap` and the decision
/// is logged via `audit::log_fix`.
pub async fn verify_overlap_pair(
    core: &AtomicCore,
    atom_a_id: &str,
    atom_b_id: &str,
) -> Result<(bool, String), AtomicCoreError> {
    let Some(a) = core.get_atom(atom_a_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_a_id} not found")));
    };
    let Some(b) = core.get_atom(atom_b_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_b_id} not found")));
    };

    let prompt = format!(
        "Below are two knowledge-base notes flagged as possible duplicates. \
Determine whether they cover substantially the same subject or are coincidentally \
similar (different topics, similar vocabulary). Reply with STRICT JSON: \
{{\"duplicate\": true|false, \"reason\": \"one short sentence\"}}. Nothing else.\n\n\
ATOM A (source: {source_a}, created: {date_a}):\n{content_a}\n\n\
ATOM B (source: {source_b}, created: {date_b}):\n{content_b}",
        source_a = a.atom.source_url.as_deref().unwrap_or("manual"),
        date_a = a.atom.created_at,
        content_a = a.atom.content,
        source_b = b.atom.source_url.as_deref().unwrap_or("manual"),
        date_b = b.atom.created_at,
        content_b = b.atom.content,
    );

    let settings = core.get_settings_map().await.unwrap_or_default();
    let provider_config = ProviderConfig::from_settings(&settings);
    let llm = create_llm_provider(&provider_config).map_err(|e| {
        AtomicCoreError::Configuration(format!("LLM provider unavailable: {e}"))
    })?;
    let model = settings
        .get("wiki_model")
        .cloned()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string());
    let messages = vec![Message::user(prompt.clone())];
    let config = LlmConfig::new(model).with_params(
        crate::providers::types::GenerationParams::new().with_max_tokens(256),
    );
    let response = llm.complete(&messages, &config).await?;
    let raw = response.content.trim();

    #[derive(serde::Deserialize)]
    struct VerifyOverlapResp { duplicate: bool, reason: String }
    let parsed: VerifyOverlapResp = serde_json::from_str(strip_llm_json_fences(raw)).map_err(|e| {
        AtomicCoreError::Validation(format!("LLM response parse error: {e} — raw: {raw}"))
    })?;

    if !parsed.duplicate {
        let key = crate::health::pair_key(atom_a_id, atom_b_id);
        let _ = core
            .dismiss_health_item("content_overlap", &key, "llm_false_positive", None)
            .await;
        let _ = audit::log_fix(
            core,
            "content_overlap",
            "verify_with_llm",
            "low",
            Some(&[atom_a_id.to_string(), atom_b_id.to_string()]),
            None,
            json!({"atom_a": atom_a_id, "atom_b": atom_b_id}),
            json!({"is_duplicate": false, "reason": parsed.reason.clone()}),
            Some(&prompt),
            Some(raw),
        )
        .await;
    }

    Ok((parsed.duplicate, parsed.reason))
}

/// Ask the LLM whether two atoms flagged as contradicting actually assert conflicting
/// facts.  Returns `(is_real, reason)`.
///
/// On not-real: dismisses under `contradiction_detection` with reason `llm_false_positive`.
pub async fn verify_contradiction_pair(
    core: &AtomicCore,
    atom_a_id: &str,
    atom_b_id: &str,
) -> Result<(bool, String), AtomicCoreError> {
    let Some(a) = core.get_atom(atom_a_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_a_id} not found")));
    };
    let Some(b) = core.get_atom(atom_b_id).await? else {
        return Err(AtomicCoreError::NotFound(format!("atom {atom_b_id} not found")));
    };

    let prompt = format!(
        "Two knowledge base atoms have been flagged as possibly contradicting each other. \
Reply with STRICT JSON: {{\"contradiction\": true|false, \"reason\": \"one short sentence\"}}. \
Contradiction means the two atoms assert directly conflicting facts about the same subject. \
Nothing else.\n\n\
ATOM A (source: {source_a}):\n{content_a}\n\n\
ATOM B (source: {source_b}):\n{content_b}",
        source_a = a.atom.source_url.as_deref().unwrap_or("manual"),
        content_a = a.atom.content,
        source_b = b.atom.source_url.as_deref().unwrap_or("manual"),
        content_b = b.atom.content,
    );

    let settings = core.get_settings_map().await.unwrap_or_default();
    let provider_config = ProviderConfig::from_settings(&settings);
    let llm = create_llm_provider(&provider_config).map_err(|e| {
        AtomicCoreError::Configuration(format!("LLM provider unavailable: {e}"))
    })?;
    let model = settings
        .get("wiki_model")
        .cloned()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string());
    let messages = vec![Message::user(prompt.clone())];
    let config = LlmConfig::new(model).with_params(
        crate::providers::types::GenerationParams::new().with_max_tokens(256),
    );
    let response = llm.complete(&messages, &config).await?;
    let raw = response.content.trim();

    #[derive(serde::Deserialize)]
    struct VerifyContradictionResp { contradiction: bool, reason: String }
    let parsed: VerifyContradictionResp = serde_json::from_str(strip_llm_json_fences(raw)).map_err(|e| {
        AtomicCoreError::Validation(format!("LLM response parse error: {e} — raw: {raw}"))
    })?;

    if !parsed.contradiction {
        let key = crate::health::pair_key(atom_a_id, atom_b_id);
        let _ = core
            .dismiss_health_item("contradiction_detection", &key, "llm_false_positive", None)
            .await;
        let _ = audit::log_fix(
            core,
            "contradiction_detection",
            "verify_with_llm",
            "low",
            Some(&[atom_a_id.to_string(), atom_b_id.to_string()]),
            None,
            json!({"atom_a": atom_a_id, "atom_b": atom_b_id}),
            json!({"is_contradiction": false, "reason": parsed.reason.clone()}),
            Some(&prompt),
            Some(raw),
        )
        .await;
    }

    Ok((parsed.contradiction, parsed.reason))
}

/// LLM-reconcile two contradicting atoms into one document that acknowledges the
/// disagreement, records both positions, and prefers the more recent/authoritative
/// source where clear.
///
/// Writes merged content to the newer atom, deletes the older atom, and dismisses
/// the pair under `contradiction_detection`.
pub async fn merge_contradicting_pair(
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
    if atom_a.atom.is_locked || atom_b.atom.is_locked {
        return Err(AtomicCoreError::Validation(
            "one or both atoms are locked — unlock before auto-merging. Contradictions in locked source material should stay recorded as-is.".to_string()
        ));
    }

    // Newer atom is the keeper
    let (keep, delete) = if atom_a.atom.updated_at >= atom_b.atom.updated_at {
        (atom_a, atom_b)
    } else {
        (atom_b, atom_a)
    };

    let merge_prompt = format!(
        "The two atoms contradict each other. Produce ONE reconciled document that \
acknowledges the disagreement and records both positions with attribution (date + source). \
Prefer the more recent/authoritative source where clear. Use clean markdown. \
Do not add commentary beyond the reconciled document.\n\n\
ATOM A (source: {source_a}, created: {date_a}):\n{content_a}\n\n\
ATOM B (source: {source_b}, created: {date_b}):\n{content_b}\n\n\
Reconciled document:",
        source_a = keep.atom.source_url.as_deref().unwrap_or("manual"),
        date_a = keep.atom.created_at,
        content_a = keep.atom.content,
        source_b = delete.atom.source_url.as_deref().unwrap_or("manual"),
        date_b = delete.atom.created_at,
        content_b = delete.atom.content,
    );

    if dry_run {
        return Ok(Some(FixAction {
            id: "dry_run".to_string(),
            check: "contradiction_detection".to_string(),
            action: "merge_with_llm".to_string(),
            count: 1,
            details: vec![format!("Would merge {} into {}", delete.atom.id, keep.atom.id)],
        }));
    }

    let settings = core.get_settings_map().await.unwrap_or_default();
    let provider_config = ProviderConfig::from_settings(&settings);
    let llm = create_llm_provider(&provider_config).map_err(|e| {
        AtomicCoreError::Configuration(format!("LLM provider unavailable: {e}"))
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
    let merged_content = response.content.trim().to_string();

    if merged_content.is_empty() {
        return Err(AtomicCoreError::Validation(
            "LLM returned empty merged content".to_string(),
        ));
    }

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

    // Update keeper with reconciled content
    let upd = crate::UpdateAtomRequest {
        content: merged_content.clone(),
        source_url: keep.atom.source_url.clone(),
        published_at: None,
        tag_ids: None,
    };
    core.update_atom(&keep.atom.id, upd, |_| {}).await?;
    core.delete_atom(&delete.atom.id).await?;

    // Dismiss the pair
    let pair_key = crate::health::pair_key(atom_a_id, atom_b_id);
    let _ = core
        .dismiss_health_item("contradiction_detection", &pair_key, "merged", None)
        .await;

    let fix_id = audit::log_fix(
        core,
        "contradiction_detection",
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
        "contradiction pair merged with LLM"
    );

    Ok(Some(FixAction {
        id: fix_id,
        check: "contradiction_detection".to_string(),
        action: "merge_with_llm".to_string(),
        count: 1,
        details: vec![
            format!("Kept: {}", keep.atom.id),
            format!("Deleted: {}", delete.atom.id),
        ],
    }))
}

// ==================== Tag structure proposal ====================

/// Raw LLM output shape — parsed before enriching with the DB-generated UUID.
#[derive(serde::Deserialize)]
struct RawTagProposalResponse {
    summary: String,
    actions: Vec<crate::health::TagProposalAction>,
}

/// Ask the LLM to propose merges, renames, reparentings and deletions for the
/// current tag tree.  The proposal is persisted in `tag_proposals` and returned.
pub async fn propose_tag_restructure(
    core: &AtomicCore,
) -> Result<crate::health::TagProposal, AtomicCoreError> {
    // 1. Load flat tag list.
    let tags = core.get_all_tags_filtered(0).await?;

    // 2. Build compact JSON capped at 500 tags.
    let mut tag_rows: Vec<serde_json::Value> = tags
        .iter()
        .take(500)
        .map(|t| {
            json!({
                "id":         t.tag.id,
                "name":       t.tag.name,
                "parent_id":  t.tag.parent_id,
                "atom_count": t.atom_count,
            })
        })
        .collect();
    // Sort by atom_count desc so the most relevant tags appear first in the cap.
    tag_rows.sort_by(|a, b| {
        let ca = a["atom_count"].as_i64().unwrap_or(0);
        let cb = b["atom_count"].as_i64().unwrap_or(0);
        cb.cmp(&ca)
    });
    let tag_tree_json = serde_json::to_string_pretty(&tag_rows)
        .unwrap_or_else(|_| "[]".to_string());

    // 3. Build prompt.
    let prompt = format!(
        "You are a knowledge-base curator.  Analyse the tag tree below and propose a \
better organisation.\n\n\
Rules:\n\
- Propose MERGES for near-duplicate tag names (same concept, different spelling or casing).\n\
- Propose RENAMES for tags whose names are unclear or inconsistent.\n\
- Propose REPARENTINGS for orphan tags or tags placed under the wrong parent.\n\
- Propose DELETIONS only for single-use, irrelevant, or clearly erroneous tags.\n\
- Limit total actions to 25.\n\
- Every action must include a human-readable `reason`.\n\n\
Output STRICT JSON only (no markdown fences, no commentary) matching this schema:\n\
{{\"summary\": \"<one paragraph rationale>\", \"actions\": [<action>, ...]}}\n\n\
Each action is one of:\n\
  {{\"kind\":\"merge\", \"from_id\":\"<id>\", \"into_id\":\"<id>\", \"from_name\":\"<name>\", \"into_name\":\"<name>\", \"reason\":\"...\"}}\n\
  {{\"kind\":\"rename\", \"tag_id\":\"<id>\", \"old_name\":\"<name>\", \"new_name\":\"<name>\", \"reason\":\"...\"}}\n\
  {{\"kind\":\"reparent\", \"tag_id\":\"<id>\", \"tag_name\":\"<name>\", \"new_parent_id\":null|\"<id>\", \"new_parent_name\":null|\"<name>\", \"reason\":\"...\"}}\n\
  {{\"kind\":\"delete\", \"tag_id\":\"<id>\", \"tag_name\":\"<name>\", \"reason\":\"...\"}}\n\n\
Current tag tree ({count} tags):\n{tree}",
        count = tag_rows.len(),
        tree = tag_tree_json,
    );

    // 4. Call LLM.
    let settings = core.get_settings_map().await.unwrap_or_default();
    let provider_config = ProviderConfig::from_settings(&settings);
    let llm = create_llm_provider(&provider_config).map_err(|e| {
        AtomicCoreError::Configuration(format!("LLM provider unavailable for tag proposal: {e}"))
    })?;
    let model = settings
        .get("wiki_model")
        .cloned()
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string());
    let messages = vec![Message::user(prompt.clone())];
    let config = LlmConfig::new(model).with_params(
        crate::providers::types::GenerationParams::new().with_max_tokens(4096),
    );
    let response = llm.complete(&messages, &config).await?;

    // 5. Parse.
    let raw: RawTagProposalResponse = serde_json::from_str(strip_llm_json_fences(&response.content)).map_err(|e| {
        AtomicCoreError::Validation(format!(
            "LLM returned unparseable proposal: {e}. Raw: {}",
            &response.content[..response.content.len().min(200)]
        ))
    })?;

    let proposal = crate::health::TagProposal {
        id: uuid::Uuid::new_v4().to_string(),
        summary: raw.summary,
        actions: raw.actions,
        generated_at: chrono::Utc::now().to_rfc3339(),
    };

    // 6. Persist.
    core.storage().save_tag_proposal_sync(proposal.clone()).await?;

    Ok(proposal)
}

/// Apply accepted actions from a persisted proposal.
pub async fn apply_tag_proposal(
    core: &AtomicCore,
    proposal_id: &str,
    accepted_indices: &[usize],
) -> Result<Vec<FixAction>, AtomicCoreError> {
    let proposal = core
        .storage()
        .get_tag_proposal_sync(proposal_id)
        .await?
        .ok_or_else(|| AtomicCoreError::NotFound(format!("tag proposal {proposal_id} not found")))?;

    let mut fix_actions = Vec::new();

    for &idx in accepted_indices {
        let action = proposal.actions.get(idx).ok_or_else(|| {
            AtomicCoreError::Validation(format!("accepted index {idx} out of range"))
        })?;

        match action {
            crate::health::TagProposalAction::Merge {
                from_id,
                into_id,
                from_name,
                into_name,
                reason,
            } => {
                let merge = crate::compaction::TagMerge {
                    winner_name: into_name.clone(),
                    loser_name: from_name.clone(),
                    reason: reason.clone(),
                };
                let result = core.apply_tag_merges(&[merge]).await;
                let detail = match &result {
                    Ok(r) => format!("Merged '{}' into '{}': {} atoms retagged", from_name, into_name, r.atoms_retagged),
                    Err(e) => format!("Merge '{}' into '{}' failed: {e}", from_name, into_name),
                };
                let fix_id = audit::log_fix(
                    core, "tag_health", "tag_proposal_merge", "medium",
                    None,
                    Some(&[from_id.clone(), into_id.clone()]),
                    json!({"from_id": from_id, "from_name": from_name}),
                    json!({"into_id": into_id, "into_name": into_name, "detail": detail}),
                    None, None,
                ).await?;
                fix_actions.push(FixAction {
                    id: fix_id,
                    check: "tag_health".to_string(),
                    action: "tag_proposal_merge".to_string(),
                    count: 1,
                    details: vec![detail],
                });
            }
            crate::health::TagProposalAction::Rename {
                tag_id,
                old_name,
                new_name,
                reason,
            } => {
                // Fetch current parent so we only change the name.
                let current = core.storage().get_tag_by_id_sync(tag_id).await?;
                let parent_id = current.as_ref().and_then(|(_, p)| p.as_deref().map(|s| s.to_string()));
                let result = core.update_tag(tag_id, new_name, parent_id.as_deref()).await;
                let detail = match &result {
                    Ok(_) => format!("Renamed '{}' → '{}'", old_name, new_name),
                    Err(e) => format!("Rename '{}' failed: {e}", old_name),
                };
                let fix_id = audit::log_fix(
                    core, "tag_health", "tag_proposal_rename", "low",
                    None, Some(std::slice::from_ref(&tag_id)),
                    json!({"tag_id": tag_id, "old_name": old_name}),
                    json!({"new_name": new_name, "reason": reason, "detail": detail}),
                    None, None,
                ).await?;
                fix_actions.push(FixAction {
                    id: fix_id,
                    check: "tag_health".to_string(),
                    action: "tag_proposal_rename".to_string(),
                    count: 1,
                    details: vec![detail],
                });
            }
            crate::health::TagProposalAction::Reparent {
                tag_id,
                tag_name,
                new_parent_id,
                reason,
                ..
            } => {
                let result = core.update_tag(tag_id, tag_name, new_parent_id.as_deref()).await;
                let detail = match &result {
                    Ok(_) => format!("Reparented '{}' → parent {:?}", tag_name, new_parent_id),
                    Err(e) => format!("Reparent '{}' failed: {e}", tag_name),
                };
                let fix_id = audit::log_fix(
                    core, "tag_health", "tag_proposal_reparent", "low",
                    None, Some(std::slice::from_ref(&tag_id)),
                    json!({"tag_id": tag_id, "tag_name": tag_name}),
                    json!({"new_parent_id": new_parent_id, "reason": reason, "detail": detail}),
                    None, None,
                ).await?;
                fix_actions.push(FixAction {
                    id: fix_id,
                    check: "tag_health".to_string(),
                    action: "tag_proposal_reparent".to_string(),
                    count: 1,
                    details: vec![detail],
                });
            }
            crate::health::TagProposalAction::Delete { tag_id, tag_name, reason } => {
                let result = core.delete_tag(tag_id, false).await;
                let detail = match &result {
                    Ok(_) => format!("Deleted tag '{}'", tag_name),
                    Err(e) => format!("Delete '{}' failed: {e}", tag_name),
                };
                let fix_id = audit::log_fix(
                    core, "tag_health", "tag_proposal_delete", "medium",
                    None, Some(std::slice::from_ref(&tag_id)),
                    json!({"tag_id": tag_id, "tag_name": tag_name}),
                    json!({"reason": reason, "detail": detail}),
                    None, None,
                ).await?;
                fix_actions.push(FixAction {
                    id: fix_id,
                    check: "tag_health".to_string(),
                    action: "tag_proposal_delete".to_string(),
                    count: 1,
                    details: vec![detail],
                });
            }
        }
    }

    // Mark proposal applied.
    core.storage().mark_tag_proposal_applied_sync(proposal_id).await?;

    Ok(fix_actions)
}