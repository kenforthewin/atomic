//! Knowledge-base health monitoring and auto-remediation.
//!
//! This module computes a scored health report across 10+ checks, each
//! targeting a distinct class of data-quality issue. Deterministic fixes
//! (orphan-tag deletion, retry pipelines, graph rebuild) run automatically
//! at "safe" or "low" tier. LLM-powered fixes (merge duplicates, enrich
//! stubs, structure content) are available but always logged and undoable.
//!
//! # Layout
//! | module               | role                                              |
//! |----------------------|---------------------------------------------------|
//! | [`compute`]          | `compute_health`, `compute_single_check`, dismiss |
//! | [`run_fix`]          | auto-fix orchestrator                             |
//! | [`checks`]           | individual sync check implementations             |
//! | [`fixes`]            | deterministic fix implementations                 |
//! | [`llm_fixes`]        | LLM-powered fixes (merge, enrich, reorg)          |
//! | [`custom`]           | user-defined custom rules                         |
//! | [`audit`]            | health_fix_log read/write                         |
//! | [`score`]            | weighted aggregation                              |
//! | [`types`]            | public data types                                 |
//! | [`link_resolution`]  | wikilink + markdown link parsing                  |
//! | [`task`], [`gc_task`]| background scheduled jobs                         |
//!
//! # Flow
//! 1. `compute_health(core)` → runs all checks → returns `HealthReport`
//! 2. `run_fix(core, req)` → applies fixes by tier → returns `FixResponse`
//! 3. `undo_fix(core, fix_id)` → restores pre-fix state from audit log

pub mod audit;
pub mod checks;
pub mod compute;
pub mod custom;
pub mod fixes;
pub mod gc_task;
pub mod link_resolution;
pub mod llm_fixes;
pub mod run_fix;
pub mod score;
pub mod task;
pub mod types;

// Re-export the public surface so existing callers
// (`use crate::health::HealthReport`) keep working.
pub use compute::{compute_health, compute_single_check};
pub use run_fix::run_fix;
pub use score::aggregate_score;
pub use types::{
    AtomPreview, BoilerplateAtomEntry, ContradictionAtom, ContradictionPairEntry,
    DuplicatePair, FixAction, FixRequest, FixResponse, FixTier, HealthCheckOverride,
    HealthCheckResult, HealthConfig, HealthReport, HealthStatus, HealthThresholds,
    RootlessTagEntry, SingleAtomTagEntry, SkippedFix, TagProposal, TagProposalAction,
    WikiGap, WikiStaleEntry,
};

// Internal cross-module references.
pub(crate) use compute::{apply_dismissals, title_preview};

/// Build a stable item key for a pair. Sorts atom IDs lexicographically so
/// key ordering is independent of which atom is A vs B.
pub fn pair_key(a: &str, b: &str) -> String {
    if a <= b {
        format!("{}__{}", a, b)
    } else {
        format!("{}__{}", b, a)
    }
}

#[cfg(test)]
mod tests;
