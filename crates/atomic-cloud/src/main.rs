//! Atomic Cloud — binary entry point.
//!
//! Subcommands:
//!
//! - `serve` — run the composed multi-tenant server (see [`atomic_cloud::server`]),
//!   applying pending control-plane migrations on boot.
//! - `migrate` — apply pending control-plane migrations and exit.
//! - `account create` / `account delete` — operator-side provisioning, the
//!   manual path until the signup slice lands the HTTP flow.
//! - `token create` — mint a control-plane API token for an account.
//!
//! Every command takes `--control-url` (or `ATOMIC_CLOUD_CONTROL_URL`) before
//! the subcommand and runs migrations first, so any command works against a
//! fresh cluster.
//!
//! # Master key
//!
//! `serve` additionally requires the provider-credential master key (env
//! var named by `--master-key-env`, default `ATOMIC_CLOUD_MASTER_KEY`) and
//! refuses to boot without a valid one — provider features are integral to
//! serving (managed keys are provisioned at signup; BYOK settings routes
//! decrypt stored keys), so a missing/malformed key is a deployment error
//! best surfaced at boot, not on the first signup. The other subcommands
//! (`migrate`, `account`, `token`) deliberately never load the vault — or
//! the OpenRouter provisioning key — so operator tooling stays runnable
//! from hosts that hold neither production secret. The trade-off is that
//! `account create` provisions keyless accounts and `account delete` cannot
//! delete a managed runtime key (it logs the residue loudly with the
//! `external_key_id`; the master OpenRouter account's key listing is the
//! cleanup path). The HTTP routes, which run inside `serve`, do both
//! properly — prefer them.

use std::sync::Arc;

use actix_web::{web, App, HttpServer};
use atomic_cloud::account_over_plan_limits;
use atomic_cloud::{
    abandoned_run_threshold, advance_deploy, advance_dunning_with, advance_expired_trials,
    configure_cloud_app, delete_account, finalize_abandoned_runs, issue_token, latest_deploy_run,
    list_failed_migrations, list_unmigrated, provision_account, recompute_storage,
    roll_over_period, run_fleet_gate, tenant_schema_target, AccountCache, AccountCacheConfig,
    AccountPlane, AccountPlaneConfig, AdvanceOutcome, Billing, BillingConfig, ChatStreamLimiter,
    CloudAuth, ClusterConfig, ControlPlane, DataPlaneRateLimiter, DataPlaneRateLimits,
    DeployPolicy, Dispatcher, DispatcherConfig, EmailSender, EnvMasterKeyVault, FallbackAppState,
    FleetMigrationConfig, KeyVault, LogSender, MailgunSender, ManagedKeyConfig, ManagedKeys,
    NewAccount, OpenRouterProvisioning, PlanRegistry, PoolCaps, QuotaBilling, RateLimits,
    Readiness, TenantPlane, TokenScope, WorkerPoolsConfig, DEFAULT_DUNNING_SWEEP_INTERVAL,
    DEFAULT_PLAN_ID,
};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use std::collections::HashMap;

#[derive(Parser)]
#[command(name = "atomic-cloud", about = "Atomic Cloud multi-tenant server")]
struct Cli {
    /// Postgres URL of the control-plane database. When the URL omits a
    /// database name, `atomic_cloud_control` is used.
    #[arg(long, env = "ATOMIC_CLOUD_CONTROL_URL")]
    control_url: String,

    #[command(subcommand)]
    command: Command,
}

/// Where tenant databases live. Shared by every subcommand that touches a
/// tenant database (`serve`, `account create`, `account delete`).
#[derive(Args)]
struct ClusterArgs {
    /// Postgres URL of the shared tenant cluster. The database path
    /// component is replaced per tenant; the user must be able to
    /// CREATE/DROP DATABASE.
    #[arg(long, env = "ATOMIC_CLOUD_CLUSTER_URL")]
    cluster_url: String,

    /// Identifier recorded on account_databases rows, for the future
    /// shard split. v1 runs a single cluster.
    #[arg(long, env = "ATOMIC_CLOUD_CLUSTER_ID", default_value = "primary")]
    cluster_id: String,
}

impl ClusterArgs {
    fn into_config(self) -> ClusterConfig {
        ClusterConfig {
            cluster_id: self.cluster_id,
            cluster_url: self.cluster_url,
        }
    }
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // One Command exists per process; Serve is just every serve knob.
enum Command {
    /// Start the multi-tenant HTTP server.
    Serve {
        #[command(flatten)]
        cluster: ClusterArgs,

        /// Base domain accounts are hosted under: requests to
        /// `<subdomain>.<base-domain>` route to the matching account.
        #[arg(long, env = "ATOMIC_CLOUD_BASE_DOMAIN")]
        base_domain: String,

        /// Port to listen on.
        #[arg(long, default_value_t = 8080)]
        port: u16,

        /// Address to bind to.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Max connections in each cached tenant's Postgres pool. Every
        /// active account holds its own pool, so keep this small (the plan
        /// budgets ~5 per tenant behind pgbouncer).
        #[arg(
            long,
            env = "ATOMIC_CLOUD_TENANT_POOL_MAX_CONNECTIONS",
            default_value_t = 5
        )]
        tenant_pool_max_connections: u32,

        /// Close a tenant pool's connections after this many seconds idle,
        /// so quiet-but-cached accounts release connections back to the
        /// cluster before their cache entry is evicted.
        #[arg(
            long,
            env = "ATOMIC_CLOUD_TENANT_POOL_IDLE_TIMEOUT_SECS",
            default_value_t = 300
        )]
        tenant_pool_idle_timeout_secs: u64,

        /// Max connections in the control-plane pool. This single pool fronts
        /// both the auth path (every request runs ≥2 control queries) and all
        /// of serve's background loops, so under concurrency it must be sized
        /// for the pod's request volume — too small and the 10s acquire
        /// timeout surfaces as spurious 500s to healthy tenants. Keep below
        /// the cluster's per-pod connection budget (pgbouncer fronts it in
        /// production).
        #[arg(
            long,
            env = "ATOMIC_CLOUD_CONTROL_POOL_MAX_CONNECTIONS",
            default_value_t = atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS
        )]
        control_pool_max_connections: u32,

        /// How often the periodic idle sweep of the account cache runs, in
        /// seconds. Defaults to a quarter of the cache idle TTL.
        #[arg(long, env = "ATOMIC_CLOUD_CACHE_SWEEP_INTERVAL_SECS")]
        cache_sweep_interval_secs: Option<u64>,

        /// How often the failure-recovery reaper runs, in seconds (plan:
        /// "Failure recovery & the reaper"). Each pass resumes or rolls
        /// back stuck provisions, reclaims orphaned tenant databases,
        /// clears self-reservations, and purges expired links/sessions/
        /// reservations. Per-account advisory locks make concurrent passes
        /// across pods safe. `0` disables the reaper entirely — dev only, for
        /// a box whose tenant cluster is SHARED with the test suite, where
        /// orphan-reclaim would otherwise drop the suite's `acct_*` databases.
        #[arg(long, env = "ATOMIC_CLOUD_REAPER_INTERVAL_SECS", default_value_t = 60)]
        reaper_interval_secs: u64,

        #[command(flatten)]
        email: EmailArgs,

        /// Name of the environment variable holding the master key that
        /// encrypts provider credentials at rest (32 bytes, hex or
        /// base64). The key VALUE is only ever read from the environment —
        /// argv leaks into process listings. serve fails at boot without a
        /// valid key; see atomic_cloud::keyvault for the custody runbook
        /// (loss of this key = unrecoverable credentials).
        #[arg(long, default_value = atomic_cloud::MASTER_KEY_ENV)]
        master_key_env: String,

        /// Derive the client IP for rate limiting from `X-Forwarded-For`
        /// (rightmost entry — the one appended by your proxy) instead of
        /// the connection peer address. Enable when, and ONLY when, a
        /// trusted reverse proxy fronts this process: without one, clients
        /// can spoof the header and sidestep per-IP limits; with one but
        /// this flag off, every client shares the proxy's bucket.
        #[arg(long, env = "ATOMIC_CLOUD_TRUST_PROXY_HEADER")]
        trust_proxy_header: bool,

        /// Public origin used when building emailed magic links, e.g.
        /// `https://app.atomic.cloud`. Defaults to `https://app.<base-domain>`;
        /// override for local/dev deployments with ports or plain http.
        /// Post-signup/login redirects to tenant subdomains reuse its
        /// scheme and port.
        #[arg(long, env = "ATOMIC_CLOUD_APP_PUBLIC_URL")]
        app_public_url: Option<String>,

        /// DEV ONLY. Drop the `Secure` attribute from the session cookie so it
        /// works over plain HTTP on a non-`localhost` host (e.g. a headless
        /// dev box reached over Tailscale). NEVER set this in production — it
        /// allows the session cookie to travel over unencrypted connections.
        /// Default off (the cookie is `Secure`). Boot warns loudly when set.
        #[arg(long, env = "ATOMIC_CLOUD_DANGEROUSLY_INSECURE_COOKIES")]
        dangerously_insecure_cookies: bool,

        /// Directory holding the built account-plane SPA (`npm run build` →
        /// `dist`) to serve as the cloud server's fallback route — the
        /// signup/login pages and the authenticated `/account/*` dashboard.
        /// Defaults to `crates/atomic-cloud/frontend/dist` relative to the
        /// repo root; if that directory has no `index.html` (a pure-API pod,
        /// or a dev run that hasn't built the frontend), the SPA fallback is
        /// simply absent and unmatched paths 404.
        #[arg(
            long,
            env = "ATOMIC_CLOUD_SPA_DIR",
            default_value = "crates/atomic-cloud/frontend/dist"
        )]
        spa_dir: std::path::PathBuf,

        /// Optional directory holding the built PRODUCT app (`npm run
        /// build:web` → `dist-web`) to serve at the tenant root, so the
        /// dashboard's "Open knowledge base" link lands on the real product
        /// app on the same origin. Mainly a local/dev convenience — in
        /// production a reverse proxy serves the product app at the tenant
        /// root. Unset (the default) means the tenant root falls back to the
        /// account dashboard.
        #[arg(long, env = "ATOMIC_CLOUD_PRODUCT_DIR")]
        product_dir: Option<std::path::PathBuf>,

        /// Max signups provisioning concurrently in this process; further
        /// signup completions get a 503 + Retry-After without consuming
        /// their link (the plan budgets 4-8).
        #[arg(
            long,
            env = "ATOMIC_CLOUD_MAX_CONCURRENT_PROVISIONS",
            default_value_t = atomic_cloud::DEFAULT_MAX_CONCURRENT_PROVISIONS
        )]
        max_concurrent_provisions: usize,

        #[command(flatten)]
        provisioning: ProvisioningArgs,

        #[command(flatten)]
        billing: BillingArgs,

        #[command(flatten)]
        quota: QuotaArgs,

        #[command(flatten)]
        dispatcher: DispatcherArgs,

        #[command(flatten)]
        fleet: FleetArgs,

        /// Max concurrent streaming-chat requests per account in this
        /// process (plan: streaming chat is not pooled — a per-tenant
        /// semaphore at the route caps it). Over-cap chat sends get a
        /// structured 429 with a Retry-After hint. Independent of
        /// --dispatcher: the cap guards the interactive route either way.
        #[arg(
            long,
            env = "ATOMIC_CLOUD_CHAT_STREAMS_PER_ACCOUNT",
            default_value_t = atomic_cloud::DEFAULT_CHAT_STREAMS_PER_ACCOUNT
        )]
        chat_streams_per_account: usize,

        #[command(flatten)]
        backup: BackupArgs,

        /// How often the nightly backup pass runs, in seconds (plan:
        /// "Backups & disaster recovery" → nightly logical dumps). Each pass
        /// dumps every active tenant database (per-tenant advisory-locked,
        /// cross-pod safe) plus the control plane to the configured backup
        /// store. The default is 24h; a jittered start keeps a fleet of pods
        /// from synchronizing their passes.
        #[arg(
            long,
            env = "ATOMIC_CLOUD_BACKUP_INTERVAL_SECS",
            default_value_t = 24 * 60 * 60
        )]
        backup_interval_secs: u64,

        /// Max tenant dumps per nightly pass before deferring the rest to the
        /// next pass (each shells out to pg_dump). Stale-first ordering means
        /// a capped pass makes progress on the most-overdue tenants.
        #[arg(
            long,
            env = "ATOMIC_CLOUD_MAX_BACKUPS_PER_PASS",
            default_value_t = atomic_cloud::DEFAULT_MAX_BACKUPS_PER_PASS
        )]
        max_backups_per_pass: usize,

        /// Staleness alert horizon in seconds (plan: ">36h old"). After each
        /// nightly pass, any active tenant whose last successful backup is
        /// older than this is logged at error level — the
        /// "unmonitored-backup-job-is-a-placebo" alert.
        #[arg(
            long,
            env = "ATOMIC_CLOUD_BACKUP_STALENESS_SECS",
            default_value_t = atomic_cloud::DEFAULT_STALENESS_HORIZON.as_secs()
        )]
        backup_staleness_secs: u64,

        /// Per-`pg_dump`/`pg_restore` wall-clock budget in seconds
        /// (adversarial-review issue 1). A child that overruns this is killed
        /// and the dump records a typed timeout failure rather than hanging the
        /// serial nightly pass forever. The whole-pass worst case is bounded by
        /// this × the per-pass cap. Default 30 minutes.
        #[arg(
            long,
            env = "ATOMIC_CLOUD_BACKUP_TIMEOUT_SECS",
            default_value_t = atomic_cloud::DEFAULT_BACKUP_TIMEOUT.as_secs()
        )]
        backup_timeout_secs: u64,
    },

    /// Connect to the control plane (creating the database if it doesn't
    /// exist) and apply pending migrations.
    Migrate,

    /// Manage accounts.
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },

    /// Manage control-plane API tokens.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },

    /// Inspect and acknowledge boot-time fleet migrations (deploy gating).
    Deploy {
        #[command(subcommand)]
        action: DeployAction,
    },

    /// Logical-backup operations (plan: "Backups & disaster recovery"):
    /// `run` one pass now, `status` (per-tenant freshness + stale tenants +
    /// recent runs), `list` a tenant's dumps, and `restore` a dump into a
    /// fresh database (the runbook).
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
}

/// How magic-link emails leave the process.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum EmailMode {
    /// Write the link to the server log at info level instead of sending
    /// anything (dev mode — the log is the delivery channel).
    Log,
    /// Send real email via the Mailgun REST API; links are never logged.
    Mailgun,
}

/// Default name of the environment variable holding the Mailgun API key.
/// Matches the master-key / provisioning-key / Stripe-secret custody rule:
/// `serve` takes only the variable NAME on argv and reads the secret VALUE
/// from the environment, so the key never surfaces in process listings.
const MAILGUN_API_KEY_ENV: &str = "ATOMIC_CLOUD_MAILGUN_API_KEY";

/// Email delivery selection for `serve`. Mailgun mode requires all three
/// credentials; the API key is env-only by convention (it's a secret).
#[derive(Args)]
struct EmailArgs {
    /// Email delivery mode for magic links.
    #[arg(long, env = "ATOMIC_CLOUD_EMAIL_MODE", default_value = "log")]
    email_mode: EmailMode,

    /// Name of the environment variable holding the Mailgun API key
    /// (required with --email-mode mailgun). The key VALUE is only ever read
    /// from the environment — argv leaks into process listings (`ps`,
    /// `/proc/<pid>/cmdline`).
    #[arg(long, default_value = MAILGUN_API_KEY_ENV)]
    mailgun_api_key_env: String,

    /// DEPRECATED, INSECURE. Pass the Mailgun API key VALUE directly on
    /// argv. The key leaks into process listings; use --mailgun-api-key-env
    /// (the default reads `ATOMIC_CLOUD_MAILGUN_API_KEY` from the
    /// environment) instead. Boot warns loudly when set.
    #[arg(long, hide_env_values = true)]
    mailgun_api_key: Option<String>,

    /// Mailgun sending domain, e.g. `mg.atomic.cloud` (required with
    /// --email-mode mailgun).
    #[arg(long, env = "ATOMIC_CLOUD_MAILGUN_DOMAIN")]
    mailgun_domain: Option<String>,

    /// From address for magic-link email, e.g.
    /// `Atomic <no-reply@mg.atomic.cloud>` (required with --email-mode
    /// mailgun).
    #[arg(long, env = "ATOMIC_CLOUD_MAILGUN_FROM")]
    mailgun_from: Option<String>,
}

/// How managed provider keys are provisioned at signup (plan: "Provider
/// management" → "Managed key lifecycle").
#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProvisioningMode {
    /// No managed keys (dev default): signup skips the key step entirely
    /// and accounts run keyless until a provider is configured. Account
    /// deletion logs loudly if it encounters managed-key residue it cannot
    /// delete.
    Disabled,
    /// Mint a per-account OpenRouter runtime key at signup, with the
    /// configured monthly allowance and native monthly reset. Requires the
    /// provisioning key in the environment.
    Openrouter,
}

/// Managed-key provisioning selection for `serve`. The provisioning key is
/// env-only by convention — it can mint runtime keys against the master
/// OpenRouter account's balance (crown-jewel custody; see
/// `atomic_cloud::provisioning_api`), and argv leaks into process listings.
#[derive(Args)]
struct ProvisioningArgs {
    /// Managed provider-key provisioning mode.
    #[arg(
        long = "provisioning-mode",
        env = "ATOMIC_CLOUD_PROVISIONING_MODE",
        default_value = "disabled"
    )]
    provisioning_mode: ProvisioningMode,

    /// Name of the environment variable holding the OpenRouter provisioning
    /// key (required with --provisioning-mode openrouter). The key VALUE is
    /// only ever read from the environment.
    #[arg(long, default_value = atomic_cloud::PROVISIONING_KEY_ENV)]
    openrouter_provisioning_key_env: String,

    /// Base URL of the OpenRouter provisioning API. Override for proxies or
    /// test servers speaking the same API.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_OPENROUTER_PROVISIONING_URL",
        default_value = atomic_cloud::DEFAULT_OPENROUTER_PROVISIONING_URL
    )]
    openrouter_provisioning_url: String,

    /// Monthly credit allowance per managed key, in cents, enforced
    /// provider-side with native monthly reset (the plan's free-tier
    /// placeholder is 50 = $0.50/mo; the billing slice derives this from
    /// the account's plan instead).
    #[arg(
        long,
        env = "ATOMIC_CLOUD_MANAGED_KEY_ALLOWANCE_CENTS",
        default_value_t = atomic_cloud::DEFAULT_MONTHLY_ALLOWANCE_CENTS
    )]
    managed_key_allowance_cents: u32,
}

impl ProvisioningArgs {
    /// Build the managed-key handle, erroring (at boot, never on the first
    /// signup) when openrouter mode is missing its provisioning key. Takes
    /// the boot-validated vault; `Disabled` drops it — the boot contract
    /// ("stored credentials are decryptable") holds either way.
    fn into_managed_keys(
        self,
        vault: Arc<dyn KeyVault>,
    ) -> Result<ManagedKeys, Box<dyn std::error::Error>> {
        match self.provisioning_mode {
            ProvisioningMode::Disabled => {
                tracing::warn!(
                    "managed-key provisioning is disabled: new accounts are \
                     created without AI provider credentials"
                );
                Ok(ManagedKeys::Disabled)
            }
            ProvisioningMode::Openrouter => {
                let api = OpenRouterProvisioning::from_env(
                    &self.openrouter_provisioning_url,
                    &self.openrouter_provisioning_key_env,
                )?;
                Ok(ManagedKeys::Enabled {
                    api: Arc::new(api),
                    vault,
                    config: ManagedKeyConfig {
                        monthly_allowance_cents: self.managed_key_allowance_cents,
                        ..ManagedKeyConfig::default()
                    },
                })
            }
        }
    }
}

/// Where logical backups land (plan: "Backups & disaster recovery"). `local`
/// is the dev/self-hosted default — a directory tree, always available, never
/// network — and `s3` targets S3 or any S3-compatible endpoint via
/// [`object_store`]. S3 access-key/secret are read from the **environment**
/// (the standard `AWS_*` vars), never argv.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackupStoreKind {
    /// A local directory tree under `--backup-base-dir`.
    Local,
    /// An S3 / S3-compatible bucket.
    S3,
}

/// Backup-store selection, shared by `serve` (nightly + final dumps),
/// `account delete` (the final dump), and `backup restore`. Defaults to a
/// local store under `./backups`, so a fresh dev deployment has working
/// backups with no flags.
#[derive(Args, Clone)]
struct BackupArgs {
    /// Backup object-store backend.
    #[arg(long, env = "ATOMIC_CLOUD_BACKUP_STORE", default_value = "local")]
    backup_store: BackupStoreKind,

    /// Base directory for `--backup-store local`.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_BACKUP_BASE_DIR",
        default_value = "./backups"
    )]
    backup_base_dir: String,

    /// Bucket name for `--backup-store s3` (required in that mode).
    #[arg(long, env = "ATOMIC_CLOUD_BACKUP_BUCKET")]
    backup_bucket: Option<String>,

    /// AWS region for `--backup-store s3`. S3-compatible endpoints usually
    /// accept `us-east-1`/`auto`.
    #[arg(long, env = "ATOMIC_CLOUD_BACKUP_REGION", default_value = "us-east-1")]
    backup_region: String,

    /// Override endpoint for an S3-compatible provider (R2/MinIO). Omit for
    /// AWS S3 proper.
    #[arg(long, env = "ATOMIC_CLOUD_BACKUP_ENDPOINT")]
    backup_endpoint: Option<String>,

    /// Optional key prefix prepended to every backup object key, for operators
    /// sharing one bucket across deployments (`--backup-store s3`). Composes in
    /// front of the `backups/` layout (e.g. `prod` → `prod/backups/<date>/...`).
    /// Ignored by the local store. Omit for the bare `backups/...` layout.
    #[arg(long, env = "ATOMIC_CLOUD_BACKUP_PREFIX")]
    backup_prefix: Option<String>,
}

impl BackupArgs {
    /// Build the configured backup store. S3 credentials come from the
    /// environment (never argv); a misconfigured S3 selection (missing
    /// bucket) fails at boot, not on the first dump.
    fn into_store(self) -> Result<Arc<dyn atomic_cloud::BackupStore>, Box<dyn std::error::Error>> {
        match self.backup_store {
            BackupStoreKind::Local => Ok(Arc::new(atomic_cloud::LocalFileSystemStore::new(
                self.backup_base_dir,
            ))),
            BackupStoreKind::S3 => {
                let bucket = self
                    .backup_bucket
                    .ok_or("--backup-bucket is required with --backup-store s3")?;
                let store = atomic_cloud::S3Store::new(&atomic_cloud::S3Config {
                    bucket,
                    region: self.backup_region,
                    endpoint: self.backup_endpoint,
                    prefix: self.backup_prefix,
                })?;
                Ok(Arc::new(store))
            }
        }
    }
}

/// Stripe billing selection for `serve` (plan: "Billing"). Billing is
/// OPTIONAL: with the secret-key env var unset, the billing routes degrade to
/// a structured 503 and the dunning sweep simply never has past_due accounts
/// to advance — fine for dev clusters and self-hosted-style deployments.
///
/// The secret key and webhook signing secret follow the same custody rule as
/// the master key and the OpenRouter provisioning key: `serve` takes only the
/// environment-variable NAME on argv and reads the VALUE from the environment
/// (`into_config`). The secret itself never appears in argv, so it can't leak
/// into process listings (`ps`, `/proc/<pid>/cmdline`).
#[derive(Args)]
struct BillingArgs {
    /// Name of the environment variable holding the Stripe secret key
    /// (`sk_…`). Billing is enabled when that variable is set; the key VALUE
    /// is only ever read from the environment, never taken on argv.
    #[arg(long, default_value = atomic_cloud::STRIPE_SECRET_KEY_ENV)]
    stripe_secret_key_env: String,

    /// Name of the environment variable holding the Stripe webhook signing
    /// secret (`whsec_…`), used to verify the `app.<base>/billing/webhook`
    /// signature. Required for the webhook to accept anything; the secret
    /// VALUE is only ever read from the environment, never taken on argv.
    #[arg(long, default_value = atomic_cloud::STRIPE_WEBHOOK_SECRET_ENV)]
    stripe_webhook_secret_env: String,

    /// Plan→price mapping as repeatable `plan_id=stripe_price_id` pairs, e.g.
    /// `--stripe-price pro=price_123`. The reverse map drives webhook
    /// price→plan resolution (a price's `metadata.plan_id` takes precedence
    /// when present).
    #[arg(
        long = "stripe-price",
        env = "ATOMIC_CLOUD_STRIPE_PRICES",
        value_delimiter = ','
    )]
    stripe_prices: Vec<String>,
}

/// Read a secret VALUE from the named environment variable, treating an unset
/// OR empty variable as "absent" (`None`). Used for the optional Stripe
/// credentials: the variable NAME comes from argv, the secret never does, so
/// it can't surface in process listings. An empty value is deliberately
/// folded into `None` so that exporting `VAR=` reads as "billing disabled"
/// rather than enabling billing with an empty key.
fn read_secret_env(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

/// Whether the configured base domain looks like a local/dev host, used to
/// scope boot warnings to deployments that are plausibly production. Treats
/// `localhost` and any `*.localhost` subdomain as local (the dev/test
/// convention this crate already uses for cookies and host matching).
fn base_domain_is_localhost(base_domain: &str) -> bool {
    let host = base_domain.split(':').next().unwrap_or(base_domain);
    host == "localhost" || host.ends_with(".localhost")
}

/// Whether a Postgres connection URL asks for a TLS-negotiating `sslmode`
/// (`require` / `verify-ca` / `verify-full`). A missing or weaker mode
/// (`disable` / `allow` / `prefer`) can silently fall back to plaintext. Used
/// only for a boot-time advisory — sqlx performs the actual TLS handshake from
/// the URL; this just inspects the operator's stated intent.
fn pg_url_requires_tls(pg_url: &str) -> bool {
    url::Url::parse(pg_url)
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "sslmode")
                .map(|(_, v)| matches!(v.as_ref(), "require" | "verify-ca" | "verify-full"))
        })
        .unwrap_or(false)
}

impl BillingArgs {
    /// Parse the `plan=price` pairs into a map.
    fn plan_prices(&self) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
        let mut map = HashMap::new();
        for pair in &self.stripe_prices {
            let (plan, price) = pair
                .split_once('=')
                .ok_or_else(|| format!("--stripe-price expects plan=price, got {pair:?}"))?;
            map.insert(plan.to_string(), price.to_string());
        }
        Ok(map)
    }

    fn into_config(
        self,
        base_domain: String,
        app_public_url: Option<String>,
    ) -> Result<BillingConfig, Box<dyn std::error::Error>> {
        let plan_prices = self.plan_prices()?;
        Ok(BillingConfig {
            stripe_secret_key: read_secret_env(&self.stripe_secret_key_env),
            webhook_secret: read_secret_env(&self.stripe_webhook_secret_env),
            plan_prices,
            app_public_url,
            base_domain,
            stripe_base_url: None,
        })
    }
}

/// Quota period-rollover, storage enforcement, and dunning-threshold knobs
/// for `serve` (plan: "Observability, quotas, billing" → "Quotas" period
/// rollover + storage recompute; "Billing" → dunning ladder). Every value
/// defaults to the plan's number, so a deployment that sets none of these
/// gets the plan's behavior verbatim.
#[derive(Args)]
struct QuotaArgs {
    /// How often the period-rollover job runs, in seconds (plan: "A
    /// 1-hour-cadence job inserts new `period_start` rows"). Each run opens
    /// the current month's `quota_usage` rows for the non-AI metrics
    /// (idempotent `ON CONFLICT DO NOTHING`, cross-pod safe); AI allowances
    /// reset natively at OpenRouter and need no rollover code.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_PERIOD_ROLLOVER_INTERVAL_SECS",
        default_value_t = 3600
    )]
    period_rollover_interval_secs: u64,

    /// How often the storage-bytes recompute runs, in seconds (plan:
    /// "Periodic reaper | Storage bytes recompute"). Each run measures every
    /// active tenant's `pg_database_size`, records it in `quota_usage`, and
    /// advances the storage serving state (warn → restrict) against the
    /// plan's `storage_bytes_limit`. Hourly is ample for the day-granularity
    /// grace windows.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_STORAGE_RECOMPUTE_INTERVAL_SECS",
        default_value_t = 3600
    )]
    storage_recompute_interval_secs: u64,

    /// Days an account may sit over its storage limit before the `warn`
    /// marker is set (plan: "week 1 warn"). 0 = warn the instant a recompute
    /// finds it over (the default — the banner should show immediately; the
    /// "week 1" is the window warn occupies before restriction).
    #[arg(
        long,
        env = "ATOMIC_CLOUD_STORAGE_WARN_AFTER_DAYS",
        default_value_t = 0
    )]
    storage_warn_after_days: u64,

    /// Days an account may stay over its storage limit before writes are
    /// restricted (plan: "week 2 restrict writes; no auto-delete"). Default
    /// 7 — the first week is warn-only, restriction lands at week two. Data
    /// is RETAINED, never deleted.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_STORAGE_RESTRICT_AFTER_DAYS",
        default_value_t = 7
    )]
    storage_restrict_after_days: u64,

    /// Days past_due before a delinquent account goes read-only (writes
    /// blocked, reads allowed; plan: "3 days past_due → Read-only mode").
    #[arg(
        long,
        env = "ATOMIC_CLOUD_DUNNING_READ_ONLY_DAYS",
        default_value_t = atomic_cloud::READ_ONLY_AFTER_DAYS
    )]
    dunning_read_only_days: i64,

    /// Days past_due before a delinquent account is suspended (serving and
    /// login blocked; data RETAINED, never deleted; plan: "14 days past_due
    /// → Suspended").
    #[arg(
        long,
        env = "ATOMIC_CLOUD_DUNNING_SUSPENDED_DAYS",
        default_value_t = atomic_cloud::SUSPENDED_AFTER_DAYS
    )]
    dunning_suspended_days: i64,
}

/// The resolved quota/billing background-job configuration, built from
/// [`QuotaArgs`] and handed to `serve`.
#[derive(Clone)]
struct QuotaJobsConfig {
    period_rollover_interval: std::time::Duration,
    storage_recompute_interval: std::time::Duration,
    storage_policy: atomic_cloud::StoragePolicy,
    dunning_thresholds: atomic_cloud::DunningThresholds,
}

impl QuotaArgs {
    fn into_config(self) -> QuotaJobsConfig {
        QuotaJobsConfig {
            // tokio::time::interval panics on a zero period; clamp.
            period_rollover_interval: std::time::Duration::from_secs(
                self.period_rollover_interval_secs.max(1),
            ),
            storage_recompute_interval: std::time::Duration::from_secs(
                self.storage_recompute_interval_secs.max(1),
            ),
            storage_policy: atomic_cloud::StoragePolicy {
                warn_after: std::time::Duration::from_secs(
                    self.storage_warn_after_days * 24 * 60 * 60,
                ),
                restrict_after: std::time::Duration::from_secs(
                    self.storage_restrict_after_days * 24 * 60 * 60,
                ),
            },
            dunning_thresholds: atomic_cloud::DunningThresholds {
                read_only_after_days: self.dunning_read_only_days,
                suspended_after_days: self.dunning_suspended_days,
            },
        }
    }
}

/// The per-pod background dispatcher (plan: "Worker fairness & job queue").
/// On by default for `serve`: tenant saves enqueue durable
/// `atom_pipeline_jobs` rows only, and this pod's dispatcher executes them
/// (plus scheduled tasks, feed polls, wiki-regen retries, and reports)
/// through four bounded worker pools with per-tenant round-robin fairness.
/// Disabling it (`--dispatcher=false`) restores inline pipeline execution
/// in the serving process and runs NO background work — only sensible for
/// debugging a single pod while another pod's dispatcher covers the fleet.
#[derive(Args)]
struct DispatcherArgs {
    /// Run the background dispatcher in this process. On by default:
    /// tenant saves enqueue durable ledger rows and this pod's worker
    /// pools execute them (plus scheduled tasks, feed polls, wiki-regen
    /// retries, and reports). --dispatcher=false restores inline pipeline
    /// execution and runs NO background work — self-hosted parity mode,
    /// only sensible for debugging while another pod covers the fleet.
    #[arg(
        long = "dispatcher",
        env = "ATOMIC_CLOUD_DISPATCHER",
        default_value_t = true,
        action = ArgAction::Set,
        num_args = 1
    )]
    dispatcher: bool,

    /// Milliseconds between dispatcher ticks. Each pod offsets its first
    /// tick by a random fraction of this so a fleet doesn't synchronize.
    #[arg(long, env = "ATOMIC_CLOUD_DISPATCHER_TICK_MS", default_value_t = 2_000)]
    dispatcher_tick_ms: u64,

    /// Seconds between slow-path full scans of ALL active accounts (the
    /// recovery bound for lost dispatch hints and for time-driven work on
    /// otherwise-idle tenants). The first tick after boot always full-scans.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_DISPATCHER_SLOW_SCAN_SECS",
        default_value_t = 300
    )]
    dispatcher_slow_scan_secs: u64,

    /// Pipeline jobs claimed per embedding-pool batch.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_DISPATCHER_PIPELINE_BATCH",
        default_value_t = 8
    )]
    dispatcher_pipeline_batch: i32,

    /// Embedding pool: total in-flight batches per pod.
    #[arg(long, env = "ATOMIC_CLOUD_EMBEDDING_POOL_TOTAL", default_value_t = 32)]
    embedding_pool_total: usize,

    /// Embedding pool: in-flight batches per tenant.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_EMBEDDING_POOL_PER_TENANT",
        default_value_t = 4
    )]
    embedding_pool_per_tenant: usize,

    /// LLM pool (wiki regeneration, reports): total in-flight per pod.
    #[arg(long, env = "ATOMIC_CLOUD_LLM_POOL_TOTAL", default_value_t = 16)]
    llm_pool_total: usize,

    /// LLM pool: in-flight per tenant.
    #[arg(long, env = "ATOMIC_CLOUD_LLM_POOL_PER_TENANT", default_value_t = 2)]
    llm_pool_per_tenant: usize,

    /// Ingestion pool (feed polls): total in-flight per pod.
    #[arg(long, env = "ATOMIC_CLOUD_INGESTION_POOL_TOTAL", default_value_t = 16)]
    ingestion_pool_total: usize,

    /// Ingestion pool: in-flight per tenant.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_INGESTION_POOL_PER_TENANT",
        default_value_t = 4
    )]
    ingestion_pool_per_tenant: usize,

    /// Maintenance pool (draft pipeline, graph maintenance, ledger GC):
    /// total in-flight per pod.
    #[arg(long, env = "ATOMIC_CLOUD_MAINTENANCE_POOL_TOTAL", default_value_t = 8)]
    maintenance_pool_total: usize,

    /// Maintenance pool: in-flight per tenant.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_MAINTENANCE_POOL_PER_TENANT",
        default_value_t = 1
    )]
    maintenance_pool_per_tenant: usize,

    /// Per-tenant in-flight cap for report runs (a work-type carve-out
    /// inside the LLM pool; the plan serializes reports per tenant).
    #[arg(long, env = "ATOMIC_CLOUD_REPORTS_PER_TENANT_CAP", default_value_t = 1)]
    reports_per_tenant_cap: usize,

    /// Ceiling in seconds on one tenant's ledger poll inside a dispatcher
    /// tick: a wedged or unreachable tenant database is skipped for the
    /// tick (and retried next tick) instead of head-of-line-blocking every
    /// other tenant.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_DISPATCHER_TENANT_POLL_TIMEOUT_SECS",
        default_value_t = 10
    )]
    dispatcher_tenant_poll_timeout_secs: u64,

    /// Ceiling in seconds on a provider-supplied Retry-After hint when
    /// scheduling backoff — a hostile or buggy provider must not strand a
    /// tenant's jobs behind an arbitrary horizon.
    #[arg(long, env = "ATOMIC_CLOUD_RETRY_AFTER_CAP_SECS", default_value_t = 900)]
    retry_after_cap_secs: u64,

    /// Circuit breaker: sliding detection window in seconds for
    /// rate-limit-classified provider failures.
    #[arg(long, env = "ATOMIC_CLOUD_BREAKER_WINDOW_SECS", default_value_t = 60)]
    breaker_window_secs: u64,

    /// Circuit breaker: rate-limit failures within the window that trip a
    /// tenant pause.
    #[arg(long, env = "ATOMIC_CLOUD_BREAKER_THRESHOLD", default_value_t = 3)]
    breaker_threshold: usize,

    /// Circuit breaker: first trip's cooldown in seconds (doubles per
    /// consecutive trip).
    #[arg(
        long,
        env = "ATOMIC_CLOUD_BREAKER_BASE_COOLDOWN_SECS",
        default_value_t = 60
    )]
    breaker_base_cooldown_secs: u64,

    /// Circuit breaker: cooldown ceiling in seconds.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_BREAKER_MAX_COOLDOWN_SECS",
        default_value_t = 3600
    )]
    breaker_max_cooldown_secs: u64,

    /// Circuit breaker: credits/auth pause horizon in seconds when the
    /// provider exposes no reset time — how long until dispatch re-probes.
    /// Also the deferral horizon for credits/auth-classified task runs.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_BREAKER_CREDITS_RECHECK_SECS",
        default_value_t = 3600
    )]
    breaker_credits_recheck_secs: u64,
}

impl DispatcherArgs {
    /// `Some(config)` when the dispatcher is enabled, `None` otherwise.
    fn into_config(self) -> Option<DispatcherConfig> {
        if !self.dispatcher {
            return None;
        }
        Some(DispatcherConfig {
            // tokio::time::interval panics on a zero period; clamp.
            tick_interval: std::time::Duration::from_millis(self.dispatcher_tick_ms.max(1)),
            slow_scan_interval: std::time::Duration::from_secs(
                self.dispatcher_slow_scan_secs.max(1),
            ),
            tenant_poll_timeout: std::time::Duration::from_secs(
                self.dispatcher_tenant_poll_timeout_secs.max(1),
            ),
            pipeline_batch_size: self.dispatcher_pipeline_batch.max(1),
            reports_per_tenant_cap: self.reports_per_tenant_cap.max(1),
            retry_after_cap: std::time::Duration::from_secs(self.retry_after_cap_secs.max(1)),
            pools: WorkerPoolsConfig {
                embedding: PoolCaps {
                    total: self.embedding_pool_total,
                    per_tenant: self.embedding_pool_per_tenant,
                },
                llm: PoolCaps {
                    total: self.llm_pool_total,
                    per_tenant: self.llm_pool_per_tenant,
                },
                ingestion: PoolCaps {
                    total: self.ingestion_pool_total,
                    per_tenant: self.ingestion_pool_per_tenant,
                },
                maintenance: PoolCaps {
                    total: self.maintenance_pool_total,
                    per_tenant: self.maintenance_pool_per_tenant,
                },
            },
            breaker: atomic_cloud::BreakerConfig {
                window: std::time::Duration::from_secs(self.breaker_window_secs.max(1)),
                threshold: self.breaker_threshold.max(1),
                base_cooldown: std::time::Duration::from_secs(
                    self.breaker_base_cooldown_secs.max(1),
                ),
                max_cooldown: std::time::Duration::from_secs(self.breaker_max_cooldown_secs.max(1)),
                credits_recheck: std::time::Duration::from_secs(
                    self.breaker_credits_recheck_secs.max(1),
                ),
            },
        })
    }
}

/// Boot-time fleet migration + deploy gating (plan: "Schema migration on
/// deploy"). The new binary boots in migrating mode: `/health` (liveness) is
/// up immediately, `/ready` answers 503 until every lagging tenant database
/// has been brought to the compiled schema target and the failure-rate
/// policy admits. See `atomic_cloud::deploy` for the policy table and
/// `atomic-cloud deploy status` / `deploy advance` for the operator surface.
/// The per-tenant fields (connect timeout, retry backoff) are shared with
/// the reaper's failed-migrations retry arm, so a reaper retry and a boot
/// attempt run and record identically.
#[derive(Args)]
struct FleetArgs {
    /// Tenant databases migrating concurrently during the boot fleet run
    /// (the plan starts at 16; tune from production).
    #[arg(
        long,
        env = "ATOMIC_CLOUD_FLEET_MIGRATION_CONCURRENCY",
        default_value_t = 16
    )]
    fleet_migration_concurrency: usize,

    /// Wall-clock limit in seconds on the boot fleet migration; past it the
    /// run is abandoned and the pod holds not-ready with
    /// deploy_status='migration_timeout' (plan: 30 minutes).
    #[arg(
        long,
        env = "ATOMIC_CLOUD_FLEET_MIGRATION_TIMEOUT_SECS",
        default_value_t = 1800
    )]
    fleet_migration_timeout_secs: u64,

    /// Ceiling in seconds on establishing one tenant's migration connection
    /// — an unreachable tenant database fails fast and recorded instead of
    /// holding a fan-out slot.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_FLEET_CONNECT_TIMEOUT_SECS",
        default_value_t = 10
    )]
    fleet_connect_timeout_secs: u64,

    /// Base in seconds of the exponential backoff horizon recorded on a
    /// failed tenant migration (`base * 2^retry_count`, capped below); the
    /// always-running reaper retries the row once the horizon passes. One
    /// flag feeds both writers of `migration_retry_after` — the boot fleet
    /// runner and the reaper's retry arm — so they can never disagree on
    /// backoff arithmetic.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_MIGRATION_RETRY_BACKOFF_BASE_SECS",
        default_value_t = 60
    )]
    migration_retry_backoff_base_secs: u64,

    /// Ceiling in seconds on that backoff horizon. The reaper keeps
    /// retrying at the cap; a row past 5 retries is escalated to an
    /// error-level alert (plan: "alerts when retry_count > 5").
    #[arg(
        long,
        env = "ATOMIC_CLOUD_MIGRATION_RETRY_BACKOFF_CAP_SECS",
        default_value_t = 1800
    )]
    migration_retry_backoff_cap_secs: u64,

    /// Failure-rate threshold below which the deploy proceeds without
    /// operator action (plan: 1%). Sub-threshold failures are stragglers:
    /// CloudAuth 503s them per request and the reaper retries them.
    #[arg(
        long,
        env = "ATOMIC_CLOUD_DEPLOY_READY_FAILURE_RATE",
        default_value_t = 0.01
    )]
    deploy_ready_failure_rate: f64,

    /// Failure-rate threshold below which (and at/above the ready
    /// threshold) the pod holds with deploy_status='awaiting_review' for an
    /// operator's `deploy advance`; at/above it, 'rollback_required' — no
    /// override short of redeploying the old binary (plan: 10%).
    #[arg(
        long,
        env = "ATOMIC_CLOUD_DEPLOY_REVIEW_FAILURE_RATE",
        default_value_t = 0.10
    )]
    deploy_review_failure_rate: f64,
}

impl FleetArgs {
    fn into_configs(self) -> (FleetMigrationConfig, DeployPolicy) {
        (
            FleetMigrationConfig {
                concurrency: self.fleet_migration_concurrency.max(1),
                tenant_connect_timeout: std::time::Duration::from_secs(
                    self.fleet_connect_timeout_secs.max(1),
                ),
                wall_clock_limit: std::time::Duration::from_secs(
                    self.fleet_migration_timeout_secs.max(1),
                ),
                // Zero base is legal ("retry on the very next reaper pass");
                // only the cap needs a floor for the doubling to terminate.
                retry_backoff_base: std::time::Duration::from_secs(
                    self.migration_retry_backoff_base_secs,
                ),
                retry_backoff_cap: std::time::Duration::from_secs(
                    self.migration_retry_backoff_cap_secs.max(1),
                ),
            },
            DeployPolicy {
                ready_failure_rate: self.deploy_ready_failure_rate,
                review_failure_rate: self.deploy_review_failure_rate,
            },
        )
    }
}

impl EmailArgs {
    /// Build the configured sender, erroring when mailgun mode is missing
    /// credentials — fail at boot, not on the first signup.
    fn into_sender(self) -> Result<Arc<dyn EmailSender>, Box<dyn std::error::Error>> {
        match self.email_mode {
            EmailMode::Log => {
                tracing::warn!(
                    "email mode is 'log': magic links are written to this log, not emailed"
                );
                Ok(Arc::new(LogSender))
            }
            EmailMode::Mailgun => {
                let missing = |flag: &str| {
                    format!("--email-mode mailgun requires {flag} (or its environment variable)")
                };
                // The key VALUE comes from the named environment variable
                // (never argv). The deprecated --mailgun-api-key still works
                // for compatibility, but warns loudly: it leaks the secret
                // into process listings.
                let api_key = if let Some(argv_key) = self.mailgun_api_key {
                    tracing::warn!(
                        "DANGER: --mailgun-api-key passes the Mailgun API key on argv, where it \
                         leaks into process listings (ps, /proc/<pid>/cmdline). Set the key in \
                         the environment and use --mailgun-api-key-env (default reads {}) instead.",
                        MAILGUN_API_KEY_ENV
                    );
                    argv_key
                } else {
                    read_secret_env(&self.mailgun_api_key_env).ok_or_else(|| {
                        format!(
                            "--email-mode mailgun requires the Mailgun API key in the environment \
                             variable {:?} (set --mailgun-api-key-env to read a different one)",
                            self.mailgun_api_key_env
                        )
                    })?
                };
                let domain = self
                    .mailgun_domain
                    .ok_or_else(|| missing("--mailgun-domain"))?;
                let from = self.mailgun_from.ok_or_else(|| missing("--mailgun-from"))?;
                Ok(Arc::new(MailgunSender::new(api_key, domain, from)))
            }
        }
    }
}

#[derive(Subcommand)]
enum AccountAction {
    /// Provision a new account: claim the subdomain, create + migrate the
    /// tenant database, and print a fresh account-scope API token.
    Create {
        #[command(flatten)]
        cluster: ClusterArgs,

        /// Account owner's email address.
        #[arg(long)]
        email: String,

        /// Subdomain the account is served under (3-32 chars of [a-z0-9-]).
        #[arg(long)]
        subdomain: String,
    },

    /// Hard-delete an account: revoke its credentials, drop its tenant
    /// database, and reserve the freed subdomain for 90 days.
    ///
    /// The preferred path is the authenticated HTTP route (`DELETE
    /// /api/account` on the account's subdomain), which also evicts the
    /// serve process's AccountCache entry and severs the account's live
    /// WebSocket sessions. This CLI command runs process-separate, so it
    /// can't reach that cache; a running serve process's stale entry is
    /// harmless and self-heals (requests 404 at auth, the idle TTL reclaims
    /// the entry). Use the CLI for operator cleanup, the route for
    /// everything else.
    Delete {
        #[command(flatten)]
        cluster: ClusterArgs,

        #[command(flatten)]
        backup: BackupArgs,

        /// Subdomain of the account to delete.
        #[arg(long)]
        subdomain: String,
    },

    /// Grant (or with --revoke, remove) admin-portal access for an account.
    /// Admin is a control-plane flag, settable only here — never through a
    /// tenant-facing route.
    Promote {
        /// Subdomain of the account.
        #[arg(long)]
        subdomain: String,

        /// Remove admin access instead of granting it.
        #[arg(long)]
        revoke: bool,
    },

    /// Override an account's plan (and pin it against the automated plan
    /// writers — the trial sweep and Stripe projection). The operator-side
    /// twin of the admin portal's plan action; audits to `admin_actions`.
    SetPlan {
        /// Subdomain of the account.
        #[arg(long)]
        subdomain: String,

        /// Plan id from the `plans` catalogue (e.g. free, pro, comp).
        #[arg(long)]
        plan: String,

        /// Pin the plan (default true). --pin false re-exposes the account
        /// to the automated writers.
        #[arg(long, default_value_t = true)]
        pin: bool,
    },
}

#[derive(Subcommand)]
enum DeployAction {
    /// Print the latest deploy run (counts, verdict) plus every tenant
    /// whose migration is currently failed.
    Status,

    /// Acknowledge an awaiting-review deploy: flips every awaiting_review
    /// run at the latest target version to 'advanced' in the control plane,
    /// so EVERY pod holding on that review (each boots its own run row)
    /// observes the acknowledgment on its next readiness probe and goes
    /// ready. rollback_required deliberately has no override — the
    /// migration itself is broken; redeploy the old binary (see
    /// atomic_cloud::deploy).
    Advance,
}

#[derive(Subcommand)]
enum BackupAction {
    /// Run one nightly backup pass now: dump every active tenant database
    /// (each under its own per-account advisory lock, so a concurrent serve
    /// pass never double-dumps a tenant) plus the control plane to the
    /// configured store, and record a `backup_runs` ledger row. Prints the
    /// observable summary. This is the same `run_backup_pass` the serve loop
    /// runs on its interval — useful for an out-of-band catch-up or a
    /// rehearsal.
    Run {
        #[command(flatten)]
        cluster: ClusterArgs,

        #[command(flatten)]
        backup: BackupArgs,

        /// Max successful tenant dumps this pass before deferring the rest
        /// (stale-first ordering, so the next pass picks up the remainder).
        #[arg(
            long,
            env = "ATOMIC_CLOUD_MAX_BACKUPS_PER_PASS",
            default_value_t = atomic_cloud::DEFAULT_MAX_BACKUPS_PER_PASS
        )]
        max_backups_per_pass: usize,
    },

    /// Report backup health: each active tenant's last successful backup (and
    /// last error), the staleness alert horizon, the tenants currently past
    /// it, and the most recent `backup_runs` ledger rows. The operator's
    /// "is the backup job actually working?" check (plan: "an unmonitored
    /// backup job is a placebo").
    Status {
        /// Staleness alert horizon, in seconds — tenants whose last
        /// successful backup is older than this are flagged stale (plan:
        /// ">36h old").
        #[arg(
            long,
            env = "ATOMIC_CLOUD_BACKUP_STALENESS_SECS",
            default_value_t = atomic_cloud::DEFAULT_STALENESS_HORIZON.as_secs()
        )]
        staleness_secs: u64,
    },

    /// List one tenant's dumps in the configured store (its nightly dumps
    /// plus any final dump). Per-tenant by construction — the keys are named
    /// by this tenant's `db_name` and account id, so another tenant's backups
    /// never surface.
    List {
        #[command(flatten)]
        backup: BackupArgs,

        /// Subdomain of the account whose dumps to list.
        #[arg(long)]
        subdomain: String,
    },

    /// Restore a tenant dump (object key in the configured backup store) into
    /// a FRESH database, then print the runbook's remaining manual steps.
    ///
    /// Restore deliberately does NOT repoint `account_databases.db_name` or
    /// evict a running serve process's AccountCache — those are the operator's
    /// explicit, reviewed steps (plan: "Restore runbook"): a CLI invocation
    /// can't reach another process's in-memory cache, and silently repointing
    /// the mapping would skip the human checkpoint before a tenant starts
    /// serving restored data. The command prints exactly what to run.
    Restore {
        #[command(flatten)]
        cluster: ClusterArgs,

        #[command(flatten)]
        backup: BackupArgs,

        /// Object key of the dump to restore (e.g.
        /// `backups/2026-06-09/acct_<base32>.dump` or
        /// `backups/final/<uuid>-<ts>.dump`).
        #[arg(long)]
        key: String,

        /// Name of the fresh tenant database to restore into. MUST be a valid
        /// `acct_<base32>` tenant name and MUST NOT already exist — restore
        /// never clobbers a live database.
        #[arg(long)]
        target_db: String,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// Mint a new API token for an account and print its plaintext (shown
    /// exactly once; only a hash is stored).
    Create {
        /// Subdomain of the account the token belongs to.
        #[arg(long)]
        subdomain: String,

        /// Token scope: "account" (full access), "database" (one knowledge
        /// base; requires --db), or "mcp".
        #[arg(long, default_value = "account")]
        scope: String,

        /// Knowledge-base id the token is pinned to (required for
        /// --scope database; optional for --scope mcp).
        #[arg(long)]
        db: Option<String>,

        /// Human-readable label for the token.
        #[arg(long, default_value = "cli")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "atomic_cloud=info,warn".parse().unwrap());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Connect to the control plane and bring its schema current — the shared
/// preamble of every subcommand, so each one works against a fresh cluster.
///
/// `max_connections` sizes the control pool: `serve` passes its tuned
/// `--control-pool-max-connections`; short-lived CLI subcommands pass the
/// default.
async fn connect_control(
    control_url: &str,
    max_connections: u32,
) -> Result<ControlPlane, Box<dyn std::error::Error>> {
    let control = ControlPlane::connect(control_url, max_connections).await?;
    let applied = control.initialize().await?;
    if applied > 0 {
        tracing::info!(applied, "applied control-plane migrations");
    }
    Ok(control)
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Only `serve` carries long-lived background loops + the auth path that
    // contend for the control pool; every other subcommand is a short-lived
    // operator command, so a small default pool is ample for them.
    let control_pool_max_connections = match &cli.command {
        Command::Serve {
            control_pool_max_connections,
            ..
        } => *control_pool_max_connections,
        _ => atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
    };
    let control = connect_control(&cli.control_url, control_pool_max_connections).await?;

    match cli.command {
        Command::Migrate => {
            tracing::info!("control-plane schema is current");
            Ok(())
        }

        Command::Serve {
            cluster,
            base_domain,
            port,
            bind,
            tenant_pool_max_connections,
            tenant_pool_idle_timeout_secs,
            // Consumed by the `run` preamble to size the control pool.
            control_pool_max_connections: _,
            cache_sweep_interval_secs,
            reaper_interval_secs,
            email,
            master_key_env,
            trust_proxy_header,
            app_public_url,
            dangerously_insecure_cookies,
            spa_dir,
            product_dir,
            max_concurrent_provisions,
            provisioning,
            billing,
            quota,
            dispatcher,
            fleet,
            chat_streams_per_account,
            backup,
            backup_interval_secs,
            max_backups_per_pass,
            backup_staleness_secs,
            backup_timeout_secs,
        } => {
            // Boot-time master-key check (plan: "Encryption at rest").
            // Constructing the vault validates the key, so a deployment
            // with a missing or malformed master key dies here with a
            // message naming the variable — never on the first signup.
            // The boot contract: serve does not start unless stored
            // provider credentials are decryptable.
            let vault: Arc<dyn KeyVault> = Arc::new(EnvMasterKeyVault::from_env(&master_key_env)?);
            let managed = provisioning.into_managed_keys(Arc::clone(&vault))?;
            let dispatcher_config = dispatcher.into_config();
            let (fleet_config, deploy_policy) = fleet.into_configs();

            let cache_config = AccountCacheConfig {
                tenant_pool_max_connections,
                tenant_pool_idle_timeout: std::time::Duration::from_secs(
                    tenant_pool_idle_timeout_secs,
                ),
                // With a dispatcher attached, tenant saves are enqueue-only
                // (durable `atom_pipeline_jobs` rows) and the dispatcher's
                // embedding pool owns execution. Never flip this off
                // without a dispatcher: enqueued work would sit unexecuted.
                inline_pipeline: dispatcher_config.is_none(),
                // And under the dispatcher composition, provider-classified
                // task-run failures defer instead of consuming retry budget
                // (plan: jobs sit in the ledger, never fail). Derived from
                // the breaker config so the deferral horizons match the
                // pauses that gate dispatch.
                failure_disposition_policy: dispatcher_config.as_ref().map(|config| {
                    atomic_cloud::provider_failure_policy(
                        config.breaker.credits_recheck,
                        config.retry_after_cap,
                    )
                }),
                ..AccountCacheConfig::default()
            };
            if dangerously_insecure_cookies {
                tracing::warn!(
                    "DANGER: --dangerously-insecure-cookies is set — the session cookie \
                     omits the Secure attribute and can travel over unencrypted HTTP. \
                     This is for local/headless dev over plain HTTP only; NEVER use it \
                     in production."
                );
            }
            // Boot warnings for operator footguns that are catastrophic on a
            // production pod but harmless in dev. Scoped (where it makes
            // sense) to a non-localhost base domain so dev runs stay quiet.
            let base_is_localhost = base_domain_is_localhost(&base_domain);
            if matches!(backup.backup_store, BackupStoreKind::Local) {
                tracing::warn!(
                    "DANGER: --backup-store local writes backups — including the final \
                     pre-DROP dump taken when an account is hard-deleted — to a local \
                     directory. On a pod with ephemeral storage that disk evaporates on \
                     restart, so a tenant deletion's only undo is lost. This is for \
                     local/self-hosted dev only; use --backup-store s3 in production.{}",
                    if base_is_localhost {
                        ""
                    } else {
                        " The base domain is not localhost, so this looks like a real \
                         deployment."
                    }
                );
            }
            if !trust_proxy_header && !base_is_localhost {
                tracing::warn!(
                    "--trust-proxy-header is off but the base domain is not localhost: if a \
                     reverse proxy fronts this process, every client shares the proxy's IP \
                     and per-IP rate limits collapse onto a single bucket. Enable \
                     --trust-proxy-header when, and only when, a trusted proxy sets \
                     X-Forwarded-For."
                );
            }
            if atomic_cloud::provider_config::private_provider_urls_allowed() && !base_is_localhost
            {
                tracing::warn!(
                    "{} is set on a non-localhost deployment: the BYOK provider base-URL SSRF \
                     gate is DISABLED, so a tenant can point our outbound client at private/\
                     loopback/metadata addresses. This is a dev/test-only escape — unset it in \
                     production.",
                    atomic_cloud::provider_config::ALLOW_PRIVATE_PROVIDER_URLS_ENV,
                );
            }
            // Tenant content and the encrypted-credential ciphertexts travel the
            // app↔Postgres link; on a real deployment that link must be TLS.
            // sqlx negotiates TLS from the URL's sslmode, so warn when neither
            // connection URL requires it (the operator may still terminate TLS
            // via a trusted local proxy/socket, hence a warning, not a refusal).
            if !base_is_localhost {
                for (flag, pg_url) in [
                    ("--control-url", cli.control_url.as_str()),
                    ("--cluster-url", cluster.cluster_url.as_str()),
                ] {
                    if !pg_url_requires_tls(pg_url) {
                        tracing::warn!(
                            "{flag} does not require TLS (no sslmode=require/verify-ca/verify-full): \
                             tenant content and encrypted credentials may cross the network in \
                             plaintext. Set sslmode=require (verify-full preferred) on {flag}, or \
                             ensure TLS is terminated by a trusted local proxy/socket (e.g. the \
                             Cloud SQL Auth Proxy). See DEPLOY.md §2."
                        );
                    }
                }
            }
            let plane_config = AccountPlaneConfig {
                app_public_url: app_public_url.clone(),
                trust_proxy_header,
                rate_limits: RateLimits::default(),
                max_concurrent_provisions,
                cookie_secure: !dangerously_insecure_cookies,
                ..AccountPlaneConfig::new(base_domain.clone())
            };

            // Plans/quota/billing composition inputs (plan: "Quotas",
            // "Billing"). The plan registry is loaded eagerly so an unseeded
            // catalogue (a migration that didn't run) fails at boot. Billing
            // is optional — no Stripe key means the routes 503 and the
            // dunning sweep finds nothing to advance.
            let plan_registry = web::Data::new(PlanRegistry::load(control.clone()).await?);
            let billing_plane = Billing::new(
                control.clone(),
                billing.into_config(base_domain.clone(), app_public_url)?,
            )?
            // Thread the managed-key handle so the webhook reconciles a tenant's
            // managed-AI allowance to its plan after a subscription transition
            // commits (MAI-1).
            .with_managed_keys(managed.clone());
            let quota_billing = QuotaBilling {
                plan_registry,
                rate_limiter: DataPlaneRateLimiter::new(DataPlaneRateLimits::default()),
                billing: billing_plane,
            };
            let quota_jobs = quota.into_config();

            // Backup store + nightly-pass config (plan: "Backups & disaster
            // recovery"). The store backs the nightly pass, the final pre-drop
            // dump in DELETE /api/account, and the reaper's interrupted-
            // deletion completion. Built once, shared by all three.
            let backup_store = backup.into_store()?;
            let backup_timeout = std::time::Duration::from_secs(backup_timeout_secs.max(1));
            let backup_config = atomic_cloud::BackupConfig {
                max_backups_per_pass,
                staleness_horizon: std::time::Duration::from_secs(backup_staleness_secs),
                backup_timeout,
            };

            serve(
                control,
                cluster.into_config(),
                managed,
                vault,
                base_domain,
                bind,
                port,
                cache_config,
                cache_sweep_interval_secs.map(std::time::Duration::from_secs),
                // tokio::time::interval panics on a zero period; clamp.
                std::time::Duration::from_secs(reaper_interval_secs),
                email.into_sender()?,
                plane_config,
                dispatcher_config,
                fleet_config,
                deploy_policy,
                ChatStreamLimiter::new(chat_streams_per_account),
                quota_billing,
                quota_jobs,
                backup_store,
                backup_config,
                std::time::Duration::from_secs(backup_interval_secs.max(1)),
                spa_dir,
                product_dir,
            )
            .await
        }

        Command::Account { action } => match action {
            AccountAction::Create {
                cluster,
                email,
                subdomain,
            } => {
                // Operator-side creation never provisions a managed key:
                // this command runs from hosts that hold neither the
                // master key nor the provisioning key (see the module
                // docs), so the account starts keyless — same state as
                // provisioning-mode 'disabled'.
                let account = provision_account(
                    &control,
                    &cluster.into_config(),
                    &ManagedKeys::Disabled,
                    NewAccount { email, subdomain },
                )
                .await?;
                let token = issue_token(
                    &control,
                    &account.account_id,
                    TokenScope::Account,
                    None,
                    "initial",
                )
                .await?;
                println!("account_id: {}", account.account_id);
                println!("subdomain:  {}", account.subdomain);
                println!("tenant_db:  {}", account.db_name);
                println!("token:      {token}");
                println!("(the token is shown once; only its hash is stored)");
                Ok(())
            }

            AccountAction::Delete {
                cluster,
                backup,
                subdomain,
            } => {
                let account_id = control
                    .account_id_by_subdomain(&subdomain)
                    .await?
                    .ok_or_else(|| format!("no account with subdomain {subdomain:?}"))?;
                // The final pre-drop dump (plan: "Account deletion" step 4) is
                // taken to the configured backup store BEFORE the database is
                // dropped — fail-closed, so a dump failure aborts the deletion
                // rather than destroy un-backed-up data. The operator's undo.
                let backup_store = backup.into_store()?;
                // Disabled managed keys here: this host has no provisioning
                // key, so a managed runtime key (if the account has one)
                // cannot be deleted from the CLI — delete_account logs the
                // residue loudly with the external id; clean it up via the
                // master OpenRouter account's key listing. The HTTP deletion
                // route (the preferred path) deletes the key properly.
                //
                // The CLI is an active-account deletion path: it states an
                // explicit Required backup policy (a final dump is mandatory —
                // issue 3) and takes the per-account advisory lock itself
                // (DeleteLock::Acquire — issue 2), so a concurrent nightly
                // backup pass and this delete are mutually exclusive.
                delete_account(
                    &control,
                    &cluster.into_config(),
                    &ManagedKeys::Disabled,
                    // The CLI runs no Stripe client, so a subscription cancel is
                    // skipped (the DEL-1 `billing` is `None` on the CLI, per
                    // `delete_account`'s docs); an operator reconciles any leaked
                    // subscription from the Stripe dashboard.
                    None,
                    atomic_cloud::BackupPolicy::Required(&backup_store),
                    atomic_cloud::DeleteLock::Acquire,
                    &account_id,
                    atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
                )
                .await?;
                println!("deleted account {account_id} ({subdomain})");
                Ok(())
            }

            AccountAction::Promote { subdomain, revoke } => {
                atomic_cloud::admin::set_admin_flag(&control, &subdomain, !revoke).await?;
                println!(
                    "{} admin access for {subdomain}",
                    if revoke { "revoked" } else { "granted" }
                );
                Ok(())
            }

            AccountAction::SetPlan {
                subdomain,
                plan,
                pin,
            } => {
                let account_id = control
                    .account_id_by_subdomain(&subdomain)
                    .await?
                    .ok_or_else(|| format!("no account with subdomain {subdomain:?}"))?;
                // Managed keys are disabled on CLI hosts (no provisioning
                // key), so the allowance resize inside set_plan_override is
                // a logged no-op here; it reconciles on the next transition
                // a serve pod drives, or immediately when run through the
                // admin portal instead.
                atomic_cloud::admin::set_plan_override(
                    &control,
                    &ManagedKeys::Disabled,
                    "cli",
                    &account_id,
                    &plan,
                    pin,
                )
                .await?;
                println!(
                    "set {subdomain} to plan {plan} ({})",
                    if pin { "pinned" } else { "unpinned" }
                );
                Ok(())
            }
        },

        Command::Token { action } => match action {
            TokenAction::Create {
                subdomain,
                scope,
                db,
                name,
            } => {
                let scope: TokenScope = scope.parse()?;
                match scope {
                    TokenScope::Account if db.is_some() => {
                        return Err("--db only applies to database/mcp scopes".into());
                    }
                    TokenScope::Database if db.is_none() => {
                        return Err("--scope database requires --db".into());
                    }
                    _ => {}
                }
                let account_id = control
                    .account_id_by_subdomain(&subdomain)
                    .await?
                    .ok_or_else(|| format!("no account with subdomain {subdomain:?}"))?;
                let token = issue_token(&control, &account_id, scope, db.as_deref(), &name).await?;
                println!("{token}");
                Ok(())
            }
        },

        Command::Deploy { action } => match action {
            DeployAction::Status => {
                finalize_stale_runs(&control).await;
                print_deploy_status(&control).await?;
                Ok(())
            }
            DeployAction::Advance => {
                finalize_stale_runs(&control).await;
                match advance_deploy(&control).await? {
                    AdvanceOutcome::Advanced {
                        target_version,
                        runs,
                    } => {
                        println!(
                            "advanced {runs} awaiting_review deploy run(s) at target \
                         version {target_version}"
                        );
                        println!("every pod holding on that review goes ready on its next probe");
                        Ok(())
                    }
                    AdvanceOutcome::RefusedRollbackRequired => Err(
                        "refusing to advance: the latest deploy run is rollback_required \
                     (failure rate ≥ the rollback threshold). The migration itself is \
                     broken; there is deliberately no override — redeploy the previous \
                     binary (additive-only migrations make that safe), fix the \
                     migration, and deploy again."
                            .into(),
                    ),
                    AdvanceOutcome::NothingToAdvance { status } => {
                        println!("nothing awaiting review (latest deploy run is '{status}')");
                        Ok(())
                    }
                    AdvanceOutcome::NoRuns => {
                        println!("no deploy runs recorded yet");
                        Ok(())
                    }
                }
            }
        },

        Command::Backup { action } => match action {
            BackupAction::Run {
                cluster,
                backup,
                max_backups_per_pass,
            } => {
                let cluster = cluster.into_config();
                let store = backup.into_store()?;
                let config = atomic_cloud::BackupConfig {
                    max_backups_per_pass,
                    ..atomic_cloud::BackupConfig::default()
                };
                let now = chrono::Utc::now();
                let summary =
                    atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now).await;

                println!("backup pass complete:");
                println!(
                    "  tenants backed up:    {}",
                    summary.tenants_backed_up.len()
                );
                println!(
                    "  tenants skipped (locked by another pod): {}",
                    summary.tenants_skipped_locked.len()
                );
                println!(
                    "  tenants deferred (over cap): {}",
                    summary.tenants_deferred.len()
                );
                println!("  tenants failed:       {}", summary.tenants_failed.len());
                println!(
                    "  control plane:        {}",
                    if summary.control_backed_up {
                        "backed up"
                    } else {
                        "FAILED"
                    }
                );
                for err in &summary.errors {
                    eprintln!("  error: {err}");
                }
                // A pass with any per-target failure or a control-dump miss
                // exits non-zero so a cron/operator wrapper notices.
                if !summary.tenants_failed.is_empty()
                    || !summary.control_backed_up
                    || !summary.errors.is_empty()
                {
                    return Err(format!(
                        "backup pass completed with {} tenant failure(s); control plane {}",
                        summary.tenants_failed.len(),
                        if summary.control_backed_up {
                            "ok"
                        } else {
                            "FAILED"
                        }
                    )
                    .into());
                }
                Ok(())
            }

            BackupAction::Status { staleness_secs } => {
                let horizon = std::time::Duration::from_secs(staleness_secs);

                // Finalize any in-flight runs left by a pod that died mid-pass
                // so status doesn't show a perpetually in-flight pass
                // (adversarial-review issue 6).
                let finalized = atomic_cloud::finalize_abandoned_backup_runs(
                    &control,
                    atomic_cloud::DEFAULT_BACKUP_RUN_ABANDON_AFTER,
                )
                .await?;
                if finalized > 0 {
                    println!(
                        "(finalized {finalized} stale in-flight backup run(s) as 'abandoned')\n"
                    );
                }

                let statuses = atomic_cloud::tenant_backup_status(&control).await?;
                println!(
                    "per-tenant backup status ({} active tenant(s)):",
                    statuses.len()
                );
                for s in &statuses {
                    let last = s
                        .last_backup_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "never".into());
                    print!("  {} ({}) last_backup_at={last}", s.subdomain, s.db_name);
                    if let Some(err) = &s.last_backup_error {
                        print!("  last_error={err:?}");
                    }
                    println!();
                }

                let stale = atomic_cloud::stale_tenant_backups(&control, horizon).await?;
                println!();
                if stale.is_empty() {
                    println!(
                        "no stale tenants (all active tenants backed up within {}s).",
                        horizon.as_secs()
                    );
                } else {
                    println!(
                        "STALE: {} tenant(s) past the {}s horizon (no successful backup):",
                        stale.len(),
                        horizon.as_secs()
                    );
                    for t in &stale {
                        let last = t
                            .last_backup_at
                            .map(|x| x.to_rfc3339())
                            .unwrap_or_else(|| "never".into());
                        println!(
                            "  {} (account {}) last_backup_at={last}",
                            t.db_name, t.account_id
                        );
                    }
                }

                let runs = atomic_cloud::recent_backup_runs(&control, 10).await?;
                println!();
                println!("recent backup runs (newest first):");
                for r in &runs {
                    let finished = r
                        .finished_at
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "in-flight".into());
                    let status = r.status.as_deref().unwrap_or("-");
                    println!(
                        "  {} kind={} status={} started={} finished={} total={:?} ok={:?} failed={:?}",
                        r.id,
                        r.kind,
                        status,
                        r.started_at.to_rfc3339(),
                        finished,
                        r.total,
                        r.succeeded,
                        r.failed
                    );
                }
                Ok(())
            }

            BackupAction::List { backup, subdomain } => {
                let account_id = control
                    .account_id_by_subdomain(&subdomain)
                    .await?
                    .ok_or_else(|| format!("no account with subdomain {subdomain:?}"))?;
                // The tenant's active db_name names its nightly dumps; the
                // account id names its final dumps.
                let db_name: String = sqlx::query_scalar(
                    "SELECT db_name FROM account_databases \
                     WHERE account_id = $1 AND status = 'active' \
                     ORDER BY created_at LIMIT 1",
                )
                .bind(&account_id)
                .fetch_optional(control.pool())
                .await
                .map_err(|e| format!("looking up tenant db_name: {e}"))?
                .ok_or_else(|| {
                    format!("account {account_id} ({subdomain}) has no active tenant database")
                })?;

                let store = backup.into_store()?;
                let keys = atomic_cloud::dumps_for_account(&store, &account_id, &db_name).await?;
                println!(
                    "{} dump(s) for {subdomain} (account {account_id}, db {db_name}):",
                    keys.len()
                );
                for key in &keys {
                    let size = store.get(key).await.map(|b| b.len()).unwrap_or(0);
                    println!("  {key}  ({size} bytes)");
                }
                Ok(())
            }

            BackupAction::Restore {
                cluster,
                backup,
                key,
                target_db,
            } => {
                if !atomic_cloud::provision::is_tenant_db_name(&target_db) {
                    return Err(format!(
                        "--target-db {target_db:?} is not a valid acct_<base32> tenant database name"
                    )
                    .into());
                }
                let cluster = cluster.into_config();
                let store = backup.into_store()?;
                let conn = atomic_cloud::DumpConnection::for_cluster(&cluster)?;

                println!("fetching dump {key} from the backup store...");
                let dump = store.get(&key).await?;
                println!(
                    "restoring {} bytes into fresh database {target_db}...",
                    dump.len()
                );
                atomic_cloud::restore_database(
                    &cluster,
                    &conn,
                    &target_db,
                    &dump,
                    atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
                )
                .await?;

                println!("restore complete: {target_db} is populated from {key}.");
                println!();
                let target_version = atomic_cloud::tenant_schema_target();
                println!("REMAINING RUNBOOK STEPS (plan: \"Restore runbook\"):");
                println!(
                    "  1. Repoint the account's mapping to the restored database, recording the"
                );
                println!(
                    "     schema version the dump carries (this binary's compiled target = \
                     {target_version}) so CloudAuth's straggler gate does not 503 the restored"
                );
                println!("     tenant as forever-upgrading:");
                println!(
                    "       UPDATE account_databases SET db_name = '{target_db}', \
                     last_migrated_version = {target_version}, last_migrated_at = NOW() \
                     WHERE account_id = '<account-id>';"
                );
                println!(
                    "  2. Evict any running serve process's AccountCache entry for that account"
                );
                println!(
                    "     (an admin evict endpoint is out of scope this slice; restart the pod,"
                );
                println!(
                    "     or let the idle TTL reclaim it — until then it serves the OLD db_name)."
                );
                println!(
                    "  3. Drop the old database once you've confirmed the restore \
                     (it was left intact)."
                );
                Ok(())
            }
        },
    }
}

/// Finalize stale `migrating` deploy runs before any operator command reads
/// them — a dead pod's row must not shadow `deploy advance` (see
/// `atomic_cloud::deploy::finalize_abandoned_runs`). The CLI doesn't know
/// the fleet's `--fleet-migration-timeout-secs`, so the threshold derives
/// from the default config (conservative: a custom shorter timeout only
/// delays finalization, never mislabels a live run). Best-effort — a
/// failure is reported and the command proceeds on the unfinalized rows.
async fn finalize_stale_runs(control: &ControlPlane) {
    let threshold = abandoned_run_threshold(&FleetMigrationConfig::default());
    match finalize_abandoned_runs(control, threshold).await {
        Ok(0) => {}
        Ok(n) => println!("finalized {n} stale 'migrating' deploy run(s) as 'abandoned'"),
        Err(e) => eprintln!("warning: finalizing stale deploy runs failed: {e}"),
    }
}

/// `deploy status`: the latest run, the compiled target plus how many
/// tenants still lag it, and the currently-failed tenant migrations.
async fn print_deploy_status(control: &ControlPlane) -> Result<(), Box<dyn std::error::Error>> {
    let target = tenant_schema_target();
    println!("compiled tenant schema target: {target}");
    let lagging = list_unmigrated(control, target).await?;
    println!("tenants below target: {}", lagging.len());

    match latest_deploy_run(control).await? {
        None => println!("no deploy runs recorded yet"),
        Some(run) => {
            println!("latest deploy run: {}", run.id);
            println!("  status:         {}", run.deploy_status);
            println!("  target version: {}", run.target_version);
            println!("  started:        {}", run.started_at.to_rfc3339());
            match run.finished_at {
                Some(finished) => println!("  finished:       {}", finished.to_rfc3339()),
                None => println!("  finished:       (still running, or the pod died mid-run)"),
            }
            if let (Some(total), Some(migrated), Some(failed)) =
                (run.total, run.migrated, run.failed)
            {
                let rate = if total > 0 {
                    failed as f64 / total as f64 * 100.0
                } else {
                    0.0
                };
                println!(
                    "  tenants:        {total} enumerated, {migrated} migrated, \
                     {failed} failed ({rate:.2}% failure rate), {} unattempted",
                    total - migrated - failed
                );
            }
            if let Some(advanced_at) = run.advanced_at {
                println!("  advanced:       {}", advanced_at.to_rfc3339());
            }
        }
    }

    let failures = list_failed_migrations(control).await?;
    if failures.is_empty() {
        println!("failed tenant migrations: none");
    } else {
        println!("failed tenant migrations: {}", failures.len());
        for f in failures {
            println!(
                "  {} ({}) at v{}, {} retr{}, failed {}, next retry {}: {}",
                f.account_id,
                f.db_name,
                f.last_migrated_version,
                f.migration_retry_count,
                if f.migration_retry_count == 1 {
                    "y"
                } else {
                    "ies"
                },
                f.migration_failed_at.to_rfc3339(),
                f.migration_retry_after
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "(unset)".to_string()),
                f.last_migration_error
                    .as_deref()
                    .unwrap_or("(no error recorded)"),
            );
        }
    }
    Ok(())
}

/// Run the composed multi-tenant server until interrupted. See
/// [`atomic_cloud::server`] for what the composition serves (and what it
/// deliberately doesn't until later slices).
///
/// `sweep_interval` controls the periodic account-cache sweep; `None` means
/// a quarter of the cache's idle TTL. `reaper_interval` paces the
/// failure-recovery reaper. `dispatcher_config` (`Some` by default —
/// `--dispatcher=false` to disable) runs the per-pod background dispatcher;
/// the caller must have built `cache_config` with `inline_pipeline` set to
/// the matching value (off exactly when a dispatcher runs).
/// `fleet_config` + `deploy_policy` drive the boot fleet migration gate:
/// the process starts serving immediately (liveness up) but `/ready` holds
/// 503 until the gate's policy admits (see `atomic_cloud::deploy`).
/// `chat_streams` is the process-wide streaming-chat semaphore, cloned into
/// every HTTP worker.
#[allow(clippy::too_many_arguments)] // CLI assembly; every argument is a distinct serve knob.
async fn serve(
    control: ControlPlane,
    cluster: ClusterConfig,
    managed: ManagedKeys,
    vault: Arc<dyn KeyVault>,
    base_domain: String,
    bind: String,
    port: u16,
    cache_config: AccountCacheConfig,
    sweep_interval: Option<std::time::Duration>,
    reaper_interval: std::time::Duration,
    email: Arc<dyn EmailSender>,
    plane_config: AccountPlaneConfig,
    dispatcher_config: Option<DispatcherConfig>,
    fleet_config: FleetMigrationConfig,
    deploy_policy: DeployPolicy,
    chat_streams: ChatStreamLimiter,
    quota_billing: QuotaBilling,
    quota_jobs: QuotaJobsConfig,
    backup_store: Arc<dyn atomic_cloud::BackupStore>,
    backup_config: atomic_cloud::BackupConfig,
    backup_interval: std::time::Duration,
    spa_dir: std::path::PathBuf,
    product_dir: Option<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let sweep_interval = sweep_interval
        .unwrap_or(cache_config.idle_ttl / 4)
        .max(std::time::Duration::from_secs(1));
    let cache = Arc::new(AccountCache::new(
        control.clone(),
        cluster.clone(),
        Arc::clone(&vault),
        cache_config,
    ));
    // Cluster handle for the storage-recompute loop's maintenance
    // connection, captured before `cluster` is moved into the account plane.
    let cluster_for_jobs = cluster.clone();
    // The tenant public origin's scheme — `https` in production, `http` for
    // local/dev — drives both CloudAuth's MCP `WWW-Authenticate` challenge and
    // the OAuth plane's per-tenant discovery URLs (below), so they're derived
    // once here from the same app-public-URL the account plane uses. A missing
    // or unparseable override fails at boot, not on the first OAuth probe.
    let app_public_url = plane_config
        .app_public_url
        .clone()
        .unwrap_or_else(|| format!("https://app.{base_domain}"));
    let public_scheme = url::Url::parse(&app_public_url)
        .map_err(|e| {
            atomic_cloud::CloudError::InvalidUrl(format!("app public URL {app_public_url:?}: {e}"))
        })?
        .scheme()
        .to_string();
    let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), &base_domain)
        .with_public_scheme(public_scheme.clone());
    // The deletion route takes its final pre-drop dump to the backup store
    // (plan: "Account deletion" step 4) — the operator's undo under
    // hard-delete v1.
    let tenant_plane = TenantPlane::new(
        control.clone(),
        cluster.clone(),
        managed.clone(),
        vault,
        Arc::clone(&cache),
    )
    .with_backup_store(Arc::clone(&backup_store), backup_config.backup_timeout)
    // Tell the dashboard overview whether the portal/checkout routes are live
    // (a Stripe key is configured) so the billing page enables or explains its
    // actions instead of bouncing the browser onto a `billing_not_configured`
    // 503.
    .with_billing_configured(quota_billing.billing.is_configured())
    // Thread the Stripe provider so the active-deletion route fires a
    // best-effort subscription cancel before the tenant's `stripe_subscriptions`
    // row is CASCADE-deleted (DEL-1).
    .with_billing_provider(quota_billing.billing.provider().cloned());

    // The reaper loop runs concurrently with the server below via select!,
    // not tokio::spawn: spawn's Send bound trips rustc's
    // "implementation is not general enough" higher-ranked lifetime false
    // positive on provision_account's sqlx futures (the same one
    // tests/provisioning.rs works around with join!), and select! on the
    // main task needs no Send while also tying the reaper's lifetime to
    // serve's. The reaper gets the backup store so it dumps an interrupted
    // deletion it completes (arm 3) before finishing the destruction.
    let reaper_loop = run_reaper_loop(
        control.clone(),
        cluster.clone(),
        managed.clone(),
        reaper_interval,
        fleet_config.clone(),
        Arc::clone(&backup_store),
        backup_config.backup_timeout,
    );

    // Nightly backup pass (plan: "Backups & disaster recovery" → nightly
    // logical dumps): jittered-start interval glue around the tested
    // `run_backup_pass`, mirroring the reaper loop. Cross-pod safe (per-tenant
    // advisory locks), and each pass logs a staleness alert for any tenant
    // whose last successful backup is older than the horizon.
    //
    // Unlike the reaper above, this runs via tokio::spawn rather than select!:
    // the backup pass only dumps and uploads — it never calls provision_account
    // — so it is free of the sqlx higher-ranked-lifetime future that trips
    // spawn's Send bound there. Don't "fix" this to match the reaper's select!;
    // spawn is the lighter idiom and is sound precisely because no provision
    // future crosses it.
    tokio::spawn(run_backup_loop(
        control.clone(),
        cluster.clone(),
        Arc::clone(&backup_store),
        backup_config,
        backup_interval,
    ));

    // Deploy gate (plan: "Schema migration on deploy"): boot in migrating
    // mode and run the fleet migration concurrently with serving, so
    // liveness is up from the first request while `/ready` holds 503 until
    // every lagging tenant is migrated and the failure-rate policy admits.
    // The dispatcher below is safe to start immediately — its tick skips
    // tenants whose schema still lags the compiled target, exactly as
    // CloudAuth 503s their requests.
    let readiness = Readiness::new(control.clone());
    tokio::spawn(run_fleet_gate(
        control.clone(),
        cluster.clone(),
        fleet_config,
        deploy_policy,
        readiness.clone(),
    ));

    // Cloud OAuth plane (plan: "OAuth"). It builds per-tenant issuer URLs from
    // `{public_scheme}://{request Host}` and bounces un-logged-in authorize
    // requests to `{app_public_url}/login` — both off the same `public_scheme`
    // / `app_public_url` derived above (shared with CloudAuth's MCP challenge).
    let oauth_plane = atomic_cloud::OAuthPlane::new(
        control.clone(),
        base_domain.clone(),
        public_scheme,
        app_public_url,
    );

    // The trial sweep below re-sizes a downgraded account's managed key to the
    // free allowance, so it needs its own handle to the managed-key plane;
    // clone before `managed` is moved into the account plane.
    let trial_managed = managed.clone();
    let account_plane = AccountPlane::new(control.clone(), cluster, managed, email, plane_config)?;

    // Periodic idle sweep. The cache also sweeps inline when a load inserts
    // a new entry, but a stable working set produces no inserts — without
    // this task, idle entries would hold their tenant pools forever. The
    // sweep semantics themselves (TTL, live-WebSocket pinning) are pinned by
    // tests/account_cache.rs, which drives `sweep()` with no insert traffic;
    // this loop is interval glue around that tested method.
    tokio::spawn({
        let cache = Arc::clone(&cache);
        async move {
            let mut ticker = tokio::time::interval(sweep_interval);
            // The first tick fires immediately; nothing can be idle yet.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                cache.sweep().await;
            }
        }
    });

    // Dunning + trial sweep (plan: "Billing" → dunning, "Trials"): advance
    // past_due → read_only (3 days) → suspended (14 days), and downgrade
    // expired trials to free (read_only if over the free limits). Data is
    // always retained. Hourly is ample for day-granularity thresholds; the
    // transition logic takes an explicit `now`, so this is interval glue
    // around tested functions. Cross-pod safe: every dunning transition is an
    // idempotent conditional UPDATE (the first pod to advance an account
    // wins; later pods match zero rows), and the trial downgrade is guarded
    // on `billing_state = 'trialing'` so only one pod's UPDATE takes. Cheap
    // when billing/trials are inactive — no account matches, so every sweep
    // is a no-op set of UPDATEs.
    //
    // The trial downgrade's over-limit decision reads the tenant database (the
    // live atom/KB count vs the free plan), via the same `AccountCache` the
    // request path uses; the free plan comes from the loaded registry.
    let trial_registry = quota_billing.plan_registry.clone();
    let dunning_thresholds = quota_jobs.dunning_thresholds;
    tokio::spawn({
        let control = control.clone();
        let cache = Arc::clone(&cache);
        let managed = trial_managed;
        async move {
            let mut ticker = tokio::time::interval(DEFAULT_DUNNING_SWEEP_INTERVAL);
            ticker.tick().await; // first tick fires immediately
            loop {
                ticker.tick().await;
                let now = chrono::Utc::now();
                match advance_dunning_with(&control, now, dunning_thresholds).await {
                    Ok(advance) if !advance.is_quiet() => {
                        tracing::info!(?advance, "dunning sweep")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "dunning sweep failed"),
                }

                // Trial auto-downgrade. The free plan must be in the registry
                // (migration 010 seeds it); if it somehow isn't, skip the
                // arm rather than guess a limit.
                let Some(free_plan) = trial_registry.get(DEFAULT_PLAN_ID) else {
                    tracing::error!("free plan absent from registry; skipping trial sweep");
                    continue;
                };
                let cache = Arc::clone(&cache);
                let over_free_limits = |account_id: String| {
                    let cache = Arc::clone(&cache);
                    let free_plan = free_plan.clone();
                    async move {
                        let handle = cache.get_or_load(&account_id).await?;
                        account_over_plan_limits(&free_plan, &handle.manager)
                            .await
                            .map_err(|e| {
                                atomic_cloud::CloudError::Invariant(format!(
                                    "reading tenant resource count for trial downgrade: {e}"
                                ))
                            })
                    }
                };
                match advance_expired_trials(&control, &managed, now, over_free_limits).await {
                    Ok(advance) if !advance.is_quiet() => {
                        tracing::info!(?advance, "trial sweep")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "trial sweep failed"),
                }
            }
        }
    });

    // Period-rollover loop (plan: "Period rollover" — "A 1-hour-cadence job
    // inserts new `period_start` rows for the remaining metrics"). Opens the
    // current month's quota_usage rows for the non-AI metrics; idempotent
    // (`ON CONFLICT DO NOTHING`) and cross-pod safe with no lock — the first
    // pod to run it in a new month inserts the rows, every later pod is a
    // no-op INSERT. AI allowances reset natively at OpenRouter, so they are
    // deliberately absent from the rollover. The transition takes an explicit
    // `now`, so this is interval glue around a tested function.
    let period_rollover_interval = quota_jobs.period_rollover_interval;
    tokio::spawn({
        let control = control.clone();
        async move {
            let mut ticker = tokio::time::interval(period_rollover_interval);
            // The first tick fires immediately — open the current period at
            // boot so a fresh deploy has rows without waiting an interval.
            loop {
                ticker.tick().await;
                match roll_over_period(&control, chrono::Utc::now()).await {
                    Ok(n) if n > 0 => tracing::info!(opened = n, "quota period rollover"),
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "quota period rollover failed"),
                }
            }
        }
    });

    // Storage-recompute loop (plan: "Periodic reaper | Storage bytes
    // recompute | Week 1 warn; week 2 restrict writes; **no auto-delete**").
    // Measures every active tenant's pg_database_size into quota_usage and
    // advances each account's storage_state (warn → restrict) against its
    // plan's storage_bytes_limit; data is RETAINED, never deleted. The arm
    // takes an explicit `now`/policy, so this is interval glue around a tested
    // function. Cross-pod safe: the per-account state UPDATE only ever
    // advances or clears, and the metric UPSERT is last-writer-wins on a
    // snapshot (not an accumulator), so concurrent pods converge.
    let storage_recompute_interval = quota_jobs.storage_recompute_interval;
    let storage_policy = quota_jobs.storage_policy.clone();
    let storage_registry = quota_billing.plan_registry.clone();
    tokio::spawn({
        let control = control.clone();
        let cluster = cluster_for_jobs.clone();
        async move {
            let mut ticker = tokio::time::interval(storage_recompute_interval);
            ticker.tick().await; // first tick fires immediately
            loop {
                ticker.tick().await;
                let summary = recompute_storage(
                    &control,
                    &cluster,
                    &storage_registry,
                    &storage_policy,
                    chrono::Utc::now(),
                )
                .await;
                if !summary.is_quiet() {
                    tracing::info!(?summary, "storage recompute");
                }
            }
        }
    });

    // Per-pod background dispatcher (plan: "Worker fairness & job queue").
    // Spawned over the SAME AccountCache the request path uses, so worker
    // events land on the channels live WebSocket clients subscribe to.
    // No leader election: ledger claims are the cross-pod exclusion, and
    // run_loop jitters its first tick. The dispatcher shares the request
    // path's plan catalogue so its atom-limit gate on atom-creating
    // background work reads the same limits the data-plane quota guard does.
    let dispatcher_plan_registry = quota_billing.plan_registry.clone().into_inner();
    match dispatcher_config {
        Some(config) => {
            tracing::info!(
                tick_ms = config.tick_interval.as_millis() as u64,
                slow_scan_secs = config.slow_scan_interval.as_secs(),
                "dispatcher enabled; tenant pipeline execution runs in worker pools"
            );
            let dispatcher = Arc::new(
                Dispatcher::new(control.clone(), Arc::clone(&cache), config)
                    .with_plan_registry(dispatcher_plan_registry),
            );
            tokio::spawn(dispatcher.run_loop());
        }
        None => {
            tracing::warn!(
                "dispatcher disabled: background work (feeds, reports, scheduled tasks) \
                 will not run in this process; tenant pipelines execute inline"
            );
        }
    }

    // Must outlive the server: it owns the scratch directory backing the
    // inert fallback AppState (see server.rs module docs).
    let fallback = FallbackAppState::build()?;
    let state = fallback.data();
    // One MCP transport per process (one shared session manager), cloned into
    // every worker below. Behind CloudAuth it resolves the tenant's manager
    // per request (server.rs::FallbackAppState::mcp_transport).
    let mcp_transport = fallback.mcp_transport(atomic_cloud::DEFAULT_MCP_SSE_KEEP_ALIVE);

    tracing::info!("Atomic Cloud starting...");
    tracing::info!(base_domain, "accounts served under *.{base_domain}");
    tracing::info!(
        base_domain,
        "account plane (signup/login) on {base_domain} and app.{base_domain}"
    );
    // Load the account-plane SPA (signup/login + the `/account/*` dashboard)
    // to serve as the fallback route. Absent (no `index.html` under `spa_dir`)
    // is a clean degrade — a pure-API pod or an un-built dev run boots without
    // the fallback and unmatched paths 404. A directory that *exists* but is
    // unreadable is a hard boot error (a real misconfiguration).
    let spa = atomic_cloud::SpaServer::load_optional(&spa_dir, &base_domain).await?;
    match &spa {
        Some(_) => tracing::info!(spa_dir = %spa_dir.display(), "serving account-plane SPA"),
        None => tracing::warn!(
            spa_dir = %spa_dir.display(),
            "no built SPA found (run `npm run build` in crates/atomic-cloud/frontend); \
             serving API only"
        ),
    }
    // Optionally attach the product app (`dist-web`) to serve at the tenant
    // root — a local/dev convenience so the dashboard's "Open knowledge base"
    // link reaches the real product app on the same origin. Requires the
    // account SPA to be present (it's the host that holds the product app);
    // without `--product-dir` the tenant root falls back to the dashboard.
    let spa = match (spa, &product_dir) {
        (Some(spa), Some(dir)) => {
            let spa = spa.with_product_dir(dir).await?;
            tracing::info!(product_dir = %dir.display(), "serving product app at the tenant root");
            Some(spa)
        }
        (spa, _) => spa,
    };

    tracing::info!(bind, port, "listening on http://{bind}:{port}");
    tracing::info!(bind, port, "health: http://{bind}:{port}/health");

    let server = HttpServer::new(move || {
        App::new()
            .configure(configure_cloud_app(
                state.clone(),
                auth.clone(),
                account_plane.clone(),
                tenant_plane.clone(),
                oauth_plane.clone(),
                mcp_transport.clone(),
                control.clone(),
                chat_streams.clone(),
                readiness.clone(),
                quota_billing.clone(),
                spa.clone(),
            ))
            // Same browser-origin policy as the self-hosted server: local app
            // shells and browser-extension pages (the web clipper) may call
            // the API cross-origin with a Bearer token. Answers preflights
            // before CloudAuth sees them (an OPTIONS carries no Authorization
            // and would otherwise 401). No public origin: same-origin pages
            // need no CORS, and credentials stay unsupported so the
            // session-cookie plane remains same-origin.
            .wrap(atomic_server::cors::build_cors(None))
    })
    .workers(4)
    .bind((bind.as_str(), port))?
    .run();

    tokio::select! {
        result = server => result?,
        _ = reaper_loop => unreachable!("the reaper loop never returns"),
    }

    Ok(())
}

/// Failure-recovery reaper loop (plan: "Failure recovery & the reaper").
/// The pass semantics — per-account advisory locks, resume-then-rollback,
/// orphan reclaim, hygiene purges — live in (and are tested through)
/// [`atomic_cloud::reaper::run_reaper_pass`]; this is interval glue around
/// it. The jittered start keeps a fleet of pods booted together from
/// synchronizing their passes (they'd be safe anyway — contended rows skip
/// via the advisory locks — just wasteful). Never returns; the caller
/// select!s it against the server so it lives exactly as long as serving.
async fn run_reaper_loop(
    control: ControlPlane,
    cluster: ClusterConfig,
    managed: ManagedKeys,
    reaper_interval: std::time::Duration,
    fleet_config: FleetMigrationConfig,
    backup_store: Arc<dyn atomic_cloud::BackupStore>,
    backup_timeout: std::time::Duration,
) {
    // The failed-migrations arm retries through the boot fleet runner's
    // per-tenant step; handing it the serve-level `--fleet-*` config keeps
    // the two writers of migration_retry_after on one backoff schedule. The
    // backup store lets arm 3 take the final pre-drop dump when it completes
    // an interrupted deletion (plan: "Account deletion" step 4), bounded by the
    // same per-dump timeout the nightly pass uses.
    let policy = atomic_cloud::ReaperPolicy {
        migration_retry: fleet_config,
        backup_store: Some(backup_store),
        backup_timeout,
        ..atomic_cloud::ReaperPolicy::default()
    };
    // `--reaper-interval-secs 0` disables the reaper (dev only): a local box
    // doesn't need stuck-provision recovery or orphan reclaim, and orphan
    // reclaim on a cluster SHARED with the test suite would drop the suite's
    // `acct_*` tenant databases (they're orphans relative to this control
    // plane). Pend forever rather than return — completing would fire the
    // serve `select!` arm and stop the server.
    if reaper_interval.is_zero() {
        tracing::warn!("reaper disabled (--reaper-interval-secs 0); dev only");
        std::future::pending::<()>().await;
        return;
    }
    let jitter = std::time::Duration::from_millis(rand::Rng::gen_range(
        &mut rand::thread_rng(),
        0..=reaper_interval.as_millis() as u64,
    ));
    tokio::time::sleep(jitter).await;
    let mut ticker = tokio::time::interval(reaper_interval);
    loop {
        ticker.tick().await;
        let summary = atomic_cloud::run_reaper_pass(&control, &cluster, &managed, &policy).await;
        if summary.is_quiet() {
            tracing::debug!("reaper pass: nothing to do");
        } else {
            tracing::info!(?summary, "reaper pass");
        }
    }
}

/// Nightly backup loop (plan: "Backups & disaster recovery" → nightly logical
/// dumps). The pass semantics — per-tenant advisory locks, control-plane dump,
/// the observable [`BackupSummary`](atomic_cloud::BackupSummary) and
/// `backup_runs` ledger row — live in (and are tested through)
/// [`atomic_cloud::run_backup_pass`]; this is interval glue around it. The
/// jittered start keeps a fleet of pods booted together from synchronizing
/// their passes (they'd be safe anyway — contended tenants skip via the
/// advisory locks — just wasteful). After each pass it runs the staleness
/// monitor (plan: "alert when any tenant's last successful backup is >36h
/// old"): an unmonitored backup job is a placebo.
async fn run_backup_loop(
    control: ControlPlane,
    cluster: ClusterConfig,
    store: Arc<dyn atomic_cloud::BackupStore>,
    config: atomic_cloud::BackupConfig,
    interval: std::time::Duration,
) {
    let jitter = std::time::Duration::from_millis(rand::Rng::gen_range(
        &mut rand::thread_rng(),
        0..=interval.as_millis() as u64,
    ));
    tokio::time::sleep(jitter).await;
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let now = chrono::Utc::now();
        let summary = atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now).await;
        if summary.is_quiet() {
            tracing::debug!("backup pass: nothing to do");
        } else {
            tracing::info!(?summary, "backup pass");
        }

        // Staleness alert. A tenant whose last successful backup is older than
        // the horizon (or who has never been backed up and is past it) is the
        // signal that the backup job is failing for that tenant — escalate at
        // error level so it pages, not just logs.
        match atomic_cloud::stale_tenant_backups(&control, config.staleness_horizon).await {
            Ok(stale) if !stale.is_empty() => {
                for tenant in &stale {
                    tracing::error!(
                        account_id = tenant.account_id,
                        db_name = tenant.db_name,
                        last_backup_at = tenant
                            .last_backup_at
                            .map(|t| t.to_rfc3339())
                            .unwrap_or_else(|| "never".into()),
                        "tenant backup is stale (no successful backup within the staleness \
                         horizon); the nightly backup job is failing for this tenant"
                    );
                }
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "backup staleness check failed"),
        }
    }
}
