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

pub mod account_cache;
pub mod account_plane;
pub mod auth;
pub mod control_plane;
pub mod curated_models;
pub mod email;
pub mod error;
pub mod keyvault;
pub mod magic_links;
pub mod managed_keys;
pub mod provider_config;
pub mod provider_credentials;
pub mod provision;
pub mod provisioning_api;
pub mod rate_limit;
pub mod reaper;
pub mod reserved_subdomains;
pub mod server;
pub mod tenant_plane;
pub mod tokens;

pub use account_cache::{AccountCache, AccountCacheConfig, TenantHandle};
pub use account_plane::{
    AccountPlane, AccountPlaneConfig, RateLimits, DEFAULT_MAX_CONCURRENT_PROVISIONS, SESSION_TTL,
};
pub use auth::{AuthPrincipal, CloudAuth, CredentialSource, ResolvedTenant, SESSION_COOKIE};
pub use control_plane::ControlPlane;
pub use curated_models::{
    merge_managed_model_config, validate_managed_model_config, MANAGED_EMBEDDING_MODEL,
    MANAGED_LLM_MODELS, PINNED_EMBEDDING_DIMENSION,
};
pub use email::{EmailSender, LogSender, MailgunSender};
pub use error::CloudError;
pub use keyvault::{EnvMasterKeyVault, KeyVault, SecretKey, ENCRYPTION_VERSION, MASTER_KEY_ENV};
pub use magic_links::{
    consume_magic_link, issue_magic_link, MagicLinkPurpose, MagicLinkRecord, MAGIC_LINK_TTL,
};
pub use managed_keys::{
    default_managed_model_config, ManagedKeyConfig, ManagedKeys, DEFAULT_MONTHLY_ALLOWANCE_CENTS,
};
pub use provider_config::{
    build_provider_config, config_for_credentials, keyless_provider_config,
    validate_byok_model_config, BYOK_ALLOWED_KEYS,
};
pub use provider_credentials::{
    delete_credentials, get_active_credentials, get_active_provider_state, get_credentials,
    insert_credentials_if_absent, record_validation, set_active_provider, touch_last_used,
    update_model_config, upsert_credentials, ActiveProviderState, CredentialOrigin, NewCredentials,
    Provider, ProviderCredentials,
};
pub use provision::{
    delete_account, provision_account, tenant_db_account_id, tenant_db_name, ClusterConfig,
    NewAccount, ProvisionedAccount,
};
pub use provisioning_api::{
    CreatedRuntimeKey, OpenRouterProvisioning, ProvisioningApi, RuntimeKeyUsage,
    DEFAULT_OPENROUTER_PROVISIONING_URL, PROVISIONING_KEY_ENV,
};
pub use reaper::{reaper_lock_key, run_reaper_pass, ReaperPolicy, ReaperSummary};
pub use server::{cloud_plane_guard, configure_cloud_app, FallbackAppState};
pub use tenant_plane::TenantPlane;
pub use tokens::{
    create_session, issue_token, verify_session, verify_token, SessionRecord, TokenRecord,
    TokenScope,
};
