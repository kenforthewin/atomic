//! Route configuration — registers all API route groups

pub mod atoms;
pub mod auth;
pub mod canvas;
pub mod chat;
pub mod clustering;
pub mod dashboard;
pub mod databases;
pub mod embedding;
pub mod exports;
pub mod feeds;
pub mod graph;
pub mod import;
pub mod ingest;
pub mod logs;
pub mod migrations;
pub mod oauth;
pub mod ollama;
pub mod reports;
pub mod search;
pub mod settings;
pub mod setup;
pub mod utils;
pub mod wiki;

use actix_web::web;

pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    // Atoms
    cfg.route("/atoms", web::get().to(atoms::get_atoms));
    cfg.route("/atoms", web::post().to(atoms::create_atom));
    cfg.route("/atoms/bulk", web::post().to(atoms::bulk_create_atoms));
    cfg.route("/atoms/sources", web::get().to(atoms::get_source_list));
    cfg.route(
        "/atoms/link-suggestions",
        web::get().to(atoms::get_atom_link_suggestions),
    );
    cfg.route(
        "/atoms/by-source-url",
        web::get().to(atoms::get_atom_by_source_url),
    );
    cfg.route("/atoms/{id}/links", web::get().to(atoms::get_atom_links));
    cfg.route("/atoms/{id}", web::get().to(atoms::get_atom));
    cfg.route("/atoms/{id}", web::put().to(atoms::update_atom));
    cfg.route(
        "/atoms/{id}/content",
        web::put().to(atoms::update_atom_content_only),
    );
    cfg.route(
        "/atoms/{id}/process",
        web::post().to(atoms::process_atom_pipeline),
    );
    cfg.route("/atoms/{id}", web::delete().to(atoms::delete_atom));
    cfg.route("/atoms/{id}/similar", web::get().to(search::find_similar));
    cfg.route(
        "/atoms/{id}/embedding-status",
        web::get().to(embedding::get_embedding_status),
    );

    // Tags
    cfg.route("/tags", web::get().to(atoms::get_tags));
    cfg.route("/tags", web::post().to(atoms::create_tag));
    cfg.route(
        "/tags/configure-autotag-targets",
        web::post().to(atoms::configure_autotag_targets),
    );
    cfg.route(
        "/tags/{id}/children",
        web::get().to(atoms::get_tag_children),
    );
    cfg.route(
        "/tags/{id}/autotag-target",
        web::put().to(atoms::set_tag_autotag_target),
    );
    cfg.route(
        "/tags/{id}/autotag-description",
        web::put().to(atoms::set_tag_autotag_description),
    );
    cfg.route("/tags/{id}", web::put().to(atoms::update_tag));
    cfg.route("/tags/{id}", web::delete().to(atoms::delete_tag));

    // Search
    cfg.route("/search", web::post().to(search::search));
    cfg.route("/search/global", web::post().to(search::global_search));

    // Wiki
    cfg.route("/wiki", web::get().to(wiki::get_all_wiki_articles));
    cfg.route(
        "/wiki/suggestions",
        web::get().to(wiki::get_wiki_suggestions),
    );
    cfg.route(
        "/wiki/versions/{version_id}",
        web::get().to(wiki::get_wiki_version),
    );
    cfg.route("/wiki/{tag_id}", web::get().to(wiki::get_wiki));
    cfg.route(
        "/wiki/{tag_id}/status",
        web::get().to(wiki::get_wiki_status),
    );
    cfg.route(
        "/wiki/{tag_id}/generate",
        web::post().to(wiki::generate_wiki),
    );
    cfg.route("/wiki/{tag_id}/update", web::post().to(wiki::update_wiki));
    cfg.route("/wiki/{tag_id}/propose", web::post().to(wiki::propose_wiki));
    cfg.route(
        "/wiki/{tag_id}/proposal",
        web::get().to(wiki::get_wiki_proposal),
    );
    cfg.route(
        "/wiki/{tag_id}/proposal/accept",
        web::post().to(wiki::accept_wiki_proposal),
    );
    cfg.route(
        "/wiki/{tag_id}/proposal/dismiss",
        web::post().to(wiki::dismiss_wiki_proposal),
    );
    cfg.route("/wiki/{tag_id}", web::delete().to(wiki::delete_wiki));
    cfg.route(
        "/wiki/{tag_id}/related",
        web::get().to(wiki::get_related_tags),
    );
    cfg.route("/wiki/{tag_id}/links", web::get().to(wiki::get_wiki_links));
    cfg.route(
        "/wiki/{tag_id}/versions",
        web::get().to(wiki::list_wiki_versions),
    );
    cfg.route(
        "/wiki/recompute-tag-embeddings",
        web::post().to(wiki::recompute_all_tag_embeddings),
    );

    // Dashboard
    cfg.route(
        "/dashboard/featured-report",
        web::get().to(dashboard::get_featured_report),
    );
    cfg.route(
        "/dashboard/featured-report",
        web::put().to(dashboard::set_featured_report),
    );

    // Reports (phase 2 — runs alongside the existing briefing path)
    cfg.route("/reports", web::get().to(reports::list_reports));
    cfg.route("/reports", web::post().to(reports::create_report));
    cfg.route("/reports/{id}", web::get().to(reports::get_report));
    cfg.route("/reports/{id}", web::put().to(reports::update_report));
    cfg.route("/reports/{id}", web::delete().to(reports::delete_report));
    cfg.route(
        "/reports/{id}/enabled",
        web::patch().to(reports::set_report_enabled),
    );
    cfg.route("/reports/{id}/run", web::post().to(reports::run_report_now));
    cfg.route(
        "/reports/{id}/findings",
        web::get().to(reports::list_findings_for_report),
    );
    cfg.route(
        "/findings/{atom_id}/citations",
        web::get().to(reports::list_finding_citations),
    );
    cfg.route(
        "/findings/{atom_id}",
        web::get().to(reports::get_finding_provenance),
    );

    // Settings
    cfg.route("/settings", web::get().to(settings::get_settings));
    cfg.route("/settings/{key}", web::put().to(settings::set_setting));
    cfg.route(
        "/settings/{key}",
        web::delete().to(settings::clear_setting_override),
    );
    cfg.route(
        "/settings/{key}/overrides",
        web::get().to(settings::list_setting_overrides),
    );
    cfg.route(
        "/settings/test-openrouter",
        web::post().to(settings::test_openrouter_connection),
    );
    cfg.route(
        "/settings/models",
        web::get().to(settings::get_available_llm_models),
    );
    cfg.route(
        "/settings/embedding-models",
        web::get().to(settings::get_openrouter_embedding_models),
    );
    cfg.route(
        "/settings/test-openai-compat",
        web::post().to(settings::test_openai_compat_connection),
    );

    // Embedding management
    cfg.route(
        "/embeddings/process-pending",
        web::post().to(embedding::process_pending_embeddings),
    );
    cfg.route(
        "/embeddings/process-tagging",
        web::post().to(embedding::process_pending_tagging),
    );
    cfg.route(
        "/embeddings/retry/{atom_id}",
        web::post().to(embedding::retry_embedding),
    );
    cfg.route(
        "/tagging/retry/{atom_id}",
        web::post().to(embedding::retry_tagging),
    );
    cfg.route(
        "/embeddings/retry-failed",
        web::post().to(embedding::retry_failed_embeddings),
    );
    cfg.route(
        "/tagging/retry-failed",
        web::post().to(embedding::retry_failed_tagging),
    );
    cfg.route(
        "/embeddings/reembed-all",
        web::post().to(embedding::reembed_all_atoms),
    );
    cfg.route(
        "/tagging/retag-all",
        web::post().to(embedding::retag_all_atoms),
    );
    cfg.route(
        "/embeddings/reset-stuck",
        web::post().to(embedding::reset_stuck_processing),
    );
    cfg.route(
        "/embeddings/status",
        web::get().to(embedding::get_pipeline_status),
    );
    cfg.route(
        "/embeddings/status/all",
        web::get().to(embedding::get_all_pipeline_statuses),
    );

    // Canvas
    cfg.route("/canvas/positions", web::get().to(canvas::get_positions));
    cfg.route("/canvas/positions", web::put().to(canvas::save_positions));
    cfg.route(
        "/canvas/atoms-with-embeddings",
        web::get().to(canvas::get_atoms_with_embeddings),
    );
    cfg.route("/canvas/level", web::post().to(canvas::get_canvas_level));
    cfg.route("/canvas/global", web::get().to(canvas::get_global_canvas));

    // Graph
    cfg.route("/graph/edges", web::get().to(graph::get_semantic_edges));
    cfg.route(
        "/graph/neighborhood/{atom_id}",
        web::get().to(graph::get_atom_neighborhood),
    );
    cfg.route(
        "/graph/rebuild-edges",
        web::post().to(graph::rebuild_semantic_edges),
    );

    // Clustering
    cfg.route(
        "/clustering/compute",
        web::post().to(clustering::compute_clusters),
    );
    cfg.route("/clustering", web::get().to(clustering::get_clusters));
    cfg.route(
        "/clustering/connection-counts",
        web::get().to(clustering::get_connection_counts),
    );

    // Chat / Conversations
    cfg.route("/conversations", web::post().to(chat::create_conversation));
    cfg.route("/conversations", web::get().to(chat::get_conversations));
    cfg.route("/conversations/{id}", web::get().to(chat::get_conversation));
    cfg.route(
        "/conversations/{id}",
        web::put().to(chat::update_conversation),
    );
    cfg.route(
        "/conversations/{id}",
        web::delete().to(chat::delete_conversation),
    );
    cfg.route(
        "/conversations/{id}/scope",
        web::put().to(chat::set_conversation_scope),
    );
    cfg.route(
        "/conversations/{id}/scope/tags",
        web::post().to(chat::add_tag_to_scope),
    );
    cfg.route(
        "/conversations/{id}/scope/tags/{tag_id}",
        web::delete().to(chat::remove_tag_from_scope),
    );
    cfg.route(
        "/conversations/{id}/messages",
        web::post().to(chat::send_chat_message),
    );

    // Ollama
    cfg.route("/ollama/test", web::post().to(ollama::test_ollama));
    cfg.route("/ollama/models", web::get().to(ollama::get_ollama_models));
    cfg.route(
        "/ollama/embedding-models",
        web::get().to(ollama::get_ollama_embedding_models),
    );
    cfg.route(
        "/ollama/llm-models",
        web::get().to(ollama::get_ollama_llm_models),
    );
    cfg.route(
        "/provider/verify",
        web::get().to(ollama::verify_provider_configured),
    );

    // Utils
    cfg.route("/utils/sqlite-vec", web::get().to(utils::check_sqlite_vec));
    cfg.route("/utils/compact-tags", web::post().to(utils::compact_tags));

    // Auth / Token management
    cfg.route("/auth/tokens", web::post().to(auth::create_token));
    cfg.route("/auth/tokens", web::get().to(auth::list_tokens));
    cfg.route("/auth/tokens/{id}", web::delete().to(auth::revoke_token));

    // Databases
    cfg.route("/databases", web::get().to(databases::list_databases));
    cfg.route("/databases", web::post().to(databases::create_database));
    cfg.route("/databases/{id}", web::put().to(databases::rename_database));
    cfg.route(
        "/databases/{id}",
        web::delete().to(databases::delete_database),
    );
    cfg.route(
        "/databases/{id}/activate",
        web::put().to(databases::activate_database),
    );
    cfg.route(
        "/databases/{id}/default",
        web::put().to(databases::set_default_database),
    );
    cfg.route(
        "/databases/{id}/stats",
        web::get().to(databases::database_stats),
    );
    cfg.route(
        "/databases/{id}/exports/markdown",
        web::post().to(exports::start_markdown_export),
    );
    cfg.route("/exports/{id}", web::get().to(exports::get_export_job));
    cfg.route(
        "/exports/{id}",
        web::delete().to(exports::cancel_or_delete_export_job),
    );

    // Import
    cfg.route(
        "/import/obsidian",
        web::post().to(import::import_obsidian_vault),
    );

    // SQLite → Postgres migrations (fixed paths before the {id} routes)
    cfg.route(
        "/migrations/sqlite",
        web::post().to(migrations::upload_sqlite_migration),
    );
    cfg.route(
        "/migrations/push",
        web::post().to(migrations::push_migration),
    );
    cfg.route(
        "/migrations/{id}",
        web::get().to(migrations::get_migration_job),
    );
    cfg.route(
        "/migrations/{id}",
        web::delete().to(migrations::cancel_or_delete_migration_job),
    );

    // Ingestion
    cfg.route("/ingest/url", web::post().to(ingest::ingest_url));
    cfg.route("/ingest/urls", web::post().to(ingest::ingest_urls));

    // Feeds
    cfg.route("/feeds", web::get().to(feeds::list_feeds));
    cfg.route("/feeds", web::post().to(feeds::create_feed));
    cfg.route("/feeds/{id}", web::get().to(feeds::get_feed));
    cfg.route("/feeds/{id}", web::put().to(feeds::update_feed));
    cfg.route("/feeds/{id}", web::delete().to(feeds::delete_feed));
    cfg.route("/feeds/{id}/poll", web::post().to(feeds::poll_feed));

    // Logs
    cfg.route("/logs", web::get().to(logs::get_logs));
}
