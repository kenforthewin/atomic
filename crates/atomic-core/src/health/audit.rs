//! Fix audit log and undo capability.
//!
//! Every auto-fix action is recorded in `health_fix_log` with a JSON
//! `before_state` snapshot.  `undo_fix` reads that snapshot and restores the
//! affected atoms / tags.

use crate::error::AtomicCoreError;
use crate::AtomicCore;
use serde::{Deserialize, Serialize};

/// A persisted record of one fix action (stored in `health_fix_log`).
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthFixLog {
    pub id: String,
    pub check_name: String,
    pub action: String,
    /// "safe" | "low" | "medium" | "high"
    pub tier: String,
    /// JSON array of atom IDs touched by this fix.
    pub atom_ids: Option<Vec<String>>,
    /// JSON array of tag IDs touched by this fix.
    pub tag_ids: Option<Vec<String>>,
    /// Full JSON snapshot before the fix was applied.  Used by undo.
    pub before_state: String,
    /// Full JSON snapshot after the fix was applied.
    pub after_state: String,
    pub llm_prompt: Option<String>,
    pub llm_response: Option<String>,
    pub executed_at: String,
    pub undone_at: Option<String>,
}

/// Lightweight record stored in `health_reports`.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredHealthReport {
    pub id: String,
    pub computed_at: String,
    pub overall_score: u32,
    /// JSON map check_name → score.
    pub check_scores: String,
    pub atom_count: i32,
    pub auto_fixes_applied: i32,
    /// Full serialised `HealthReport`.
    pub report_json: String,
}

/// Record a fix action in the audit log, returning the generated `id`.
pub async fn log_fix(
    core: &AtomicCore,
    check_name: &str,
    action: &str,
    tier: &str,
    atom_ids: Option<&[String]>,
    tag_ids: Option<&[String]>,
    before_state: serde_json::Value,
    after_state: serde_json::Value,
    llm_prompt: Option<&str>,
    llm_response: Option<&str>,
) -> Result<String, AtomicCoreError> {
    let id = uuid::Uuid::new_v4().to_string();
    let log = HealthFixLog {
        id: id.clone(),
        check_name: check_name.to_string(),
        action: action.to_string(),
        tier: tier.to_string(),
        atom_ids: atom_ids.map(|ids| ids.to_vec()),
        tag_ids: tag_ids.map(|ids| ids.to_vec()),
        before_state: serde_json::to_string(&before_state).unwrap_or_default(),
        after_state: serde_json::to_string(&after_state).unwrap_or_default(),
        llm_prompt: llm_prompt.map(|s| s.to_string()),
        llm_response: llm_response.map(|s| s.to_string()),
        executed_at: chrono::Utc::now().to_rfc3339(),
        undone_at: None,
    };
    core.storage().log_fix_action_sync(&log).await?;
    Ok(id)
}

/// Undo a previously applied fix using the stored `before_state` snapshot.
///
/// Currently supports:
/// - Recreating deleted atoms (JSON array of `{id, content, source_url, tags}`)
/// - Recreating deleted tags (JSON array of `{id, name, parent_id}`)
/// - Restoring updated atom content (JSON array of `{id, content}`)
pub async fn undo(core: &AtomicCore, fix_id: &str) -> Result<(), AtomicCoreError> {
    let log = core
        .storage()
        .get_fix_log_sync(fix_id)
        .await?
        .ok_or_else(|| {
            AtomicCoreError::NotFound(format!("health fix log {fix_id} not found"))
        })?;

    if log.undone_at.is_some() {
        return Err(AtomicCoreError::Validation(format!(
            "fix {fix_id} has already been undone"
        )));
    }

    let before: serde_json::Value = serde_json::from_str(&log.before_state)
        .unwrap_or(serde_json::json!({}));

    match log.action.as_str() {
        "deleted_tags" => {
            // before_state: [ { "id": "...", "name": "...", "parent_id": null } ]
            if let Some(tags) = before.as_array() {
                for tag in tags {
                    let name = tag
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let parent_id = tag.get("parent_id").and_then(|v| v.as_str());
                    // Re-create tag (ID won't be preserved, but name + parent will match)
                    if !name.is_empty() {
                        let _ = core.storage().create_tag_impl(name, parent_id).await;
                    }
                }
            }
        }
        "deleted_atoms" => {
            // before_state: [ { "id": "...", "content": "...", "source_url": null, "tag_ids": [...] } ]
            if let Some(atoms) = before.as_array() {
                for atom_snap in atoms {
                    let content = atom_snap
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let source_url = atom_snap
                        .get("source_url")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let tag_ids: Vec<String> = atom_snap
                        .get("tag_ids")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let req = crate::CreateAtomRequest {
                        content,
                        source_url,
                        tag_ids,
                        ..Default::default()
                    };
                    let _ = core.create_atom(req, |_| {}).await;
                }
            }
        }
        "updated_atoms" => {
            // before_state: [ { "id": "...", "content": "...", "source_url": null } ]
            if let Some(atoms) = before.as_array() {
                for atom_snap in atoms {
                    let id = atom_snap
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let content = atom_snap
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let source_url = atom_snap
                        .get("source_url")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if !id.is_empty() {
                        let upd = crate::UpdateAtomRequest {
                            content,
                            source_url,
                            published_at: None,
                            tag_ids: None,
                        };
                        let _ = core.update_atom(&id, upd, |_| {}).await;
                    }
                }
            }
        }
        _ => {
            tracing::warn!(action = %log.action, "undo not implemented for this action type");
        }
    }

    // Mark fix as undone
    core.storage().mark_fix_undone_sync(fix_id).await?;
    Ok(())
}

/// Fetch recent fix log entries (most recent first).
pub async fn get_recent_fixes(
    core: &AtomicCore,
    limit: i32,
) -> Result<Vec<HealthFixLog>, AtomicCoreError> {
    core.storage().get_recent_fixes_sync(limit).await
}
