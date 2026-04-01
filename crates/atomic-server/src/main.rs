//! Atomic Server — standalone HTTP server for the Atomic knowledge base
//!
//! Wraps atomic-core with a REST API + WebSocket events.
//! No Tauri dependency.

mod config;

use actix_cors::Cors;
use actix_web::{middleware, web, App, HttpResponse, HttpServer, Responder};
use atomic_server::{auth, event_bridge, log_buffer::LogBuffer, mcp, mcp_auth, routes, state::AppState, ws, Scalar, Servable};
use utoipa::OpenApi;
use clap::Parser;
use config::{Cli, Command, TokenAction};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp_actix_web::transport::StreamableHttpService;
use std::sync::Arc;
use std::time::Duration;

async fn health() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let log_buffer = LogBuffer::new(1000);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "atomic_core=info,atomic_server=info,warn".parse().unwrap());

    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer()) // console output
        .with(fmt::layer().with_ansi(false).with_writer(log_buffer.make_writer())) // ring buffer
        .init();

    let cli = Cli::parse();
    let data_dir = cli.resolve_data_dir();

    match cli.command {
        // Token management subcommands (no server needed)
        Some(Command::Token { action }) => {
            let manager = atomic_core::DatabaseManager::new(&data_dir)
                .expect("Failed to open database manager");
            let core = manager.active_core()
                .expect("Failed to get active database");
            run_token_command(&core, action);
            Ok(())
        }

        // Server mode
        Some(Command::Serve { port, bind, public_url, storage, database_url, passphrase }) => {
            // Auto-detect public URL on Fly.io if not explicitly set
            let public_url = public_url.or_else(|| {
                std::env::var("FLY_APP_NAME").ok().map(|name| format!("https://{name}.fly.dev"))
            });
            let manager = create_manager(&data_dir, &storage, database_url.as_deref(), passphrase);
            run_server(manager, &data_dir.display().to_string(), port, &bind, public_url, log_buffer).await
        }
        None => {
            let manager = create_manager(&data_dir, "sqlite", None, None);
            run_server(manager, &data_dir.display().to_string(), 8080, "127.0.0.1", None, log_buffer).await
        }
    }
}

/// Create a DatabaseManager based on the chosen storage backend.
fn create_manager(
    data_dir: &std::path::Path,
    storage: &str,
    database_url: Option<&str>,
    passphrase: Option<String>,
) -> atomic_core::DatabaseManager {
    match storage {
        "postgres" => {
            if passphrase.is_some() {
                tracing::warn!("--passphrase is ignored in Postgres mode (encryption is managed by Postgres)");
            }
            let url = database_url.unwrap_or_else(|| {
                tracing::error!("--database-url is required when --storage=postgres");
                tracing::error!("Example: --database-url postgres://user:pass@localhost:5432/atomic");
                tracing::error!("Or set ATOMIC_DATABASE_URL environment variable.");
                std::process::exit(1);
            });
            tracing::info!(backend = "postgres", host = url.split('@').last().unwrap_or(url), "storage backend selected");
            atomic_core::DatabaseManager::new_postgres(data_dir, url)
                .expect("Failed to connect to Postgres")
        }
        _ => {
            let registry_path = data_dir.join("registry.db");
            let is_fresh = !registry_path.exists();

            tracing::info!(backend = "sqlite", path = %data_dir.display(), "storage backend selected");

            if let Some(pp) = passphrase {
                // Explicit passphrase — open/create databases immediately
                tracing::info!("encryption enabled (SQLCipher)");
                atomic_core::DatabaseManager::new_encrypted(data_dir, Some(pp))
                    .expect("Failed to open database manager")
            } else if is_fresh {
                // No databases exist yet and no passphrase given — defer creation
                // until the user completes setup (where they can optionally set a passphrase)
                tracing::info!("no databases found — waiting for setup");
                atomic_core::DatabaseManager::new_deferred(data_dir)
            } else {
                // Databases exist, no passphrase — try opening unencrypted.
                // If they're encrypted, start in locked/deferred mode and
                // serve an unlock endpoint for the UI to provide the passphrase.
                match atomic_core::DatabaseManager::new_encrypted(data_dir, None) {
                    Ok(manager) => manager,
                    Err(e) => {
                        // SQLCipher returns "file is not a database" when the key is wrong.
                        // Distinguish this from genuine corruption or permission errors.
                        let msg = e.to_string();
                        if msg.contains("not a database") || msg.contains("file is encrypted") {
                            tracing::info!("database is encrypted — waiting for unlock via UI");
                            atomic_core::DatabaseManager::new_deferred(data_dir)
                        } else {
                            tracing::error!(error = %e, "failed to open database — this may indicate file corruption or a permissions issue");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    }
}

fn run_token_command(core: &atomic_core::AtomicCore, action: TokenAction) {
    match action {
        TokenAction::Create { name } => {
            match core.create_api_token(&name) {
                Ok((info, raw_token)) => {
                    println!("Token created:");
                    println!("  ID:     {}", info.id);
                    println!("  Name:   {}", info.name);
                    println!("  Token:  {}", raw_token);
                    println!();
                    println!("Save this token — it won't be shown again.");
                }
                Err(e) => {
                    eprintln!("Failed to create token: {}", e);
                    std::process::exit(1);
                }
            }
        }
        TokenAction::List => {
            match core.list_api_tokens() {
                Ok(tokens) => {
                    if tokens.is_empty() {
                        println!("No API tokens found.");
                    } else {
                        println!(
                            "{:<38} {:<24} {:<12} {:<22} {:<22} {}",
                            "ID", "NAME", "PREFIX", "CREATED", "LAST USED", "STATUS"
                        );
                        for t in &tokens {
                            let status = if t.is_revoked { "REVOKED" } else { "active" };
                            let last_used = t.last_used_at.as_deref().unwrap_or("never");
                            // Truncate timestamps to 19 chars (drop timezone)
                            let created = &t.created_at[..t.created_at.len().min(19)];
                            let last_used = &last_used[..last_used.len().min(19)];
                            println!(
                                "{:<38} {:<24} {:<12} {:<22} {:<22} {}",
                                t.id, t.name, t.token_prefix, created, last_used, status
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to list tokens: {}", e);
                    std::process::exit(1);
                }
            }
        }
        TokenAction::Revoke { id } => {
            match core.revoke_api_token(&id) {
                Ok(()) => println!("Token {} revoked.", id),
                Err(e) => {
                    eprintln!("Failed to revoke token: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

async fn run_server(
    manager: atomic_core::DatabaseManager,
    data_dir: &str,
    port: u16,
    bind: &str,
    public_url: Option<String>,
    log_buffer: LogBuffer,
) -> std::io::Result<()> {
    let manager = Arc::new(manager);

    // Run startup tasks only if databases are initialized
    if manager.is_initialized() {
        let core = manager.active_core().expect("Failed to get active database");

        // Migrate legacy token if present
        match core.migrate_legacy_token() {
            Ok(true) => tracing::info!("migrated legacy auth token to new token system"),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "failed to migrate legacy token"),
        }

        // Check token status
        match core.list_api_tokens() {
            Ok(tokens) => {
                let active = tokens.iter().filter(|t| !t.is_revoked).count();
                if active == 0 {
                    tracing::info!("no API tokens configured — open the web UI to claim this instance or run: atomic-server token create --name default");
                } else {
                    tracing::info!(count = active, "active API tokens configured");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to check tokens"),
        }
    } else {
        tracing::info!("waiting for setup — visit the web UI to configure this instance");
    }

    // Create broadcast channel for WebSocket events (buffer 256 events)
    let (event_tx, _) = tokio::sync::broadcast::channel(256);

    let app_state = web::Data::new(AppState {
        manager: Arc::clone(&manager),
        event_tx: event_tx.clone(),
        public_url: public_url.clone(),
        log_buffer,
    });

    // Create MCP service with multi-database support via ?db= query param
    let mcp_manager = Arc::clone(&manager);
    let mcp_tx = event_tx.clone();
    let mcp_service = StreamableHttpService::builder()
        .service_factory(Arc::new(move || {
            Ok(mcp::AtomicMcpServer::new(
                Arc::clone(&mcp_manager),
                mcp_tx.clone(),
            ))
        }))
        .on_request_fn(|http_req, ext| {
            let db_id = http_req
                .query_string()
                .split('&')
                .find_map(|pair| {
                    let mut parts = pair.splitn(2, '=');
                    if parts.next()? == "db" { parts.next().map(String::from) } else { None }
                });
            ext.insert(mcp::DbSelection(db_id));
        })
        .session_manager(Arc::new(LocalSessionManager::default()))
        .stateful_mode(true)
        .sse_keep_alive(Duration::from_secs(30))
        .build();

    tracing::info!("Atomic Server starting...");
    tracing::info!(data_dir = data_dir, "data directory");
    tracing::info!(bind = bind, port = port, "listening on http://{}:{}", bind, port);
    if let Some(ref url) = public_url {
        tracing::info!(public_url = %url, "public URL configured");
    }
    tracing::info!(bind = bind, port = port, "health: http://{}:{}/health", bind, port);
    tracing::info!(bind = bind, port = port, "MCP: http://{}:{}/mcp", bind, port);
    tracing::info!(bind = bind, port = port, "WebSocket: ws://{}:{}/ws?token=<token>", bind, port);

    // Startup recovery: reset stuck atoms and process any pending work for ALL databases
    if manager.is_initialized() {
        let (databases, _active_id) = manager.list_databases().unwrap_or_default();
        for db_info in &databases {
            let db_core = match manager.get_core(&db_info.id) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(db = %db_info.name, error = %e, "failed to load database");
                    continue;
                }
            };
            let on_event = event_bridge::embedding_event_callback(app_state.event_tx.clone());

            match db_core.reset_stuck_processing() {
                Ok(count) if count > 0 => tracing::info!(db = %db_info.name, count = count, "reset atoms stuck in processing state"),
                Ok(_) => {}
                Err(e) => tracing::warn!(db = %db_info.name, error = %e, "failed to reset stuck processing"),
            }

            match db_core.process_pending_embeddings(on_event.clone()) {
                Ok(count) if count > 0 => tracing::info!(db = %db_info.name, count = count, "processing pending embeddings in background"),
                Ok(_) => {}
                Err(e) => tracing::warn!(db = %db_info.name, error = %e, "failed to start pending embeddings"),
            }

            match db_core.process_pending_tagging(on_event) {
                Ok(count) if count > 0 => tracing::info!(db = %db_info.name, count = count, "processing pending tagging operations in background"),
                Ok(_) => {}
                Err(e) => tracing::warn!(db = %db_info.name, error = %e, "failed to start pending tagging"),
            }
        }
    }

    // Spawn feed polling scheduler (ticks every 60 seconds, polls all databases)
    {
        let poll_manager = Arc::clone(&manager);
        let poll_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await; // first tick fires immediately — skip it
            loop {
                interval.tick().await;
                let databases = match poll_manager.list_databases() {
                    Ok((dbs, _)) => dbs,
                    Err(_) => continue,
                };
                for db_info in &databases {
                    let db_core = match poll_manager.get_core(&db_info.id) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let on_ingest = event_bridge::ingestion_event_callback(poll_tx.clone());
                    let on_embed = event_bridge::embedding_event_callback(poll_tx.clone());
                    let results = db_core.poll_due_feeds(on_ingest, on_embed).await;
                    for r in &results {
                        if r.new_items > 0 {
                            tracing::info!(
                                db = %db_info.name,
                                feed_id = %r.feed_id,
                                new = r.new_items,
                                skipped = r.skipped,
                                errors = r.errors,
                                "feed poll complete"
                            );
                        }
                    }
                }
            }
        });
    }

    let bind_owned = bind.to_string();
    let shutdown_manager = Arc::clone(&manager);

    HttpServer::new(move || {
        let cors = Cors::permissive();

        App::new()
            .wrap(cors)
            .wrap(middleware::Compress::default())
            .app_data(app_state.clone())
            // Public routes (no auth)
            .route("/health", web::get().to(health))
            .route("/api/docs/openapi.json", web::get().to(atomic_server::openapi_spec))
            .service(Scalar::with_url("/api/docs", atomic_server::ApiDoc::openapi()))
            .route("/ws", web::get().to(ws::ws_handler))
            // OAuth discovery (public, no auth)
            .route(
                "/.well-known/oauth-authorization-server",
                web::get().to(routes::oauth::metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource",
                web::get().to(routes::oauth::resource_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                web::get().to(routes::oauth::resource_metadata),
            )
            // Instance setup (public, no auth — guarded by zero-token check)
            .route("/api/setup/status", web::get().to(routes::setup::setup_status))
            .route("/api/setup/claim", web::post().to(routes::setup::claim_instance))
            .route("/api/setup/unlock", web::post().to(routes::setup::unlock_instance))
            // OAuth flow (public, no auth)
            .route("/oauth/register", web::post().to(routes::oauth::register))
            .route(
                "/oauth/authorize",
                web::get().to(routes::oauth::authorize_page),
            )
            .route(
                "/oauth/authorize",
                web::post().to(routes::oauth::authorize_approve),
            )
            .route("/oauth/token", web::post().to(routes::oauth::token))
            // MCP endpoint with MCP-aware auth
            .service(
                web::scope("/mcp")
                    .wrap(mcp_auth::McpAuth {
                        state: app_state.clone(),
                    })
                    .service(mcp_service.clone().scope()),
            )
            // Authenticated API routes
            .service(
                web::scope("/api")
                    .wrap(auth::BearerAuth {
                        state: app_state.clone(),
                    })
                    .configure(routes::configure_routes),
            )
    })
    .workers(4)
    .bind((bind_owned.as_str(), port))?
    .run()
    .await?;

    // Graceful shutdown: update query planner statistics
    tracing::info!("shutting down — running PRAGMA optimize...");
    shutdown_manager.optimize_all();

    Ok(())
}
