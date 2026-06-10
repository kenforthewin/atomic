//! Shared test fixtures for the Atomic workspace.
//!
//! Only useful as a `dev-dependencies` entry. The module exists so we don't
//! maintain two copies of the wiremock-backed OpenAI-compat mock between
//! `atomic-core/tests/support/mod.rs` and `atomic-server/tests/support/mod.rs`
//! (and, eventually, atomic-cloud's e2e suite).
//!
//! ## What lives here
//!
//! - `MockAiServer`: a wiremock-backed server that responds to
//!   `/v1/embeddings` and `/v1/chat/completions` with deterministic output.
//! - `EMBED_DIM` and `EDGE_SIMILARITY_THRESHOLD`: shared constants pinned
//!   to the default provider/embedding-pipeline config so tests don't drift.
//! - `truncate_postgres_for_test`: ready-made truncation helper for the
//!   per-DB tables used by integration tests. Behind the `postgres`
//!   feature so default consumers don't pull sqlx.
//!
//! ## What does NOT live here
//!
//! Anything that depends on `atomic_core` or `atomic_server` — including
//! `Backend` enums, `TestCtx`, app factories, or pipeline pollers. Those
//! types reference concrete `AtomicCore` / `AppState` shapes that differ
//! between crates, and bringing them into this lib would force a circular
//! workspace dep on atomic-core. They live in the consuming crate's
//! `tests/support/mod.rs`.

pub mod mock_ai;
pub mod mock_url;

#[cfg(feature = "postgres")]
pub mod postgres_helpers;

pub use mock_ai::{MockAiServer, EDGE_SIMILARITY_THRESHOLD, EMBED_DIM};
pub use mock_url::MockUrlServer;

#[cfg(feature = "postgres")]
pub use postgres_helpers::truncate_postgres_for_test;
