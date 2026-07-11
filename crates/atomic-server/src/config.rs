//! Server configuration and CLI

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

/// Standalone HTTP server for the Atomic knowledge base
#[derive(Parser, Debug)]
#[command(name = "atomic-server", about = "Atomic knowledge base HTTP server")]
pub struct Cli {
    /// Path to the data directory containing registry.db and databases/
    #[arg(long, global = true)]
    pub data_dir: Option<String>,

    /// Path to the SQLite database file (deprecated, use --data-dir)
    #[arg(long, global = true)]
    pub db_path: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    /// Resolve the data directory from CLI args.
    /// Priority: --data-dir > derived from --db-path > current directory
    pub fn resolve_data_dir(&self) -> PathBuf {
        if let Some(ref dir) = self.data_dir {
            return PathBuf::from(dir);
        }
        if let Some(ref path) = self.db_path {
            // Derive data_dir from db_path's parent directory
            let p = Path::new(path);
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    return parent.to_path_buf();
                }
            }
        }
        PathBuf::from(".")
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the HTTP server (default if no subcommand given)
    Serve {
        /// Port to listen on
        #[arg(long, default_value_t = 8080)]
        port: u16,

        /// Address to bind to
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Public URL for OAuth discovery (e.g. https://atomic.example.com).
        /// Required for OAuth/MCP remote auth. Without this, OAuth endpoints return 404.
        /// Can also be set via PUBLIC_URL env var.
        #[arg(long, env = "PUBLIC_URL")]
        public_url: Option<String>,

        /// Storage backend: "sqlite" (default) or "postgres"
        #[arg(long, default_value = "sqlite", env = "ATOMIC_STORAGE")]
        storage: String,

        /// Postgres connection string (required when --storage=postgres).
        /// Example: postgres://user:pass@localhost:5432/atomic
        #[arg(long, env = "ATOMIC_DATABASE_URL")]
        database_url: Option<String>,

        /// Setup token required for first-run claims.
        /// Can also be set via ATOMIC_SETUP_TOKEN.
        #[arg(long, env = "ATOMIC_SETUP_TOKEN")]
        setup_token: Option<String>,

        /// Insecurely allow first-run claims without ATOMIC_SETUP_TOKEN.
        /// Intended only for trusted dev setups.
        #[arg(
            long,
            env = "ATOMIC_DANGEROUSLY_SKIP_SETUP_TOKEN",
            default_value_t = false
        )]
        dangerously_skip_setup_token: bool,
    },

    /// Manage API tokens
    Token {
        /// Storage backend: "sqlite" (default) or "postgres". Must match the
        /// backend the server is running against, since tokens live in storage.
        #[arg(long, default_value = "sqlite", env = "ATOMIC_STORAGE", global = true)]
        storage: String,

        /// Postgres connection string (required when --storage=postgres).
        #[arg(long, env = "ATOMIC_DATABASE_URL", global = true)]
        database_url: Option<String>,

        #[command(subcommand)]
        action: TokenAction,
    },

    /// Migrate data between storage backends
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum MigrateAction {
    /// Copy a local SQLite database file into a Postgres instance as a new
    /// logical database. Embeddings are not copied — the destination re-embeds
    /// with its own provider; chunk text is copied so keyword search works
    /// immediately.
    Sqlite {
        /// Path to the source SQLite database file (e.g. databases/default.db)
        #[arg(long)]
        source: String,

        /// Postgres connection string for the destination
        #[arg(long, env = "ATOMIC_DATABASE_URL")]
        database_url: String,

        /// Display name for the new database on the destination
        #[arg(long)]
        name: String,

        /// Validate and count rows without writing anything
        #[arg(long, default_value_t = false)]
        dry_run: bool,

        /// Land migrated feeds paused so the destination doesn't start
        /// polling while the source instance may still be running
        #[arg(long, default_value_t = false)]
        pause_feeds: bool,
    },

    /// Push a local SQLite database to a remote Atomic server (e.g. an
    /// Atomic Cloud tenant). The database is snapshotted and uploaded; the
    /// destination imports it as a new knowledge base and re-embeds with its
    /// own provider. The local database is left untouched.
    Push {
        /// Source database id or name; defaults to the default database
        #[arg(long)]
        database: Option<String>,

        /// Base URL of the destination server
        /// (e.g. https://you.atomicapp.ai)
        #[arg(long)]
        target_url: String,

        /// API token for the destination server. For Atomic Cloud this must
        /// be an account-scoped token (Settings → API tokens on your tenant).
        #[arg(long, env = "ATOMIC_MIGRATE_TARGET_TOKEN", hide_env_values = true)]
        target_token: String,

        /// Display name for the new database on the destination; defaults to
        /// the source database's name
        #[arg(long)]
        name: Option<String>,

        /// Land migrated feeds active on the destination. By default they
        /// arrive paused, so the destination doesn't start polling the same
        /// feeds while this instance keeps running.
        #[arg(long, default_value_t = false)]
        resume_feeds: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum TokenAction {
    /// Create a new API token
    Create {
        /// Human-readable name for the token
        #[arg(long)]
        name: String,
    },

    /// List all API tokens
    List,

    /// Revoke an API token by ID
    Revoke {
        /// Token ID to revoke
        id: String,
    },
}
