//! Atomic Server — standalone HTTP server for the Atomic knowledge base
//!
//! Wraps atomic-core with a REST API + WebSocket events.
//! No Tauri dependency.

mod config;

use actix_cors::Cors;
use actix_web::{middleware, web, App, HttpResponse, HttpServer, Responder};
use atomic_server::{
    auth, event_bridge, export_jobs::ExportJobManager, log_buffer::LogBuffer, mcp, mcp_auth,
    routes, state::AppState, ws, Scalar, Servable,
};
use clap::Parser;
use config::{Cli, Command, TokenAction};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp_actix_web::transport::StreamableHttpService;
use std::sync::Arc;
use std::time::Duration;
use utoipa::OpenApi;

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
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(log_buffer.make_writer()),
        ) // ring buffer
        .init();

    let cli = Cli::parse();
    let data_dir = cli.resolve_data_dir();

    match cli.command {
        // Token management subcommands (no server needed)
        Some(Command::Token {
            storage,
            database_url,
            action,
        }) => {
            let manager = create_manager(&data_dir, &storage, database_url.as_deref()).await;
            let core = manager
                .active_core()
                .await
                .expect("Failed to get active database");
            run_token_command(&core, action).await;
            Ok(())
        }

        // Server mode
        Some(Command::Serve {
            port,
            bind,
            public_url,
            storage,
            database_url,
        }) => {
            // Auto-detect public URL on Fly.io if not explicitly set
            let public_url = public_url.or_else(|| {
                std::env::var("FLY_APP_NAME")
                    .ok()
                    .map(|name| format!("https://{name}.fly.dev"))
            });
            let manager = create_manager(&data_dir, &storage, database_url.as_deref()).await;
            run_server(
                manager,
                &data_dir.display().to_string(),
                port,
                &bind,
                public_url,
                log_buffer,
            )
            .await
        }
        None => {
            let manager = create_manager(&data_dir, "sqlite", None).await;
            run_server(
                manager,
                &data_dir.display().to_string(),
                8080,
                "127.0.0.1",
                None,
                log_buffer,
            )
            .await
        }
    }
}

/// Create a DatabaseManager based on the chosen storage backend.
async fn create_manager(
    data_dir: &std::path::Path,
    storage: &str,
    database_url: Option<&str>,
) -> atomic_core::DatabaseManager {
    match storage {
        "postgres" => {
            let url = database_url.unwrap_or_else(|| {
                tracing::error!("--database-url is required when --storage=postgres");
                tracing::error!(
                    "Example: --database-url postgres://user:pass@localhost:5432/atomic"
                );
                tracing::error!("Or set ATOMIC_DATABASE_URL environment variable.");
                std::process::exit(1);
            });
            tracing::info!(
                backend = "postgres",
                host = url.split('@').last().unwrap_or(url),
                "storage backend selected"
            );
            atomic_core::DatabaseManager::new_postgres(data_dir, url)
                .await
                .expect("Failed to connect to Postgres")
        }
        _ => {
            tracing::info!(backend = "sqlite", path = %data_dir.display(), "storage backend selected");
            atomic_core::DatabaseManager::new(data_dir).expect("Failed to open database manager")
        }
    }
}

async fn run_token_command(core: &atomic_core::AtomicCore, action: TokenAction) {
    match action {
        TokenAction::Create { name } => match core.create_api_token(&name).await {
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
        },
        TokenAction::List => {
            match core.list_api_tokens().await {
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
        TokenAction::Revoke { id } => match core.revoke_api_token(&id).await {
            Ok(()) => println!("Token {} revoked.", id),
            Err(e) => {
                eprintln!("Failed to revoke token: {}", e);
                std::process::exit(1);
            }
        },
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

    // Get active core for startup tasks
    let core = manager
        .active_core()
        .await
        .expect("Failed to get active database");

    // Migrate legacy token if present
    match core.migrate_legacy_token().await {
        Ok(true) => tracing::info!("migrated legacy auth token to new token system"),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "failed to migrate legacy token"),
    }

    // Check token status
    match core.list_api_tokens().await {
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

    // Create broadcast channel for WebSocket events. Bulk imports can produce
    // dense atom + pipeline bursts, so keep enough room for slower clients to
    // avoid losing the first queue status events.
    let (event_tx, _) = tokio::sync::broadcast::channel(4096);
    let export_jobs = ExportJobManager::new(std::path::Path::new(data_dir).join("exports"))
        .expect("Failed to initialize export job manager");

    let app_state = web::Data::new(AppState {
        manager: Arc::clone(&manager),
        event_tx: event_tx.clone(),
        public_url: public_url.clone(),
        log_buffer,
        export_jobs,
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
            let db_id = http_req.query_string().split('&').find_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                if parts.next()? == "db" {
                    parts.next().map(String::from)
                } else {
                    None
                }
            });
            ext.insert(mcp::DbSelection(db_id));
        })
        .session_manager(Arc::new(LocalSessionManager::default()))
        .stateful_mode(true)
        .sse_keep_alive(Duration::from_secs(30))
        .build();

    tracing::info!("Atomic Server starting...");
    tracing::info!(data_dir = data_dir, "data directory");
    tracing::info!(
        bind = bind,
        port = port,
        "listening on http://{}:{}",
        bind,
        port
    );
    if let Some(ref url) = public_url {
        tracing::info!(public_url = %url, "public URL configured");
    }
    tracing::info!(
        bind = bind,
        port = port,
        "health: http://{}:{}/health",
        bind,
        port
    );
    tracing::info!(
        bind = bind,
        port = port,
        "MCP: http://{}:{}/mcp",
        bind,
        port
    );
    tracing::info!(
        bind = bind,
        port = port,
        "WebSocket: ws://{}:{}/ws?token=<token>",
        bind,
        port
    );

    // Startup recovery: reset stuck atoms and process any pending work for ALL databases
    {
        let (databases, _active_id) = manager.list_databases().await.unwrap_or_default();
        for db_info in &databases {
            let db_core = match manager.get_core(&db_info.id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(db = %db_info.name, error = %e, "failed to load database");
                    continue;
                }
            };
            let on_event = event_bridge::embedding_event_callback(app_state.event_tx.clone());

            match db_core.reset_stuck_processing().await {
                Ok(count) if count > 0 => {
                    tracing::info!(db = %db_info.name, count = count, "reset atoms stuck in processing state")
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(db = %db_info.name, error = %e, "failed to reset stuck processing")
                }
            }

            match db_core.process_pending_embeddings(on_event.clone()).await {
                Ok(count) if count > 0 => {
                    tracing::info!(db = %db_info.name, count = count, "processing pending embeddings in background")
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(db = %db_info.name, error = %e, "failed to start pending embeddings")
                }
            }

            match db_core.process_pending_tagging(on_event).await {
                Ok(count) if count > 0 => {
                    tracing::info!(db = %db_info.name, count = count, "processing pending tagging operations in background")
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(db = %db_info.name, error = %e, "failed to start pending tagging")
                }
            }

            match db_core.process_pending_edges().await {
                Ok(count) if count > 0 => {
                    tracing::info!(db = %db_info.name, count = count, "processing pending edge computation in background")
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(db = %db_info.name, error = %e, "failed to start pending edge computation")
                }
            }
        }
    }

    // Canvas cache warmup: compute the global canvas payload for every
    // database in the background so the first request after startup hits a
    // warm cache. Sequenced across databases (not parallel) to avoid an
    // N-way PCA spike on startup, and off-loaded to the blocking pool so it
    // never ties up an async worker.
    {
        let warm_manager = Arc::clone(&manager);
        tokio::spawn(async move {
            let (databases, _) = match warm_manager.list_databases().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "canvas warmup: failed to list databases");
                    return;
                }
            };
            for db_info in &databases {
                let db_core = match warm_manager.get_core(&db_info.id).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(db = %db_info.name, error = %e, "canvas warmup: failed to load database");
                        continue;
                    }
                };
                let db_name = db_info.name.clone();
                let started = std::time::Instant::now();
                match db_core.compute_and_get_canvas_data().await {
                    Ok(data) => tracing::info!(
                        db = %db_name,
                        atoms = data.atoms.len(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "canvas cache warmed"
                    ),
                    Err(e) => {
                        tracing::warn!(db = %db_name, error = %e, "canvas cache warmup failed")
                    }
                }
            }
        });
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
                let databases = match poll_manager.list_databases().await {
                    Ok((dbs, _)) => dbs,
                    Err(_) => continue,
                };
                for db_info in &databases {
                    let db_core = match poll_manager.get_core(&db_info.id).await {
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

    // Spawn scheduled-tasks runner (ticks every 15 seconds across all databases).
    // Each registered task checks its own due-ness and state; we just hand it
    // a core + context. A per-(task, db) lock in the registry prevents the
    // next tick from re-entering a still-running task.
    {
        let task_manager = Arc::clone(&manager);
        let task_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut registry = atomic_core::scheduler::TaskRegistry::new();
            registry.register(Arc::new(atomic_core::briefing::DailyBriefingTask));
            registry.register(Arc::new(atomic_core::pipeline_task::DraftPipelineTask));
            registry.register(Arc::new(
                atomic_core::graph_maintenance::GraphMaintenanceTask,
            ));
            registry.register(Arc::new(
                atomic_core::health::task::HealthMaintenanceTask,
            ));
            let registry = Arc::new(registry);

            let mut interval = tokio::time::interval(Duration::from_secs(15));
            interval.tick().await;
            loop {
                interval.tick().await;
                let databases = match task_manager.list_databases().await {
                    Ok((dbs, _)) => dbs,
                    Err(_) => continue,
                };
                for db_info in &databases {
                    let db_core = match task_manager.get_core(&db_info.id).await {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    for task in registry.tasks() {
                        let Some(guard) = registry.try_lock(task.id(), &db_info.id) else {
                            continue;
                        };
                        let task_clone = Arc::clone(task);
                        let db_core_clone = db_core.clone();
                        let tx = task_tx.clone();
                        let embedding_tx = task_tx.clone();
                        let db_id = db_info.id.clone();
                        tokio::spawn(async move {
                            let ctx = atomic_core::scheduler::TaskContext {
                                event_cb: event_bridge::task_event_callback(tx),
                                embedding_event_cb: Arc::new(
                                    event_bridge::embedding_event_callback(embedding_tx),
                                ),
                            };
                            if let Err(e) = task_clone.run(&db_core_clone, &ctx).await {
                                tracing::debug!(
                                    task = task_clone.id(),
                                    db = %db_id,
                                    error = %e,
                                    "task run ended"
                                );
                            }
                            drop(guard);
                        });
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
            .route(
                "/api/docs/openapi.json",
                web::get().to(atomic_server::openapi_spec),
            )
            .service(Scalar::with_url(
                "/api/docs",
                atomic_server::ApiDoc::openapi(),
            ))
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
            .route(
                "/api/setup/status",
                web::get().to(routes::setup::setup_status),
            )
            .route(
                "/api/setup/claim",
                web::post().to(routes::setup::claim_instance),
            )
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
            .route(
                "/api/exports/{id}/download",
                web::get().to(routes::exports::download_export),
            )
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
