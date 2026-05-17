//! Atomic Server library — exposes server components for integration testing.
//!
//! The binary entry point is in `main.rs`. This module re-exports the pieces
//! needed to spin up a test server.

pub mod auth;
mod db_extractor;
pub mod error;
pub mod event_bridge;
pub mod export_jobs;
pub mod log_buffer;
pub mod mcp;
pub mod mcp_auth;
pub mod routes;
pub mod state;
pub mod ws;

use actix_web::{HttpResponse, Responder};
use utoipa::OpenApi;
pub use utoipa_scalar::{Scalar, Servable};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Atomic API",
        description = "REST API for the Atomic knowledge base",
        version = "1.1.1",
    ),
    paths(
        // Atoms
        routes::atoms::get_atoms,
        routes::atoms::get_atom,
        routes::atoms::get_atom_links,
        routes::atoms::get_atom_link_suggestions,
        routes::atoms::get_atom_by_source_url,
        routes::atoms::create_atom,
        routes::atoms::update_atom,
        routes::atoms::update_atom_content_only,
        routes::atoms::process_atom_pipeline,
        routes::atoms::delete_atom,
        routes::atoms::bulk_create_atoms,
        routes::atoms::get_source_list,
        // Tags
        routes::atoms::get_tags,
        routes::atoms::get_tag_children,
        routes::atoms::create_tag,
        routes::atoms::update_tag,
        routes::atoms::delete_tag,
        routes::atoms::set_tag_autotag_target,
        routes::atoms::set_tag_autotag_description,
        routes::atoms::configure_autotag_targets,
        // Search
        routes::search::search,
        routes::search::global_search,
        routes::search::find_similar,
        // Wiki
        routes::wiki::get_all_wiki_articles,
        routes::wiki::get_wiki,
        routes::wiki::get_wiki_status,
        routes::wiki::generate_wiki,
        routes::wiki::update_wiki,
        routes::wiki::delete_wiki,
        routes::wiki::get_related_tags,
        routes::wiki::get_wiki_links,
        routes::wiki::get_wiki_suggestions,
        routes::wiki::list_wiki_versions,
        routes::wiki::get_wiki_version,
        routes::wiki::recompute_all_tag_embeddings,
        routes::wiki::propose_wiki,
        routes::wiki::get_wiki_proposal,
        routes::wiki::accept_wiki_proposal,
        routes::wiki::dismiss_wiki_proposal,
        // Knowledge-quality signals
        routes::knowledge_signals::list_knowledge_signals,
        routes::knowledge_signals::set_knowledge_signal_provider_config,
        routes::knowledge_signals::dismiss_knowledge_signal,
        routes::knowledge_signals::snooze_knowledge_signal,
        routes::knowledge_signals::restore_knowledge_signal,
        // Briefings
        routes::briefings::get_latest_briefing,
        routes::briefings::list_briefings,
        routes::briefings::get_briefing,
        routes::briefings::run_briefing_now,
        routes::briefings::get_briefing_schedule,
        routes::briefings::set_briefing_schedule,
        // Settings
        routes::settings::get_settings,
        routes::settings::set_setting,
        routes::settings::clear_setting_override,
        routes::settings::list_setting_overrides,
        routes::settings::test_openrouter_connection,
        routes::settings::test_openai_compat_connection,
        routes::settings::get_available_llm_models,
        routes::settings::get_openrouter_embedding_models,
        // Embeddings
        routes::embedding::process_pending_embeddings,
        routes::embedding::process_pending_tagging,
        routes::embedding::retry_embedding,
        routes::embedding::retry_failed_embeddings,
        routes::embedding::retry_failed_tagging,
        routes::embedding::retry_tagging,
        routes::embedding::reembed_all_atoms,
        routes::embedding::retag_all_atoms,
        routes::embedding::reset_stuck_processing,
        routes::embedding::get_pipeline_status,
        routes::embedding::get_all_pipeline_statuses,
        routes::embedding::get_embedding_status,
        // Canvas
        routes::canvas::get_positions,
        routes::canvas::save_positions,
        routes::canvas::get_atoms_with_embeddings,
        routes::canvas::get_canvas_level,
        routes::canvas::get_global_canvas,
        // Graph
        routes::graph::get_semantic_edges,
        routes::graph::get_atom_neighborhood,
        routes::graph::rebuild_semantic_edges,
        // Clustering
        routes::clustering::compute_clusters,
        routes::clustering::get_clusters,
        routes::clustering::get_connection_counts,
        // Chat
        routes::chat::create_conversation,
        routes::chat::get_conversations,
        routes::chat::get_conversation,
        routes::chat::update_conversation,
        routes::chat::delete_conversation,
        routes::chat::set_conversation_scope,
        routes::chat::add_tag_to_scope,
        routes::chat::remove_tag_from_scope,
        routes::chat::send_chat_message,
        // Providers
        routes::ollama::test_ollama,
        routes::ollama::get_ollama_models,
        routes::ollama::get_ollama_embedding_models,
        routes::ollama::get_ollama_llm_models,
        routes::ollama::verify_provider_configured,
        // Utils
        routes::utils::check_sqlite_vec,
        routes::utils::compact_tags,
        // Auth
        routes::auth::create_token,
        routes::auth::list_tokens,
        routes::auth::revoke_token,
        // Databases
        routes::databases::list_databases,
        routes::databases::create_database,
        routes::databases::rename_database,
        routes::databases::delete_database,
        routes::databases::activate_database,
        routes::databases::set_default_database,
        routes::databases::database_stats,
        routes::exports::start_markdown_export,
        routes::exports::get_export_job,
        routes::exports::cancel_or_delete_export_job,
        routes::exports::download_export,
        // Setup
        routes::setup::setup_status,
        routes::setup::claim_instance,
        // OAuth
        routes::oauth::resource_metadata,
        routes::oauth::metadata,
        routes::oauth::register,
        routes::oauth::authorize_page,
        routes::oauth::authorize_approve,
        routes::oauth::token,
        // Import
        routes::import::import_obsidian_vault,
        // Ingestion
        routes::ingest::ingest_url,
        routes::ingest::ingest_urls,
        // Feeds
        routes::feeds::list_feeds,
        routes::feeds::get_feed,
        routes::feeds::create_feed,
        routes::feeds::update_feed,
        routes::feeds::delete_feed,
        routes::feeds::poll_feed,
        // Logs
        routes::logs::get_logs,
    ),
    components(schemas(
        // Core types
        atomic_core::Atom,
        atomic_core::AtomLink,
        atomic_core::AtomLinkSuggestion,
        atomic_core::Tag,
        atomic_core::AtomWithTags,
        atomic_core::AtomSummary,
        atomic_core::PaginatedAtoms,
        atomic_core::BulkCreateResult,
        atomic_core::TagWithCount,
        atomic_core::PaginatedTagChildren,
        atomic_core::SourceInfo,
        atomic_core::SimilarAtomResult,
        atomic_core::SemanticSearchResult,
        atomic_core::GlobalSearchResponse,
        atomic_core::GlobalWikiSearchResult,
        atomic_core::GlobalChatSearchResult,
        atomic_core::GlobalTagSearchResult,
        atomic_core::MatchOffset,
        // Wiki
        atomic_core::WikiArticle,
        atomic_core::WikiCitation,
        atomic_core::WikiArticleWithCitations,
        atomic_core::WikiArticleStatus,
        atomic_core::WikiArticleSummary,
        atomic_core::WikiLink,
        atomic_core::RelatedTag,
        atomic_core::SuggestedArticle,
        atomic_core::WikiArticleVersion,
        atomic_core::WikiVersionSummary,
        atomic_core::WikiProposal,
        // Knowledge-quality signals
        atomic_core::KnowledgeSignal,
        atomic_core::KnowledgeSignalAction,
        atomic_core::KnowledgeSignalReason,
        atomic_core::KnowledgeSignalSeverity,
        atomic_core::KnowledgeSignalTarget,
        atomic_core::KnowledgeSignalProviderConfig,
        atomic_core::WikiCandidateEvidence,
        // Briefings
        atomic_core::Briefing,
        atomic_core::BriefingCitation,
        atomic_core::BriefingWithCitations,
        // Canvas
        atomic_core::AtomPosition,
        atomic_core::AtomWithEmbedding,
        atomic_core::CanvasLevel,
        atomic_core::CanvasNode,
        atomic_core::CanvasNodeType,
        atomic_core::CanvasEdge,
        atomic_core::BreadcrumbEntry,
        atomic_core::CanvasAtomPosition,
        atomic_core::CanvasEdgeData,
        atomic_core::CanvasClusterLabel,
        atomic_core::GlobalCanvasData,
        // Graph
        atomic_core::SemanticEdge,
        atomic_core::NeighborhoodGraph,
        atomic_core::NeighborhoodAtom,
        atomic_core::NeighborhoodEdge,
        atomic_core::AtomCluster,
        // Chat
        atomic_core::Conversation,
        atomic_core::ConversationWithTags,
        atomic_core::ConversationWithMessages,
        atomic_core::ChatMessage,
        atomic_core::ChatMessageWithContext,
        atomic_core::ChatToolCall,
        atomic_core::ChatCitation,
        // Feeds
        atomic_core::Feed,
        // Auth & Databases
        atomic_core::ApiTokenInfo,
        atomic_core::DatabaseInfo,
        atomic_core::PipelineStatus,
        atomic_core::FailedAtom,
        // Server request types
        routes::atoms::CreateAtomRequest,
        routes::atoms::UpdateAtomRequest,
        routes::atoms::CreateTagRequest,
        routes::atoms::UpdateTagRequest,
        routes::atoms::SetAutotagTargetRequest,
        routes::atoms::ConfigureAutotagTargetsRequest,
        routes::search::SearchRequest,
        routes::search::GlobalSearchRequest,
        routes::wiki::GenerateWikiBody,
        routes::knowledge_signals::KnowledgeSignalsQuery,
        routes::knowledge_signals::SnoozeSignalBody,
        routes::settings::SetSettingBody,
        routes::settings::TestOpenRouterBody,
        routes::settings::OverrideEntry,
        routes::canvas::CanvasLevelBody,
        routes::clustering::ComputeClustersBody,
        routes::chat::CreateConversationBody,
        routes::chat::UpdateConversationBody,
        routes::chat::SetScopeBody,
        routes::chat::AddTagBody,
        routes::chat::SendMessageBody,
        routes::ollama::TestOllamaBody,
        routes::auth::CreateTokenBody,
        routes::databases::CreateDatabaseBody,
        routes::databases::RenameDatabaseBody,
        routes::embedding::DatabasePipelineStatus,
        routes::embedding::AllPipelineStatuses,
        routes::setup::SetupStatusResponse,
        routes::setup::ClaimBody,
        routes::setup::ClaimResponse,
        routes::logs::LogsResponse,
        routes::exports::DownloadQuery,
        routes::oauth::OAuthProtectedResourceMetadata,
        routes::oauth::OAuthAuthorizationServerMetadata,
        routes::oauth::RegisterRequest,
        routes::oauth::RegisterResponse,
        routes::oauth::AuthorizeQuery,
        routes::oauth::AuthorizeApproveForm,
        routes::oauth::TokenRequest,
        routes::oauth::TokenResponse,
        routes::import::ImportObsidianRequest,
        routes::ingest::IngestUrlRequest,
        routes::ingest::IngestUrlsRequest,
        atomic_core::CreateFeedRequest,
        atomic_core::UpdateFeedRequest,
        error::ApiErrorResponse,
    )),
    tags(
        (name = "atoms", description = "Atom CRUD operations"),
        (name = "tags", description = "Tag management"),
        (name = "search", description = "Semantic and keyword search"),
        (name = "wiki", description = "Wiki article generation and management"),
        (name = "knowledge-signals", description = "Knowledge quality signals and feedback"),
        (name = "settings", description = "Server configuration"),
        (name = "embeddings", description = "Embedding pipeline management"),
        (name = "canvas", description = "Canvas positions and hierarchy"),
        (name = "graph", description = "Semantic graph and edges"),
        (name = "clustering", description = "Atom clustering"),
        (name = "chat", description = "Conversations and chat"),
        (name = "providers", description = "AI provider configuration"),
        (name = "utils", description = "Utility endpoints"),
        (name = "auth", description = "API token management"),
        (name = "databases", description = "Multi-database management"),
        (name = "setup", description = "Initial instance setup"),
        (name = "import", description = "Data import"),
        (name = "ingestion", description = "URL content ingestion"),
        (name = "feeds", description = "RSS/Atom feed management"),
        (name = "briefings", description = "Daily briefing generation and history"),
        (name = "logs", description = "Server log access"),
        (name = "oauth", description = "OAuth 2.0 endpoints for remote MCP clients"),
    ),
    security(
        ("bearer_auth" = []),
    ),
    modifiers(&SecurityAddon),
)]
pub struct ApiDoc;

struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer_auth",
            utoipa::openapi::security::SecurityScheme::Http(utoipa::openapi::security::Http::new(
                utoipa::openapi::security::HttpAuthScheme::Bearer,
            )),
        );
    }
}

pub async fn openapi_spec() -> impl Responder {
    HttpResponse::Ok().json(ApiDoc::openapi())
}
