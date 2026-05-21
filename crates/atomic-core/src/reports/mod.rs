//! Reports primitive — scheduled, durable research runs that emit finding
//! atoms (`kind = 'report'`) with structured provenance and citations.
//!
//! See `docs/plans/reports.md` for the full design. Phase 2 lands the
//! schema, the runner, and the scheduled-execution loop alongside the
//! existing daily-briefing path; phase 3 collapses the briefing onto this
//! abstraction.

pub mod agentic;
pub mod runner;
pub mod schedule;
pub mod scope;

pub use runner::{run_report, RunOutcome};
