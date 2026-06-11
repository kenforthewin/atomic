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

use std::sync::Arc;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, delete_account, issue_token, provision_account, AccountCache,
    AccountCacheConfig, CloudAuth, ClusterConfig, ControlPlane, FallbackAppState, NewAccount,
    TokenScope,
};
use clap::{Args, Parser, Subcommand};

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
        } => {
            let cache_config = AccountCacheConfig {
                tenant_pool_max_connections,
                tenant_pool_idle_timeout: std::time::Duration::from_secs(
                    tenant_pool_idle_timeout_secs,
                ),
                ..AccountCacheConfig::default()
            };
            serve(
                control,
                cluster.into_config(),
                base_domain,
                bind,
                port,
                cache_config,
                cache_sweep_interval_secs.map(std::time::Duration::from_secs),
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
/// a quarter of the cache's idle TTL.
async fn serve(
    control: ControlPlane,
    cluster: ClusterConfig,
    base_domain: String,
    bind: String,
    port: u16,
    cache_config: AccountCacheConfig,
    sweep_interval: Option<std::time::Duration>,
) -> Result<(), Box<dyn std::error::Error>> {
    let sweep_interval = sweep_interval
        .unwrap_or(cache_config.idle_ttl / 4)
        .max(std::time::Duration::from_secs(1));
    let cache = Arc::new(AccountCache::new(control.clone(), cluster, cache_config));
    let auth = CloudAuth::new(control, Arc::clone(&cache), &base_domain);

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
    tracing::info!(bind, port, "listening on http://{bind}:{port}");
    tracing::info!(bind, port, "health: http://{bind}:{port}/health");

    HttpServer::new(move || App::new().configure(configure_cloud_app(state.clone(), auth.clone())))
        .workers(4)
        .bind((bind.as_str(), port))?
        .run()
        .await?;

    Ok(())
}
