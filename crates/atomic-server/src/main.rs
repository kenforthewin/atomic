//! Atomic Server — standalone HTTP server for the Atomic knowledge base
//!
//! Wraps atomic-core with a REST API + WebSocket events.
//! No Tauri dependency.

mod config;

use actix_cors::Cors;
use actix_web::{http::header, middleware, web, App, HttpServer};
use atomic_server::{
    app::configure_app,
    event_bridge,
    export_jobs::ExportJobManager,
    log_buffer::LogBuffer,
    mcp,
    migration_jobs::MigrationJobManager,
    state::{AppState, ServerEvent, SetupClaimLimiter, SetupToken},
};
use clap::Parser;
use config::{Cli, Command, MigrateAction, TokenAction};
use std::sync::Arc;
use std::time::Duration;

const SETUP_CLAIMED_AT_KEY: &str = "setup.claimed_at";

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

        // Migration subcommands (no server needed)
        Some(Command::Migrate { action }) => {
            run_migrate_command(&data_dir, action).await;
            Ok(())
        }

        // Server mode
        Some(Command::Serve {
            port,
            bind,
            public_url,
            storage,
            database_url,
            setup_token,
            dangerously_skip_setup_token,
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
                setup_token,
                dangerously_skip_setup_token,
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
                None,
                false,
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

async fn run_migrate_command(data_dir: &std::path::Path, action: MigrateAction) {
    use atomic_core::migrate::{MigrationEvent, MigrationOptions};

    match action {
        MigrateAction::Sqlite {
            source,
            database_url,
            name,
            dry_run,
            pause_feeds,
        } => {
            let manager = atomic_core::DatabaseManager::new_postgres(data_dir, &database_url)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Failed to connect to Postgres: {e}");
                    std::process::exit(1);
                });

            let result = manager
                .migrate_sqlite_to_postgres(
                    std::path::Path::new(&source),
                    MigrationOptions {
                        db_name: name,
                        dry_run,
                        pause_feeds,
                    },
                    |event| match event {
                        MigrationEvent::Started {
                            table_count,
                            total_rows,
                        } => eprintln!("Copying {total_rows} rows across {table_count} tables..."),
                        MigrationEvent::TableCompleted { table, copied_rows } => {
                            eprintln!("  {table}: {copied_rows} rows");
                        }
                        _ => {}
                    },
                    || false,
                )
                .await;

            match result {
                Ok(report) => {
                    if report.dry_run {
                        println!("Dry run — nothing was written. Source contains:");
                    } else {
                        println!(
                            "Migration complete in {:.1}s. New database \"{}\" ({}).",
                            report.duration_ms as f64 / 1000.0,
                            report.db_name,
                            report.db_id.as_deref().unwrap_or("-"),
                        );
                    }
                    let width = report
                        .tables
                        .iter()
                        .map(|t| t.table.len())
                        .max()
                        .unwrap_or(0);
                    for t in &report.tables {
                        if report.dry_run {
                            println!("  {:width$}  {:>8}", t.table, t.source_rows);
                        } else {
                            let skipped = t.source_rows - t.copied_rows;
                            if skipped > 0 {
                                println!(
                                    "  {:width$}  {:>8}  ({skipped} skipped)",
                                    t.table, t.copied_rows
                                );
                            } else {
                                println!("  {:width$}  {:>8}", t.table, t.copied_rows);
                            }
                        }
                    }
                    if !report.skipped_feed_urls.is_empty() {
                        println!(
                            "Skipped feeds already subscribed on the destination: {}",
                            report.skipped_feed_urls.join(", ")
                        );
                    }
                    if !report.dry_run {
                        println!(
                            "Embeddings and semantic edges will rebuild in the background \
                             once the destination server processes the new database."
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Migration failed: {e}");
                    std::process::exit(1);
                }
            }
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
    setup_token: Option<String>,
    dangerously_skip_setup_token: bool,
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
            if let Err(e) = backfill_setup_claimed_at(&core, &tokens).await {
                tracing::warn!(error = %e, "failed to backfill setup claimed state");
            }

            let active = tokens.iter().filter(|t| !t.is_revoked).count();
            if active == 0 {
                if dangerously_skip_setup_token {
                    tracing::warn!("no API tokens configured — insecure setup-token bypass is enabled; any client can claim this instance");
                } else if setup_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty())
                {
                    tracing::info!("no API tokens configured — open the web UI and enter ATOMIC_SETUP_TOKEN to claim this instance, or run: atomic-server token create --name default");
                } else {
                    tracing::info!("no API tokens configured — set ATOMIC_SETUP_TOKEN to claim this instance from the web UI, run: atomic-server token create --name default, or use --dangerously-skip-setup-token only for trusted development");
                }
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
    let migration_jobs =
        MigrationJobManager::new(std::path::Path::new(data_dir).join("migrations"))
            .expect("Failed to initialize migration job manager");

    let app_state = web::Data::new(AppState {
        manager: Arc::clone(&manager),
        event_tx: event_tx.clone(),
        public_url: public_url.clone(),
        log_buffer,
        export_jobs,
        migration_jobs,
        setup_token: setup_token.and_then(SetupToken::from_raw),
        dangerously_skip_setup_token,
        setup_claim_lock: tokio::sync::Mutex::new(()),
        setup_claim_limiter: SetupClaimLimiter::new(),
    });

    // Create MCP transport outside HttpServer::new() so all Actix workers share
    // one session manager.
    let mcp_transport = mcp::AtomicMcpTransport::new(
        Arc::clone(&manager),
        event_tx.clone(),
        Duration::from_secs(30),
    );

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

    // Phase-3 briefing collapse: seed the default Daily Briefing report and
    // migrate historical briefings into finding atoms on every data DB.
    // Both helpers are idempotent (the seed keys on
    // `dashboard.featured_report_id` pointing at an extant report; the
    // migration keys on a per-DB `briefings.migrated_to_findings` flag), so
    // crashing partway through means the next boot resumes. We run this
    // synchronously, before background loops spawn, so the reports tick
    // never finds a half-migrated DB. A failure in one DB logs and skips
    // that DB; we deliberately do not abort startup on a per-DB error.
    {
        let (databases, _) = manager.list_databases().await.unwrap_or_default();
        for db_info in &databases {
            let core = match manager.get_core(&db_info.id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        db = %db_info.name, error = %e,
                        "[reports/seed] failed to load database — skipping seed"
                    );
                    continue;
                }
            };
            if let Err(e) = atomic_core::reports::seed::seed_default_briefing_report(&core).await {
                tracing::error!(
                    db = %db_info.name, error = %e,
                    "[reports/seed] seed failed — skipping migration on this DB"
                );
                continue;
            }
            match atomic_core::reports::seed::migrate_briefings_to_findings(&core).await {
                Ok(0) => {} // no-op: either already migrated or no rows
                Ok(n) => tracing::info!(
                    db = %db_info.name,
                    migrated = n,
                    "[reports/seed] briefings → findings migration complete"
                ),
                Err(e) => tracing::error!(
                    db = %db_info.name, error = %e,
                    "[reports/seed] migration failed — will retry on next boot"
                ),
            }
        }
    }

    // Canvas cache warmup: compute the global canvas payload for the default
    // database in the background so the first request after startup hits a
    // warm cache. Only the default DB is warmed — other databases rebuild
    // their canvas lazily on first view, so we neither pay PCA nor retain a
    // full canvas payload (atoms + positions + edges) for databases nobody is
    // looking at. Off-loaded to the blocking pool so it never ties up an async
    // worker.
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
            let Some(db_info) = databases
                .iter()
                .find(|d| d.is_default)
                .or_else(|| databases.first())
            else {
                return;
            };
            let db_core = match warm_manager.get_core(&db_info.id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(db = %db_info.name, error = %e, "canvas warmup: failed to load database");
                    return;
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
        });
    }

    // Spawn feed polling scheduler (ticks every 60 seconds, polls all
    // databases). Each due feed's poll is dispatched through the `task_runs`
    // ledger (`task_id = "feed_poll"`, `subject_id = <feed id>`): the durable
    // lease dedups overlapping sweeps (and peer processes) per feed, and a
    // failed poll retries with backoff via `next_attempt_at` instead of
    // waiting out the feed's full poll_interval. `feeds.last_polled_at`
    // stays the fast-path cache the hot due-feeds query reads.
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
    // Each due task is dispatched through the `task_runs` ledger
    // (claim-and-record, same shape as the reports loop below): the durable
    // lease is the re-entry guard and failed runs back off via
    // `next_attempt_at` instead of retrying every tick. The registry's
    // in-memory per-(task, db) lock survives as a fast-path that skips
    // tasks this process already has in flight without a storage round-trip.
    {
        let task_manager = Arc::clone(&manager);
        let task_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut registry = atomic_core::scheduler::TaskRegistry::new();
            // DailyBriefingTask retired in phase 3 — the seeded Daily Briefing
            // report runs through the reports loop below, dispatched via the
            // task_runs ledger.
            registry.register(Arc::new(atomic_core::pipeline_task::DraftPipelineTask));
            registry.register(Arc::new(
                atomic_core::graph_maintenance::GraphMaintenanceTask,
            ));
            // Retention GC for the ledger itself (hourly): dogfoods the same
            // dispatch path, so GC runs get their own bounded run history.
            registry.register(Arc::new(atomic_core::scheduler::gc::TaskRunsGcTask));
            let registry = Arc::new(registry);
            let ctx = atomic_core::scheduler::TaskContext {
                event_cb: event_bridge::task_event_callback(task_tx.clone()),
                embedding_event_cb: Arc::new(event_bridge::embedding_event_callback(task_tx)),
            };

            let mut interval = tokio::time::interval(Duration::from_secs(15));
            interval.tick().await;
            loop {
                interval.tick().await;
                // Handles are dropped, not awaited: a slow task must not
                // stall the next tick. The lease + lock keep re-entry safe.
                let _ = atomic_core::scheduler::runner::tick_all_databases(
                    &task_manager,
                    &registry,
                    &ctx,
                )
                .await;

                // Wiki-regen retry sweep. Regeneration is event-triggered
                // (manual request / tag change), not scheduled — nothing
                // re-fires a failed run, so each tick also scans the ledger
                // for runnable `wiki.regenerate` rows (failed runs whose
                // backoff has elapsed, crashed runs with expired leases)
                // and re-executes them. Spawned per DB so a slow LLM call
                // can't stall the tick; overlapping sweeps dedup on the
                // ledger's conditional claim.
                if let Ok((dbs, _)) = task_manager.list_databases().await {
                    for db_info in dbs {
                        let core = match task_manager.get_core(&db_info.id).await {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        tokio::spawn(async move {
                            let regenerated = core.sweep_due_wiki_regens().await;
                            if !regenerated.is_empty() {
                                tracing::info!(
                                    db = %db_info.name,
                                    tags = regenerated.len(),
                                    "[wiki.regenerate] retry sweep regenerated articles"
                                );
                            }
                        });
                    }
                }
            }
        });
    }

    // Reports runner. Independent of the legacy scheduler tick: reports
    // are dynamic per-DB, gated by cron, and dispatched through the
    // `task_runs` ledger from phase 1.5. Each tick we iterate every DB,
    // list enabled reports, and call `claim_or_create` for due ones; the
    // ledger's conditional-update guards against double-firing if a
    // previous tick is still running.
    {
        let reports_manager = Arc::clone(&manager);
        let reports_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.tick().await;
            loop {
                interval.tick().await;
                let databases = match reports_manager.list_databases().await {
                    Ok((dbs, _)) => dbs,
                    Err(_) => continue,
                };
                for db_info in &databases {
                    let core = match reports_manager.get_core(&db_info.id).await {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let reports = match core.list_enabled_reports().await {
                        Ok(rs) => rs,
                        Err(e) => {
                            tracing::warn!(db = %db_info.id, error = %e, "[reports] list failed");
                            continue;
                        }
                    };
                    let now = chrono::Utc::now();
                    for report in reports {
                        if !atomic_core::reports::schedule::is_due(&report, now) {
                            continue;
                        }
                        let core_clone = core.clone();
                        let run_tx = reports_tx.clone();
                        tokio::spawn(async move {
                            match atomic_core::reports::run_report(
                                &core_clone,
                                &report,
                                atomic_core::models::TaskRunTrigger::Schedule,
                            )
                            .await
                            {
                                Ok(outcome) => {
                                    tracing::info!(
                                        report_id = %report.id,
                                        outcome = ?outcome,
                                        "[reports] scheduled run complete"
                                    );
                                    // Broadcast `atom-created` for the new
                                    // finding so the dashboard widget
                                    // refreshes live. The runner writes
                                    // directly through storage and doesn't
                                    // touch the event bridge, so without
                                    // this an open dashboard would only
                                    // see the new finding after a manual
                                    // refresh or DB switch.
                                    if let atomic_core::reports::RunOutcome::Succeeded {
                                        finding_atom_id,
                                    } = outcome
                                    {
                                        match core_clone.get_atom(&finding_atom_id).await {
                                            Ok(Some(atom)) => {
                                                let _ =
                                                    run_tx.send(ServerEvent::AtomCreated { atom });
                                            }
                                            Ok(None) => tracing::warn!(
                                                report_id = %report.id,
                                                finding_atom_id = %finding_atom_id,
                                                "[reports] finding atom missing after write — skipping broadcast"
                                            ),
                                            Err(e) => tracing::warn!(
                                                report_id = %report.id,
                                                error = %e,
                                                "[reports] finding fetch for broadcast failed"
                                            ),
                                        }
                                    }
                                }
                                Err(e) => tracing::error!(
                                    report_id = %report.id,
                                    error = %e,
                                    "[reports] scheduled run failed"
                                ),
                            }
                        });
                    }
                }
            }
        });
    }

    let bind_owned = bind.to_string();
    let shutdown_manager = Arc::clone(&manager);
    let cors_public_url = public_url.clone();

    HttpServer::new(move || {
        let cors = build_cors(cors_public_url.as_deref());

        // CORS + compression are deployment concerns and stay here; the
        // route table itself lives in `atomic_server::app::configure_app`
        // so tests and other embedders compose the identical wiring.
        App::new()
            .wrap(cors)
            .wrap(middleware::Compress::default())
            .configure(configure_app(app_state.clone(), mcp_transport.clone()))
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

async fn backfill_setup_claimed_at(
    core: &atomic_core::AtomicCore,
    tokens: &[atomic_core::ApiTokenInfo],
) -> Result<(), atomic_core::AtomicCoreError> {
    if tokens.is_empty() {
        return Ok(());
    }

    let settings = core.get_settings().await?;
    if settings.contains_key(SETUP_CLAIMED_AT_KEY) {
        return Ok(());
    }

    let claimed_at = tokens
        .iter()
        .map(|token| token.created_at.as_str())
        .min()
        .expect("tokens is non-empty");
    core.set_setting(SETUP_CLAIMED_AT_KEY, claimed_at).await
}

fn build_cors(public_url: Option<&str>) -> Cors {
    let public_origin = public_url.and_then(origin_from_url);
    Cors::default()
        .allowed_origin_fn(move |origin, _req_head| {
            let Ok(origin) = origin.to_str() else {
                return false;
            };
            is_local_origin(origin) || public_origin.as_deref() == Some(origin)
        })
        .allowed_methods(vec!["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"])
        .allow_any_header()
        .expose_headers(vec![header::HeaderName::from_static("mcp-session-id")])
        .max_age(3600)
}

fn origin_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{scheme}://{host}{port}"))
}

fn is_local_origin(origin: &str) -> bool {
    if matches!(
        origin,
        "tauri://localhost" | "capacitor://localhost" | "ionic://localhost"
    ) {
        return true;
    }

    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    host == "localhost"
        || host == "tauri.localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host.ends_with(".localhost")
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test as actix_test;
    use atomic_server::app::health;

    #[actix_web::test]
    async fn cors_allows_mcp_session_headers_from_local_origins() {
        let app = actix_test::init_service(
            App::new()
                .wrap(build_cors(None))
                .route("/health", web::get().to(health)),
        )
        .await;

        let req = actix_test::TestRequest::default()
            .method(actix_web::http::Method::OPTIONS)
            .uri("/health")
            .insert_header((header::ORIGIN, "http://localhost:5173"))
            .insert_header((header::ACCESS_CONTROL_REQUEST_METHOD, "GET"))
            .insert_header((
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,content-type,mcp-session-id,mcp-protocol-version",
            ))
            .to_request();

        let response = actix_test::call_service(&app, req).await;

        assert!(response.status().is_success());
        let allowed_headers = response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|value| value.to_str().ok())
            .expect("preflight response should include allowed headers");

        assert!(allowed_headers.contains("authorization"));
        assert!(allowed_headers.contains("content-type"));
        assert!(allowed_headers.contains("mcp-session-id"));
        assert!(allowed_headers.contains("mcp-protocol-version"));
    }

    #[actix_web::test]
    async fn cors_exposes_mcp_session_id_to_browser_clients() {
        let app = actix_test::init_service(
            App::new()
                .wrap(build_cors(None))
                .route("/health", web::get().to(health)),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/health")
            .insert_header((header::ORIGIN, "http://localhost:5173"))
            .to_request();

        let response = actix_test::call_service(&app, req).await;

        assert!(response.status().is_success());
        let exposed_headers = response
            .headers()
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
            .and_then(|value| value.to_str().ok())
            .expect("CORS response should expose MCP session header");

        assert!(exposed_headers.contains("mcp-session-id"));
    }
}
