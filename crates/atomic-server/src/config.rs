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

        /// Passphrase for SQLCipher database encryption (SQLite only).
        /// When set, all SQLite databases are encrypted at rest.
        /// Omit for unencrypted databases.
        #[arg(long, env = "ATOMIC_PASSPHRASE")]
        passphrase: Option<String>,
    },

    /// Manage API tokens
    Token {
        #[command(subcommand)]
        action: TokenAction,
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
