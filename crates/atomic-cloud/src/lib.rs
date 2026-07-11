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
pub mod admin;
pub mod auth;
pub mod backpressure;
pub mod backup;
pub mod backup_store;
pub mod backups;
pub mod billing;
pub mod billing_guard;
pub mod billing_routes;
pub mod chat_streams;
pub mod control_plane;
pub mod curated_models;
pub mod deploy;
pub mod dispatch_hints;
pub mod dispatcher;
pub mod email;
pub mod error;
pub mod fleet_migration;
pub mod keyvault;
pub mod magic_links;
pub mod managed_keys;
pub mod oauth_routes;
pub mod oauth_store;
pub mod plans;
pub mod pools;
pub mod provider_config;
pub mod provider_credentials;
pub mod provision;
pub mod provisioning_api;
pub mod quota;
pub mod quota_usage;
pub mod rate_limit;
pub mod reaper;
pub mod reserved_subdomains;
pub mod server;
pub mod spa;
pub mod tenant_plane;
pub mod tokens;

pub use account_cache::{AccountCache, AccountCacheConfig, TenantHandle};
pub use account_plane::{
    AccountPlane, AccountPlaneConfig, RateLimits, TrialPolicy, DEFAULT_MAX_CONCURRENT_PROVISIONS,
    SESSION_TTL,
};
pub use auth::{AuthPrincipal, CloudAuth, CredentialSource, ResolvedTenant, SESSION_COOKIE};
pub use backpressure::{
    ai_interactive_route, out_of_credits_guard, provider_failure_policy, BreakerConfig, PauseKind,
    ProviderBreaker, ProviderPause, DEFAULT_RETRY_AFTER_CAP,
};
pub use backup::{
    backup_tools_available, dump_control_database, dump_tenant_database, restore_database,
    DumpConnection, DEFAULT_BACKUP_TIMEOUT, DUMP_STDERR_MAX_LEN,
};
pub use backup_store::{BackupStore, LocalFileSystemStore, S3Config, S3Store};
pub use backups::{
    dumps_for_account, final_dump_before_delete, finalize_abandoned_backup_runs, finish_backup_run,
    list_active_tenant_databases, recent_backup_runs, record_backup_failure, record_backup_success,
    run_backup_pass, stale_tenant_backups, start_backup_run, tenant_backup_status,
    tenant_database_exists, BackupConfig, BackupPolicy, BackupRunRecord, BackupSummary,
    BackupTarget, StaleBackup, TenantBackupStatus, DEFAULT_BACKUP_RUN_ABANDON_AFTER,
    DEFAULT_MAX_BACKUPS_PER_PASS, DEFAULT_STALENESS_HORIZON,
};
pub use billing::dunning::{
    advance_dunning, advance_dunning_with, advance_expired_trials, apply_payment_failed,
    apply_payment_failed_on_conn, apply_payment_succeeded, apply_payment_succeeded_on_conn,
    apply_subscription_deleted, apply_subscription_deleted_on_conn, apply_subscription_event,
    apply_subscription_event_on_conn, billing_state_from_column, claim_webhook_event,
    claim_webhook_event_on_conn, expired_trials, finish_expired_trial, link_stripe_customer,
    claim_trial_warnings, reconcile_managed_key_limit, release_webhook_event, reset_trial_warning,
    start_trial, BillingState, DunningAdvance, DunningThresholds, TrialAdvance, TrialDowngraded,
    TrialWarning, DEFAULT_DUNNING_SWEEP_INTERVAL, DEFAULT_TRIAL_DAYS,
    DEFAULT_TRIAL_WARNING_LEAD_DAYS, READ_ONLY_AFTER_DAYS, SUSPENDED_AFTER_DAYS,
};
pub use billing::trial_emails::{notify_trial_downgrades, send_trial_warnings};
pub use billing::{
    now_unix, parse_event, verify_webhook, BillingProvider, StripeClient, StripeSession,
    SubscriptionState, WebhookEvent, STRIPE_SECRET_KEY_ENV, STRIPE_WEBHOOK_SECRET_ENV,
    WEBHOOK_TOLERANCE_SECS,
};
pub use billing_guard::billing_write_guard;
pub use billing_routes::{Billing, BillingConfig};
pub use chat_streams::{
    chat_stream_guard, chat_stream_route, ChatStreamLimiter, ChatStreamPermit,
    DEFAULT_CHAT_STREAMS_PER_ACCOUNT,
};
pub use control_plane::ControlPlane;
pub use curated_models::{
    agentic_models_for_plan, merge_managed_model_config, validate_managed_model_config,
    DEFAULT_AGENTIC_MODEL, FREE_AGENTIC_MODELS, MANAGED_EMBEDDING_MODEL, MANAGED_TAGGING_MODEL,
    PINNED_EMBEDDING_DIMENSION, PRO_AGENTIC_MODELS,
};
pub use deploy::{
    abandoned_run_threshold, advance_deploy, deploy_run_status, evaluate_policy,
    finalize_abandoned_runs, finish_deploy_run, latest_deploy_run, run_fleet_gate,
    start_deploy_run, AdvanceOutcome, DeployPolicy, DeployRun, DeployStatus, Readiness,
};
pub use dispatch_hints::{
    clear_hint_if_older, list_active_account_ids, list_hinted_accounts, mark_hint, DispatchHint,
};
pub use dispatcher::{
    CoreExecutor, Dispatcher, DispatcherConfig, ExecOutcome, TenantQueue, TickOutcome,
    WorkExecutor, WorkItem, PROVIDER_BACKOFF_REASON, RATE_LIMIT_REQUEUE_DELAY,
};
pub use email::{BillingNotice, EmailSender, LogSender, MailgunSender};
pub use error::CloudError;
pub use fleet_migration::{
    list_failed_migrations, list_retryable_failures, list_unmigrated, migration_backoff_horizon,
    record_migration_failure, record_migration_success, tenant_schema_target,
    FailedTenantMigration, FleetMigrationConfig, FleetMigrator, FleetRunOutcome, UnmigratedTenant,
    MIGRATION_ERROR_MAX_LEN,
};
pub use keyvault::{EnvMasterKeyVault, KeyVault, SecretKey, ENCRYPTION_VERSION, MASTER_KEY_ENV};
pub use magic_links::{
    consume_magic_link, issue_magic_link, MagicLinkPurpose, MagicLinkRecord, MAGIC_LINK_TTL,
};
pub use managed_keys::{
    default_managed_model_config, ManagedKeyConfig, ManagedKeys, DEFAULT_MONTHLY_ALLOWANCE_CENTS,
};
pub use oauth_routes::OAuthPlane;
pub use oauth_store::{
    consume_oauth_code, create_oauth_client, get_oauth_client, insert_oauth_code,
    purge_expired_oauth_codes, record_oauth_code_token, NewOAuthCode, OAuthClient, OAuthCode,
    OAUTH_CLIENT_PREFIX, OAUTH_CODE_PREFIX, OAUTH_CODE_TTL,
};
pub use plans::{Plan, PlanRegistry, DEFAULT_PLAN_ID};
pub use pools::{PoolCaps, PoolPermit, WorkClass, WorkTypeCap, WorkerPools, WorkerPoolsConfig};
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
    DeleteLock, NewAccount, ProvisionedAccount,
};
pub use provisioning_api::{
    CreatedRuntimeKey, OpenRouterProvisioning, ProvisioningApi, RuntimeKeyUsage,
    DEFAULT_OPENROUTER_PROVISIONING_URL, PROVISIONING_KEY_ENV,
};
pub use quota::{account_over_plan_limits, quota_guard};
pub use quota_usage::{
    current_period_start, recompute_storage, roll_over_period, storage_state_from_column,
    StoragePolicy, StorageRecompute, StorageState, DEFAULT_PERIOD_ROLLOVER_INTERVAL,
    DEFAULT_STORAGE_RECOMPUTE_INTERVAL, ROLLED_OVER_METRICS, STORAGE_BYTES_METRIC,
};
pub use rate_limit::{
    data_plane_rate_limit_guard, DataPlaneLimit, DataPlaneRateLimiter, DataPlaneRateLimits,
};
pub use reaper::{reaper_lock_key, run_reaper_pass, ReaperPolicy, ReaperSummary};
pub use server::{
    cloud_plane_guard, configure_cloud_app, FallbackAppState, QuotaBilling,
    DEFAULT_MCP_SSE_KEEP_ALIVE,
};
pub use spa::SpaServer;
pub use tenant_plane::TenantPlane;
pub use tokens::{
    create_session, issue_token, verify_session, verify_token, SessionRecord, TokenRecord,
    TokenScope,
};
