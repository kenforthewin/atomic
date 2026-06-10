//! Atomic Cloud — multi-tenant hosting composition layer.
//!
//! This crate turns the single-tenant [`atomic-server`](atomic_server) into a
//! multi-tenant cloud deployment, per `docs/plans/atomic-cloud.md`. The
//! architecture is composition, not modification:
//!
//! - **One tenant = one Postgres database** (`acct_<uuid>`) on a shared
//!   cluster, running atomic-core's existing tenant migrations. Knowledge
//!   bases (`db_id`) remain the user-facing organizational unit *inside* a
//!   tenant database.
//! - A separate **control-plane database** (default `atomic_cloud_control`,
//!   see [`control_plane`]) holds accounts, tenant-database mappings, tokens,
//!   sessions, and subdomain reservations.
//! - The cloud binary composes `atomic-server`'s route registration under
//!   cloud middleware that resolves `Host` subdomain → account → tenant
//!   `DatabaseManager`, injected via request extensions. The dependency
//!   arrow is strictly one-way: `atomic-cloud → atomic-server → atomic-core`;
//!   neither lower crate contains any cloud-aware code.
//!
//! An earlier, never-shipped Fly machine-per-customer prototype previously
//! lived in this crate (last at commit `4b44c51`). Its architecture is
//! superseded wholesale, but it remains the parts bin for later slices:
//! the magic-link flow, Mailgun and Stripe clients, and the signup frontend
//! are salvageable from git history.

pub mod control_plane;
pub mod error;
pub mod reserved_subdomains;

pub use control_plane::ControlPlane;
pub use error::CloudError;
