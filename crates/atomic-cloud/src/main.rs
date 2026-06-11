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
//! (`migrate`, `account`, `token`) deliberately never load the vault: none
//! of them read or write `encrypted_key` (managed-key lifecycle calls use
//! the cleartext `external_key_id`; deletion cascades the rows), so
//! operator tooling stays runnable from hosts that don't hold the
//! production master key.

use std::sync::Arc;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, delete_account, issue_token, provision_account, AccountCache,
    AccountCacheConfig, AccountPlane, AccountPlaneConfig, CloudAuth, ClusterConfig, ControlPlane,
    EmailSender, EnvMasterKeyVault, FallbackAppState, KeyVault, LogSender, MailgunSender,
    NewAccount, RateLimits, TenantPlane, TokenScope,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

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

        /// How often the periodic idle sweep of the account cache runs, in
        /// seconds. Defaults to a quarter of the cache idle TTL.
        #[arg(long, env = "ATOMIC_CLOUD_CACHE_SWEEP_INTERVAL_SECS")]
        cache_sweep_interval_secs: Option<u64>,

        /// How often the failure-recovery reaper runs, in seconds (plan:
        /// "Failure recovery & the reaper"). Each pass resumes or rolls
        /// back stuck provisions, reclaims orphaned tenant databases,
        /// clears self-reservations, and purges expired links/sessions/
        /// reservations. Per-account advisory locks make concurrent passes
        /// across pods safe.
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

        /// Max signups provisioning concurrently in this process; further
        /// signup completions get a 503 + Retry-After without consuming
        /// their link (the plan budgets 4-8).
        #[arg(
            long,
            env = "ATOMIC_CLOUD_MAX_CONCURRENT_PROVISIONS",
            default_value_t = atomic_cloud::DEFAULT_MAX_CONCURRENT_PROVISIONS
        )]
        max_concurrent_provisions: usize,
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

/// Email delivery selection for `serve`. Mailgun mode requires all three
/// credentials; the API key is env-only by convention (it's a secret).
#[derive(Args)]
struct EmailArgs {
    /// Email delivery mode for magic links.
    #[arg(long, env = "ATOMIC_CLOUD_EMAIL_MODE", default_value = "log")]
    email_mode: EmailMode,

    /// Mailgun API key (required with --email-mode mailgun).
    #[arg(long, env = "ATOMIC_CLOUD_MAILGUN_API_KEY", hide_env_values = true)]
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
                let api_key = self
                    .mailgun_api_key
                    .ok_or_else(|| missing("--mailgun-api-key"))?;
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

        /// Subdomain of the account to delete.
        #[arg(long)]
        subdomain: String,
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
async fn connect_control(control_url: &str) -> Result<ControlPlane, Box<dyn std::error::Error>> {
    let control = ControlPlane::connect(control_url).await?;
    let applied = control.initialize().await?;
    if applied > 0 {
        tracing::info!(applied, "applied control-plane migrations");
    }
    Ok(control)
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let control = connect_control(&cli.control_url).await?;

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
            cache_sweep_interval_secs,
            reaper_interval_secs,
            email,
            master_key_env,
            trust_proxy_header,
            app_public_url,
            max_concurrent_provisions,
        } => {
            // Boot-time master-key check (plan: "Encryption at rest").
            // Constructing the vault validates the key, so a deployment
            // with a missing or malformed master key dies here with a
            // message naming the variable — never on the first signup.
            // The slice's managed-key and BYOK wiring receives this vault;
            // the boot contract it pins is: serve does not start unless
            // stored provider credentials are decryptable.
            let _vault: Arc<dyn KeyVault> = Arc::new(EnvMasterKeyVault::from_env(&master_key_env)?);

            let cache_config = AccountCacheConfig {
                tenant_pool_max_connections,
                tenant_pool_idle_timeout: std::time::Duration::from_secs(
                    tenant_pool_idle_timeout_secs,
                ),
                ..AccountCacheConfig::default()
            };
            let plane_config = AccountPlaneConfig {
                app_public_url,
                trust_proxy_header,
                rate_limits: RateLimits::default(),
                max_concurrent_provisions,
                ..AccountPlaneConfig::new(base_domain.clone())
            };
            serve(
                control,
                cluster.into_config(),
                base_domain,
                bind,
                port,
                cache_config,
                cache_sweep_interval_secs.map(std::time::Duration::from_secs),
                // tokio::time::interval panics on a zero period; clamp.
                std::time::Duration::from_secs(reaper_interval_secs.max(1)),
                email.into_sender()?,
                plane_config,
            )
            .await
        }

        Command::Account { action } => match action {
            AccountAction::Create {
                cluster,
                email,
                subdomain,
            } => {
                let account = provision_account(
                    &control,
                    &cluster.into_config(),
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

            AccountAction::Delete { cluster, subdomain } => {
                let account_id = control
                    .account_id_by_subdomain(&subdomain)
                    .await?
                    .ok_or_else(|| format!("no account with subdomain {subdomain:?}"))?;
                delete_account(&control, &cluster.into_config(), &account_id).await?;
                println!("deleted account {account_id} ({subdomain})");
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
    }
}

/// Run the composed multi-tenant server until interrupted. See
/// [`atomic_cloud::server`] for what the composition serves (and what it
/// deliberately doesn't until later slices).
///
/// `sweep_interval` controls the periodic account-cache sweep; `None` means
/// a quarter of the cache's idle TTL. `reaper_interval` paces the
/// failure-recovery reaper.
#[allow(clippy::too_many_arguments)] // CLI assembly; every argument is a distinct serve knob.
async fn serve(
    control: ControlPlane,
    cluster: ClusterConfig,
    base_domain: String,
    bind: String,
    port: u16,
    cache_config: AccountCacheConfig,
    sweep_interval: Option<std::time::Duration>,
    reaper_interval: std::time::Duration,
    email: Arc<dyn EmailSender>,
    plane_config: AccountPlaneConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let sweep_interval = sweep_interval
        .unwrap_or(cache_config.idle_ttl / 4)
        .max(std::time::Duration::from_secs(1));
    let cache = Arc::new(AccountCache::new(
        control.clone(),
        cluster.clone(),
        cache_config,
    ));
    let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), &base_domain);
    let tenant_plane = TenantPlane::new(control.clone(), cluster.clone(), Arc::clone(&cache));

    // The reaper loop runs concurrently with the server below via select!,
    // not tokio::spawn: spawn's Send bound trips rustc's
    // "implementation is not general enough" higher-ranked lifetime false
    // positive on provision_account's sqlx futures (the same one
    // tests/provisioning.rs works around with join!), and select! on the
    // main task needs no Send while also tying the reaper's lifetime to
    // serve's.
    let reaper_loop = run_reaper_loop(control.clone(), cluster.clone(), reaper_interval);

    let account_plane = AccountPlane::new(control, cluster, email, plane_config)?;

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

    // Must outlive the server: it owns the scratch directory backing the
    // inert fallback AppState (see server.rs module docs).
    let fallback = FallbackAppState::build()?;
    let state = fallback.data();

    tracing::info!("Atomic Cloud starting...");
    tracing::info!(base_domain, "accounts served under *.{base_domain}");
    tracing::info!(
        base_domain,
        "account plane (signup/login) on {base_domain} and app.{base_domain}"
    );
    tracing::info!(bind, port, "listening on http://{bind}:{port}");
    tracing::info!(bind, port, "health: http://{bind}:{port}/health");

    let server = HttpServer::new(move || {
        App::new().configure(configure_cloud_app(
            state.clone(),
            auth.clone(),
            account_plane.clone(),
            tenant_plane.clone(),
        ))
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
    reaper_interval: std::time::Duration,
) {
    let policy = atomic_cloud::ReaperPolicy::default();
    let jitter = std::time::Duration::from_millis(rand::Rng::gen_range(
        &mut rand::thread_rng(),
        0..=reaper_interval.as_millis() as u64,
    ));
    tokio::time::sleep(jitter).await;
    let mut ticker = tokio::time::interval(reaper_interval);
    loop {
        ticker.tick().await;
        let summary = atomic_cloud::run_reaper_pass(&control, &cluster, &policy).await;
        if summary.is_quiet() {
            tracing::debug!("reaper pass: nothing to do");
        } else {
            tracing::info!(?summary, "reaper pass");
        }
    }
}
