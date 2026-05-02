//! Route configuration — registers all API route groups

pub mod atoms;
pub mod auth;
pub mod briefings;
pub mod canvas;
pub mod chat;
pub mod clustering;
pub mod databases;
pub mod embedding;
pub mod exports;
pub mod feeds;
pub mod graph;
pub mod import;
pub mod ingest;
pub mod logs;
pub mod oauth;
pub mod ollama;
pub mod search;
pub mod settings;
pub mod setup;
pub mod utils;
pub mod wiki;
pub mod health;

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
    cfg.route("/atoms/{id}/lock", web::post().to(atoms::set_atom_locked));
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

    // Briefings
    cfg.route(
        "/briefings/latest",
        web::get().to(briefings::get_latest_briefing),
    );
    cfg.route("/briefings", web::get().to(briefings::list_briefings));
    cfg.route(
        "/briefings/run",
        web::post().to(briefings::run_briefing_now),
    );
    cfg.route("/briefings/{id}", web::get().to(briefings::get_briefing));

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

    // Health
    cfg.route("/health/knowledge", web::get().to(health::get_health_knowledge));
    cfg.route("/health/fix", web::post().to(health::run_health_fix));
    cfg.route(
        "/health/fix/{check}/{item_id}",
        web::post().to(health::apply_manual_fix),
    );
    cfg.route("/health/undo/{fix_id}", web::post().to(health::undo_health_fix));
    cfg.route("/health/history", web::get().to(health::get_health_history));
    cfg.route("/health/fixes/recent", web::get().to(health::get_recent_fixes));
    cfg.route("/health/check/{check_name}", web::post().to(health::compute_single_check));
    cfg.route("/health/contradiction-summary/{atom_a}/{atom_b}", web::post().to(health::contradiction_summary_handler));
    cfg.route("/health/fix/batch", web::post().to(health::apply_manual_fix_batch));
    cfg.route("/health/strip-boilerplate/{atom_id}", web::post().to(health::strip_boilerplate_handler));
    cfg.route("/health/broken-link-suggest", web::get().to(health::broken_link_suggest_handler));
    cfg.route("/health/broken-links/auto-resolve-all", web::post().to(health::broken_links_auto_resolve_all));
    cfg.route("/health/verify/{check}", web::post().to(health::verify_batch_handler));
    cfg.route("/health/tag-proposal", web::post().to(health::create_tag_proposal));
    cfg.route("/health/tag-proposal/latest", web::get().to(health::get_latest_tag_proposal));
    cfg.route("/health/tag-proposal/{proposal_id}/apply", web::post().to(health::apply_tag_proposal));
    cfg.route("/health/config", web::get().to(health::get_health_config));
    cfg.route("/health/config", web::put().to(health::set_health_config));
    cfg.route("/wiki/excluded-tags", web::get().to(health::get_wiki_excluded_tags));
    cfg.route("/wiki/excluded-tags", web::put().to(health::set_wiki_excluded_tags));
    cfg.route("/health/custom-checks", web::get().to(health::get_custom_health_checks));
    cfg.route("/health/custom-checks", web::put().to(health::set_custom_health_checks));
}