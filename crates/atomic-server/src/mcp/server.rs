use crate::event_bridge::{embedding_event_callback, ingestion_event_callback};
use crate::mcp::types::*;
use crate::state::ServerEvent;
use atomic_core::manager::DatabaseManager;
use atomic_core::AtomicCore;
use atomic_core::{apply_atom_edits, AtomEditOperation};
use rmcp::{
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler,
};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Extension type inserted by the `on_request` hook to carry the `?db=` selection.
#[derive(Clone, Debug)]
pub struct DbSelection(pub Option<String>);

/// Per-request override for the [`DatabaseManager`] the MCP tools resolve
/// against, carried in the rmcp request extensions.
///
/// The transport mirrors the data plane's [`RequestDatabaseManager`]
/// extension (`crate::db_extractor`): a caller composing the MCP scope under
/// its own middleware can install a [`RequestDatabaseManager`] on the actix
/// request, and the transport copies the manager into this extension so the
/// tool call resolves against it instead of the manager baked in at
/// [`AtomicMcpServer::new`]. When absent — the standalone server installs no
/// such middleware — [`AtomicMcpServer::resolve_core`] falls back to the
/// baked-in manager, so self-hosted behavior is unchanged.
///
/// It carries the *manager* rather than a pre-resolved [`AtomicCore`] so the
/// per-request database selection ([`DbSelection`] / the `?db=` parameter)
/// stays applied in exactly one place ([`AtomicMcpServer::resolve_core`]),
/// regardless of where the manager came from — the same discipline as the
/// data plane's [`resolve_core`](crate::db_extractor::resolve_core).
///
/// [`RequestDatabaseManager`]: crate::db_extractor::RequestDatabaseManager
#[derive(Clone)]
pub struct RequestManager(pub Arc<DatabaseManager>);

/// MCP Server for Atomic knowledge base
#[derive(Clone)]
pub struct AtomicMcpServer {
    manager: Arc<DatabaseManager>,
    event_tx: broadcast::Sender<ServerEvent>,
    tool_router: ToolRouter<Self>,
}

impl AtomicMcpServer {
    pub fn new(manager: Arc<DatabaseManager>, event_tx: broadcast::Sender<ServerEvent>) -> Self {
        Self {
            manager,
            event_tx,
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve the correct AtomicCore for the request.
    ///
    /// The manager is the per-request [`RequestManager`] override when a
    /// composing layer installed one, otherwise the manager baked in at
    /// [`Self::new`] — the same fallback shape as the data plane's
    /// [`request_manager`](crate::db_extractor::request_manager). The database
    /// *within* that manager is then selected from the [`DbSelection`]
    /// extension (the `?db=` parameter), so selection lives in one place
    /// regardless of where the manager came from.
    async fn resolve_core(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<AtomicCore, ErrorData> {
        let manager = context
            .extensions
            .get::<RequestManager>()
            .map(|m| &m.0)
            .unwrap_or(&self.manager);
        let db_id = context
            .extensions
            .get::<DbSelection>()
            .and_then(|s| s.0.clone());
        match db_id {
            Some(id) => manager
                .get_core(&id)
                .await
                .map_err(|e| ErrorData::internal_error(format!("Database not found: {}", e), None)),
            None => manager
                .active_core()
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None)),
        }
    }
}

#[tool_router]
impl AtomicMcpServer {
    /// Search for atoms using hybrid keyword + semantic search
    #[tool(
        description = "Search your memory for relevant knowledge. Use this before answering questions that may relate to previously stored information. Returns matching atoms ranked by relevance. Set since_days to constrain to recent atoms (e.g., 7 for last week, 30 for last month) when the question is time-sensitive."
    )]
    async fn semantic_search(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<SemanticSearchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let core = self.resolve_core(&context).await?;
        let limit = params.limit.unwrap_or(10).min(50);
        // External MCP clients (Claude Desktop, claude.ai, third-party
        // agents) get a captured-only view by default. The `kind`
        // discriminator landed in phase 1 so background-generated content
        // (report findings, etc.) wouldn't surface in external retrievers
        // unless the caller opts in. Threading the filter through
        // `SearchOptions` rather than post-filtering keeps result counts
        // accurate at `limit` and pushes the constraint into the SQL.
        let options =
            atomic_core::SearchOptions::new(params.query, atomic_core::SearchMode::Hybrid, limit)
                .with_threshold(0.3)
                .with_since_days(params.since_days)
                .with_kinds(atomic_core::models::KindFilter::only(
                    atomic_core::models::AtomKind::Captured,
                ));

        let results = core
            .search(options)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let search_results: Vec<SearchResult> = results
            .into_iter()
            .map(|r| SearchResult {
                atom_id: r.atom.atom.id.clone(),
                content_preview: r.atom.atom.content.chars().take(200).collect(),
                similarity_score: r.similarity_score,
                matching_chunk: r.matching_chunk_content,
            })
            .collect();

        let response_text = serde_json::to_string_pretty(&search_results)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(response_text)]))
    }

    /// Read a single atom with optional line-based pagination
    #[tool(
        description = "Read the full content of a specific atom. Use this after semantic_search returns a relevant result and you need the complete text. Supports pagination for large atoms."
    )]
    async fn read_atom(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<ReadAtomParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let core = self.resolve_core(&context).await?;
        let limit = params.limit.unwrap_or(500).min(500) as usize;
        let offset = params.offset.unwrap_or(0).max(0) as usize;

        let atom_with_tags = match core.get_atom(&params.atom_id).await {
            Ok(Some(a)) => a,
            Ok(None) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Atom not found: {}",
                    params.atom_id
                ))]));
            }
            Err(e) => return Err(ErrorData::internal_error(e.to_string(), None)),
        };

        let content = &atom_with_tags.atom.content;
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len() as i32;
        let start = offset.min(lines.len());
        let end = (start + limit).min(lines.len());
        let paginated_lines = &lines[start..end];
        let returned_lines = paginated_lines.len() as i32;
        let has_more = end < lines.len();

        let mut paginated_content = paginated_lines.join("\n");

        if has_more {
            paginated_content.push_str(&format!(
                "\n\n(Atom content continues. Use offset {} to read more lines.)",
                end
            ));
        }

        let response = AtomContent {
            atom_id: atom_with_tags.atom.id,
            content: paginated_content,
            total_lines,
            returned_lines,
            offset: offset as i32,
            has_more,
            created_at: atom_with_tags.atom.created_at,
            updated_at: atom_with_tags.atom.updated_at,
        };

        let response_text = serde_json::to_string_pretty(&response)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(response_text)]))
    }

    /// Create a new atom with markdown content
    #[tool(
        description = "Remember something new. Create an atom when you learn information worth retaining across conversations — user preferences, decisions, project context, or important facts. Write concise, self-contained markdown."
    )]
    async fn create_atom(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<CreateAtomParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let core = self.resolve_core(&context).await?;
        let request = atomic_core::CreateAtomRequest {
            content: params.content.clone(),
            source_url: params.source_url,
            published_at: None,
            tag_ids: vec![],
            skip_if_source_exists: false,
        };

        let on_event = embedding_event_callback(self.event_tx.clone());

        let result = core
            .create_atom(request, on_event)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
            .ok_or_else(|| {
                ErrorData::internal_error("Atom creation returned None".to_string(), None)
            })?;

        // Broadcast atom creation event
        let _ = self.event_tx.send(ServerEvent::AtomCreated {
            atom: result.clone(),
        });

        let response = AtomResponse {
            atom_id: result.atom.id.clone(),
            content_preview: result.atom.content.chars().take(200).collect(),
            tags: result.tags.iter().map(|t| t.name.clone()).collect(),
            embedding_status: result.atom.embedding_status.clone(),
        };

        let response_text = serde_json::to_string_pretty(&response)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(response_text)]))
    }

    /// Ingest a URL into Atomic
    #[tool(
        description = "Fetch a URL, extract its article content, and save it as an atom. Use this when the user asks to remember, save, or ingest a web page. If the URL already exists as an atom source_url, returns the existing atom_id with already_exists=true instead of creating a duplicate."
    )]
    async fn ingest_url(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<IngestUrlParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let core = self.resolve_core(&context).await?;
        let url = params.url;

        if let Some(existing) = core
            .get_atom_by_source_url(&url)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?
        {
            let response = IngestUrlResponse {
                atom_id: existing.atom.id,
                url: existing.atom.source_url.unwrap_or(url),
                title: existing.atom.title,
                content_length: existing.atom.content.len(),
                already_exists: true,
            };

            let response_text = serde_json::to_string_pretty(&response).map_err(|e| {
                ErrorData::internal_error(format!("Serialization error: {}", e), None)
            })?;

            return Ok(CallToolResult::success(vec![Content::text(response_text)]));
        }

        let request = atomic_core::IngestionRequest {
            url,
            tag_ids: vec![],
            title_hint: None,
            published_at: None,
        };

        let on_ingest = ingestion_event_callback(self.event_tx.clone());
        let on_embed = embedding_event_callback(self.event_tx.clone());

        let result = core
            .ingest_url(request, on_ingest, on_embed)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let response = IngestUrlResponse {
            atom_id: result.atom_id,
            url: result.url,
            title: result.title,
            content_length: result.content_length,
            already_exists: false,
        };

        let response_text = serde_json::to_string_pretty(&response)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(response_text)]))
    }

    /// Update an existing atom with optional full-content, metadata, or tag changes
    #[tool(
        description = "Compatibility full/partial atom update. Omitted fields are preserved. Content is optional so callers can update metadata or tag_ids without rewriting markdown. Prefer edit_atom for content changes unless replacing the whole atom intentionally."
    )]
    async fn update_atom(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<UpdateAtomParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let core = self.resolve_core(&context).await?;

        let existing = match core.get_atom(&params.atom_id).await {
            Ok(Some(atom)) => atom,
            Ok(None) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Atom not found: {}",
                    params.atom_id
                ))]));
            }
            Err(e) => return Err(ErrorData::internal_error(e.to_string(), None)),
        };

        if params.content.is_none()
            && params.source_url.is_none()
            && params.published_at.is_none()
            && params.tag_ids.is_none()
        {
            return Ok(CallToolResult::success(vec![Content::text(
                "No update fields provided".to_string(),
            )]));
        }

        let content = match params.content {
            Some(content) => {
                if content.trim().is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "Cannot update an atom to empty content".to_string(),
                    )]));
                }
                content
            }
            None => existing.atom.content.clone(),
        };

        let source_url = params.source_url.or(existing.atom.source_url.clone());
        let published_at = params.published_at.or(existing.atom.published_at.clone());

        let request = atomic_core::UpdateAtomRequest {
            content,
            source_url,
            published_at,
            tag_ids: params.tag_ids,
        };

        let on_event = embedding_event_callback(self.event_tx.clone());

        let result = core
            .update_atom_if_unchanged(
                &params.atom_id,
                request,
                &existing.atom.updated_at,
                on_event,
            )
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let response = AtomResponse {
            atom_id: result.atom.id.clone(),
            content_preview: result.atom.content.chars().take(200).collect(),
            tags: result.tags.iter().map(|t| t.name.clone()).collect(),
            embedding_status: result.atom.embedding_status.clone(),
        };

        let response_text = serde_json::to_string_pretty(&response)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(response_text)]))
    }

    /// Apply targeted edits to an atom's markdown content
    #[tool(
        description = "Apply safe edits to an existing atom. Supports replace, insert_after, append, and replace_all. replace and insert_after require exact text that appears exactly once. The whole operation fails without saving if any edit is invalid. Prefer this over update_atom for markdown changes."
    )]
    async fn edit_atom(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<EditAtomParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let core = self.resolve_core(&context).await?;

        let existing = match core.get_atom(&params.atom_id).await {
            Ok(Some(atom)) => atom,
            Ok(None) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Atom not found: {}",
                    params.atom_id
                ))]));
            }
            Err(e) => return Err(ErrorData::internal_error(e.to_string(), None)),
        };

        let edits = params
            .edits
            .iter()
            .map(AtomEditOperation::from)
            .collect::<Vec<_>>();
        let content = match apply_atom_edits(&existing.atom.content, &edits) {
            Ok(content) => content,
            Err(error) => return Ok(CallToolResult::success(vec![Content::text(error)])),
        };

        if content == existing.atom.content {
            return Ok(CallToolResult::success(vec![Content::text(
                "Edits did not change the atom content".to_string(),
            )]));
        }
        if content.trim().is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Cannot update an atom to empty content".to_string(),
            )]));
        }

        let request = atomic_core::UpdateAtomRequest {
            content,
            source_url: existing.atom.source_url.clone(),
            published_at: existing.atom.published_at.clone(),
            tag_ids: None,
        };

        let on_event = embedding_event_callback(self.event_tx.clone());

        let result = core
            .update_atom_if_unchanged(
                &params.atom_id,
                request,
                &existing.atom.updated_at,
                on_event,
            )
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        let response = AtomResponse {
            atom_id: result.atom.id.clone(),
            content_preview: result.atom.content.chars().take(200).collect(),
            tags: result.tags.iter().map(|t| t.name.clone()).collect(),
            embedding_status: result.atom.embedding_status.clone(),
        };

        let response_text = serde_json::to_string_pretty(&response)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {}", e), None))?;

        Ok(CallToolResult::success(vec![Content::text(response_text)]))
    }
}

#[tool_handler]
impl ServerHandler for AtomicMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Atomic is your long-term memory. Search before answering from recall. \
                 Remember what's worth retaining. Update what's gone stale."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
