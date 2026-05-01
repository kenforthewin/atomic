//! Daily briefing generation.
//!
//! Produces a short 2-3 paragraph summary of atoms added to the knowledge
//! base since the last briefing run. The heavy lifting is in [`agentic`],
//! which mirrors the wiki agentic strategy — an LLM with a tiny tool set
//! curates context, then writes the final briefing in a tool-free pass.
//!
//! Public entry point is [`run_briefing`]. Storage lives in new `briefings`
//! and `briefing_citations` tables (migration v12). See also
//! [`crate::scheduler`] for the task that invokes this on a timer.

pub mod agentic;
pub mod task;

pub use task::DailyBriefingTask;

use crate::error::AtomicCoreError;
use crate::AtomicCore;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A generated daily briefing. Mirrors the shape of `WikiArticle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Briefing {
    pub id: String,
    pub content: String,
    pub created_at: String,
    /// Number of atoms that were visible to the agent for this run. May be
    /// less than the total number of atoms added in the period if the run
    /// hit the 100-atom cap.
    pub atom_count: i32,
    /// Timestamp of the previous briefing run (or the seeded "7 days ago"
    /// on the first run). Used by clients to show "N new atoms since X".
    pub last_run_at: String,
}

/// A single citation attached to a briefing. The `source_url` field is
/// populated by a JOIN on read and is not stored on the row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BriefingCitation {
    pub id: String,
    pub briefing_id: String,
    pub citation_index: i32,
    pub atom_id: String,
    pub excerpt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

/// A briefing joined with its citations — the primary read shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BriefingWithCitations {
    pub briefing: Briefing,
    pub citations: Vec<BriefingCitation>,
}

/// Bounded number of new atoms sent to the LLM on a single run. Prevents
/// long context explosions while still giving the agent substantial material
/// on busy days.
const MAX_NEW_ATOMS: usize = 100;

/// Generate a daily briefing for all atoms created after `since`.
///
/// Flow:
/// 1. Fetch up to [`MAX_NEW_ATOMS`] atoms with `created_at > since`,
///    ordered by `created_at DESC`.
/// 2. If zero atoms: return `Ok(None)` — nothing is persisted. The caller
///    still bumps `last_run` so the scheduler waits the full interval before
///    re-polling. This keeps the UI from showing a stub "Nothing new since
///    ..." briefing when the knowledge base is quiet.
/// 3. Otherwise: run the agentic loop over the new atoms, synthesize the
///    briefing, extract `[N]` citations, persist, and return `Ok(Some(_))`.
pub async fn run_briefing(
    core: &AtomicCore,
    since: DateTime<Utc>,
) -> Result<Option<BriefingWithCitations>, AtomicCoreError> {
    let since_str = since.to_rfc3339();
    tracing::info!(since = %since_str, "[briefing] Starting daily briefing run");

    let new_atoms = core
        .storage()
        .list_new_atoms_since_sync(&since_str, MAX_NEW_ATOMS as i32)
        .await?;
    let total_new = core
        .storage()
        .count_new_atoms_since_sync(&since_str)
        .await?;

    tracing::info!(
        visible = new_atoms.len(),
        total = total_new,
        "[briefing] Fetched new atoms"
    );

    // Zero-atom fast path: skip the LLM call and do not persist anything.
    // The caller updates `last_run` so we don't rescan the same empty window.
    if new_atoms.is_empty() {
        tracing::info!("[briefing] No new atoms — skipping briefing creation");
        return Ok(None);
    }

    // Run the agent loop.
    let (mut content, citations) = agentic::generate(core, &since, &new_atoms, total_new)
        .await
        .map_err(AtomicCoreError::Wiki)?;

    // Append health summary when score is concerning
    if let Ok(report) = crate::health::compute_health(core).await {
        if report.overall_score < 85 {
            let health_section = format!(
                "\n\n## Knowledge Health\n\n\
                Your knowledge base health score is **{}/100** ({}).\n\n\
                {} issues can be auto-fixed via the dashboard.",
                report.overall_score,
                report.overall_status,
                report.auto_fixable
            );
            content.push_str(&health_section);
        }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let briefing = Briefing {
        id: id.clone(),
        content,
        created_at: now.clone(),
        atom_count: new_atoms.len() as i32,
        last_run_at: since_str.clone(),
    };

    // Materialize citations with the briefing_id.
    let citations: Vec<BriefingCitation> = citations
        .into_iter()
        .map(|(index, atom_id, excerpt)| BriefingCitation {
            id: uuid::Uuid::new_v4().to_string(),
            briefing_id: id.clone(),
            citation_index: index,
            atom_id,
            excerpt,
            source_url: None,
        })
        .collect();

    let saved = core
        .storage()
        .insert_briefing_sync(&briefing, &citations)
        .await?;
    tracing::info!(
        briefing_id = %saved.briefing.id,
        citations = saved.citations.len(),
        "[briefing] Saved briefing"
    );
    Ok(Some(saved))
}
