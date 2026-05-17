export interface CommandSpec {
  method: 'GET' | 'POST' | 'PUT' | 'DELETE';
  path: string | ((args: Record<string, unknown>) => string);
  argsMode?: 'body' | 'query' | 'none'; // default: 'none'
  transformArgs?: (args: Record<string, unknown>) => unknown;
  transformResponse?: (data: unknown) => unknown;
}

// Helper: snake_case body from camelCase Tauri args
function atomBody(args: Record<string, unknown>) {
  return {
    content: args.content,
    source_url: args.sourceUrl ?? null,
    published_at: args.publishedAt ?? null,
    tag_ids: args.tagIds ?? [],
    skip_if_source_exists: args.skipIfSourceExists ?? false,
  };
}

export const COMMAND_MAP: Record<string, CommandSpec> = {
  // ==================== Atoms ====================
  get_all_atoms: {
    method: 'GET',
    path: '/api/atoms',
  },
  list_atoms: {
    method: 'GET',
    path: (a) => {
      const params = new URLSearchParams();
      if (a.tagId) params.set('tag_id', a.tagId as string);
      if (a.limit != null) params.set('limit', String(a.limit));
      if (a.offset != null) params.set('offset', String(a.offset));
      if (a.cursor) params.set('cursor', a.cursor as string);
      if (a.cursorId) params.set('cursor_id', a.cursorId as string);
      if (a.source) params.set('source', a.source as string);
      if (a.sourceValue) params.set('source_value', a.sourceValue as string);
      if (a.sortBy) params.set('sort_by', a.sortBy as string);
      if (a.sortOrder) params.set('sort_order', a.sortOrder as string);
      return `/api/atoms${params.toString() ? `?${params}` : ''}`;
    },
  },
  get_source_list: {
    method: 'GET',
    path: '/api/atoms/sources',
  },
  get_atoms_by_tag: {
    method: 'GET',
    path: (a) => `/api/atoms?tag_id=${encodeURIComponent(a.tagId as string)}`,
  },
  get_atom: {
    method: 'GET',
    path: (a) => `/api/atoms/${encodeURIComponent(a.id as string)}`,
  },
  get_atom_by_source_url: {
    method: 'GET',
    path: (a) => `/api/atoms/by-source-url?url=${encodeURIComponent(a.url as string)}`,
  },
  get_atom_by_id: {
    method: 'GET',
    path: (a) => `/api/atoms/${encodeURIComponent(a.id as string)}`,
  },
  create_atom: {
    method: 'POST',
    path: '/api/atoms',
    argsMode: 'body',
    transformArgs: atomBody,
  },
  bulk_create_atoms: {
    method: 'POST',
    path: '/api/atoms/bulk',
    argsMode: 'body',
    transformArgs: (a) => (a.atoms as unknown[]).map((atom: any) => atomBody(atom)),
  },
  update_atom: {
    method: 'PUT',
    path: (a) => `/api/atoms/${encodeURIComponent(a.id as string)}`,
    argsMode: 'body',
    transformArgs: atomBody,
  },
  update_atom_content_only: {
    method: 'PUT',
    path: (a) => `/api/atoms/${encodeURIComponent(a.id as string)}/content`,
    argsMode: 'body',
    transformArgs: atomBody,
  },
  process_atom_pipeline: {
    method: 'POST',
    path: (a) => `/api/atoms/${encodeURIComponent(a.id as string)}/process`,
  },
  delete_atom: {
    method: 'DELETE',
    path: (a) => `/api/atoms/${encodeURIComponent(a.id as string)}`,
  },
  get_atom_link_suggestions: {
    method: 'GET',
    path: (a) => {
      const params = new URLSearchParams();
      if (a.q != null) params.set('q', String(a.q));
      if (a.limit != null) params.set('limit', String(a.limit));
      return `/api/atoms/link-suggestions${params.toString() ? `?${params}` : ''}`;
    },
  },
  get_atom_links: {
    method: 'GET',
    path: (a) => `/api/atoms/${encodeURIComponent(a.id as string)}/links`,
  },

  // ==================== Tags ====================
  get_all_tags: {
    method: 'GET',
    path: (a) => {
      const params = new URLSearchParams();
      if (a.minCount != null) params.set('min_count', String(a.minCount));
      const qs = params.toString();
      return `/api/tags${qs ? `?${qs}` : ''}`;
    },
  },
  get_tag_children: {
    method: 'GET',
    path: (a) => {
      const params = new URLSearchParams();
      if (a.minCount != null) params.set('min_count', String(a.minCount));
      if (a.limit != null) params.set('limit', String(a.limit));
      if (a.offset != null) params.set('offset', String(a.offset));
      const qs = params.toString();
      return `/api/tags/${encodeURIComponent(a.parentId as string)}/children${qs ? `?${qs}` : ''}`;
    },
  },
  create_tag: {
    method: 'POST',
    path: '/api/tags',
    argsMode: 'body',
    transformArgs: (a) => ({ name: a.name, parent_id: a.parentId ?? null }),
  },
  update_tag: {
    method: 'PUT',
    path: (a) => `/api/tags/${encodeURIComponent(a.id as string)}`,
    argsMode: 'body',
    transformArgs: (a) => ({ name: a.name, parent_id: a.parentId ?? null }),
  },
  delete_tag: {
    method: 'DELETE',
    path: (a) => `/api/tags/${encodeURIComponent(a.id as string)}${a.recursive ? '?recursive=true' : ''}`,
  },
  set_tag_autotag_target: {
    method: 'PUT',
    path: (a) => `/api/tags/${encodeURIComponent(a.id as string)}/autotag-target`,
    argsMode: 'body',
    transformArgs: (a) => ({ value: a.value }),
  },
  set_tag_autotag_description: {
    method: 'PUT',
    path: (a) => `/api/tags/${encodeURIComponent(a.id as string)}/autotag-description`,
    argsMode: 'body',
    transformArgs: (a) => ({ description: a.description ?? '' }),
  },
  configure_autotag_targets: {
    method: 'POST',
    path: '/api/tags/configure-autotag-targets',
    argsMode: 'body',
    transformArgs: (a) => ({
      keep_defaults: a.keepDefaults ?? [],
      add_custom: a.addCustom ?? [],
    }),
  },

  // ==================== Search ====================
  search_atoms_semantic: {
    method: 'POST',
    path: '/api/search',
    argsMode: 'body',
    transformArgs: (a) => ({ query: a.query, mode: 'semantic', limit: a.limit, threshold: a.threshold }),
  },
  search_atoms_keyword: {
    method: 'POST',
    path: '/api/search',
    argsMode: 'body',
    transformArgs: (a) => ({ query: a.query, mode: 'keyword', limit: a.limit }),
  },
  search_atoms_hybrid: {
    method: 'POST',
    path: '/api/search',
    argsMode: 'body',
    transformArgs: (a) => ({ query: a.query, mode: 'hybrid', limit: a.limit, threshold: a.threshold }),
  },
  search_global_keyword: {
    method: 'POST',
    path: '/api/search/global',
    argsMode: 'body',
    transformArgs: (a) => ({ query: a.query, section_limit: a.sectionLimit }),
  },
  find_similar_atoms: {
    method: 'GET',
    path: (a) => `/api/atoms/${encodeURIComponent(a.atomId as string)}/similar?limit=${a.limit ?? 10}&threshold=${a.threshold ?? 0.7}`,
  },

  // ==================== Embedding ====================
  process_pending_embeddings: {
    method: 'POST',
    path: '/api/embeddings/process-pending',
    transformResponse: (d: any) => d.count as number,
  },
  process_pending_tagging: {
    method: 'POST',
    path: '/api/embeddings/process-tagging',
    transformResponse: (d: any) => d.count as number,
  },
  retry_embedding: {
    method: 'POST',
    path: (a) => `/api/embeddings/retry/${encodeURIComponent(a.atomId as string)}`,
  },
  retry_tagging: {
    method: 'POST',
    path: (a) => `/api/tagging/retry/${encodeURIComponent(a.atomId as string)}`,
  },
  retry_failed_embeddings: {
    method: 'POST',
    path: (a) => `/api/embeddings/retry-failed${a.dbId ? `?db=${encodeURIComponent(a.dbId as string)}` : ''}`,
    transformResponse: (d: any) => d.count as number,
  },
  retry_failed_tagging: {
    method: 'POST',
    path: (a) => `/api/tagging/retry-failed${a.dbId ? `?db=${encodeURIComponent(a.dbId as string)}` : ''}`,
    transformResponse: (d: any) => d.count as number,
  },
  reembed_all_atoms: {
    method: 'POST',
    path: (a) => `/api/embeddings/reembed-all${a.dbId ? `?db=${encodeURIComponent(a.dbId as string)}` : ''}`,
    transformResponse: (d: any) => d.count as number,
  },
  retag_all_atoms: {
    method: 'POST',
    path: (a) => `/api/tagging/retag-all${a.dbId ? `?db=${encodeURIComponent(a.dbId as string)}` : ''}`,
    transformResponse: (d: any) => d.count as number,
  },
  reset_stuck_processing: {
    method: 'POST',
    path: '/api/embeddings/reset-stuck',
    transformResponse: (d: any) => d.count as number,
  },
  get_embedding_status: {
    method: 'GET',
    path: (a) => `/api/atoms/${encodeURIComponent(a.atomId as string)}/embedding-status`,
    transformResponse: (d: any) => d.status as string,
  },
  get_all_pipeline_statuses: {
    method: 'GET',
    path: '/api/embeddings/status/all',
  },

  // ==================== Reports ====================
  // Reports replaced the legacy briefing path in phase 3. Findings are
  // first-class atoms; their listing is the dashboard widget's data
  // source via `list_findings_for_report`.
  list_reports: {
    method: 'GET',
    path: '/api/reports',
  },
  get_report: {
    method: 'GET',
    path: (a) => `/api/reports/${encodeURIComponent(a.report_id as string)}`,
  },
  create_report: {
    method: 'POST',
    path: '/api/reports',
    argsMode: 'body',
    // Caller passes the CreateReportRequest shape directly. Serde
    // derives accept the exact snake_case keys we use in TS, so the
    // body is the args object verbatim.
  },
  update_report: {
    method: 'PUT',
    path: (a) => `/api/reports/${encodeURIComponent(a.report_id as string)}`,
    argsMode: 'body',
    // The PUT body is just the UpdateReportRequest object. We strip the
    // report_id key because it lives in the path, not the body.
    transformArgs: (a) => {
      const { report_id: _omit, ...rest } = a;
      return rest;
    },
  },
  delete_report: {
    method: 'DELETE',
    path: (a) => `/api/reports/${encodeURIComponent(a.report_id as string)}`,
  },
  list_findings_for_report: {
    method: 'GET',
    path: (a) => {
      const id = encodeURIComponent(a.report_id as string);
      const params = new URLSearchParams();
      if (a.limit != null) params.set('limit', String(a.limit));
      return `/api/reports/${id}/findings${
        params.toString() ? `?${params}` : ''
      }`;
    },
  },
  list_finding_citations: {
    method: 'GET',
    path: (a) =>
      `/api/findings/${encodeURIComponent(a.atom_id as string)}/citations`,
  },
  get_finding_provenance: {
    method: 'GET',
    path: (a) => `/api/findings/${encodeURIComponent(a.atom_id as string)}`,
  },
  run_report_now: {
    method: 'POST',
    path: (a) => `/api/reports/${encodeURIComponent(a.report_id as string)}/run`,
  },
  get_featured_report_id: {
    method: 'GET',
    path: '/api/dashboard/featured-report',
  },
  set_featured_report_id: {
    method: 'PUT',
    path: '/api/dashboard/featured-report',
    argsMode: 'body',
    transformArgs: (a) => ({
      report_id: a.report_id ?? null,
    }),
  },

  // ==================== Wiki ====================
  get_all_wiki_articles: {
    method: 'GET',
    path: '/api/wiki',
  },
  get_wiki_article: {
    method: 'GET',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}`,
  },
  get_wiki_article_status: {
    method: 'GET',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/status`,
  },
  generate_wiki_article: {
    method: 'POST',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/generate`,
    argsMode: 'body',
    transformArgs: (a) => ({ tag_name: a.tagName }),
  },
  update_wiki_article: {
    method: 'POST',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/update`,
    argsMode: 'body',
    transformArgs: (a) => ({ tag_name: a.tagName }),
  },
  propose_wiki_article: {
    method: 'POST',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/propose`,
    argsMode: 'body',
    transformArgs: (a) => ({ tag_name: a.tagName }),
  },
  get_wiki_proposal: {
    method: 'GET',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/proposal`,
  },
  accept_wiki_proposal: {
    method: 'POST',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/proposal/accept`,
  },
  dismiss_wiki_proposal: {
    method: 'POST',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/proposal/dismiss`,
  },
  delete_wiki_article: {
    method: 'DELETE',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}`,
  },
  get_related_tags: {
    method: 'GET',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/related`,
  },
  get_wiki_links: {
    method: 'GET',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/links`,
  },
  get_wiki_versions: {
    method: 'GET',
    path: (a) => `/api/wiki/${encodeURIComponent(a.tagId as string)}/versions`,
  },
  get_wiki_version: {
    method: 'GET',
    path: (a) => `/api/wiki/versions/${encodeURIComponent(a.versionId as string)}`,
  },
  recompute_all_tag_embeddings: {
    method: 'POST',
    path: '/api/wiki/recompute-tag-embeddings',
    transformResponse: (d: any) => d.count as number,
  },

  // ==================== Knowledge Signals ====================
  list_knowledge_signals: {
    method: 'GET',
    path: (a) => {
      const params = new URLSearchParams();
      if (a.providerId) params.set('provider_id', a.providerId as string);
      if (a.includeDismissed != null) params.set('include_dismissed', String(a.includeDismissed));
      if (a.includeSnoozed != null) params.set('include_snoozed', String(a.includeSnoozed));
      if (a.limit != null) params.set('limit', String(a.limit));
      return `/api/knowledge-signals${params.toString() ? `?${params}` : ''}`;
    },
  },
  dismiss_knowledge_signal: {
    method: 'POST',
    path: (a) => `/api/knowledge-signals/${encodeURIComponent(a.signalKey as string)}/dismiss`,
  },
  set_knowledge_signal_provider_config: {
    method: 'PUT',
    path: (a) => `/api/knowledge-signals/providers/${encodeURIComponent(a.providerId as string)}`,
    argsMode: 'body',
    transformArgs: (a) => a.config,
  },
  snooze_knowledge_signal: {
    method: 'POST',
    path: (a) => `/api/knowledge-signals/${encodeURIComponent(a.signalKey as string)}/snooze`,
    argsMode: 'body',
    transformArgs: (a) => ({ until: a.until }),
  },
  restore_knowledge_signal: {
    method: 'POST',
    path: (a) => `/api/knowledge-signals/${encodeURIComponent(a.signalKey as string)}/restore`,
  },

  // ==================== Settings ====================
  get_settings: {
    method: 'GET',
    path: '/api/settings',
  },
  set_setting: {
    method: 'PUT',
    path: (a) => `/api/settings/${encodeURIComponent(a.key as string)}`,
    argsMode: 'body',
    transformArgs: (a) => ({ value: a.value }),
  },
  clear_setting_override: {
    method: 'DELETE',
    path: (a) => `/api/settings/${encodeURIComponent(a.key as string)}`,
  },
  test_openrouter_connection: {
    method: 'POST',
    path: '/api/settings/test-openrouter',
    argsMode: 'body',
    transformArgs: (a) => ({ api_key: a.apiKey }),
    transformResponse: (d: any) => d.success as boolean,
  },
  get_available_llm_models: {
    method: 'GET',
    path: '/api/settings/models',
  },
  get_openrouter_embedding_models: {
    method: 'GET',
    path: '/api/settings/embedding-models',
  },
  test_openai_compat_connection: {
    method: 'POST',
    path: '/api/settings/test-openai-compat',
    argsMode: 'body',
    transformArgs: (a) => ({ base_url: a.baseUrl, api_key: a.apiKey }),
    transformResponse: (d: any) => d.success as boolean,
  },

  // ==================== Canvas ====================
  get_atom_positions: {
    method: 'GET',
    path: '/api/canvas/positions',
  },
  save_atom_positions: {
    method: 'PUT',
    path: '/api/canvas/positions',
    argsMode: 'body',
    transformArgs: (a) => a.positions,
  },
  get_atoms_with_embeddings: {
    method: 'GET',
    path: '/api/canvas/atoms-with-embeddings',
  },
  get_canvas_level: {
    method: 'POST',
    path: (a) => {
      const params = new URLSearchParams();
      if (a.parentId) params.set('parent_id', a.parentId as string);
      const qs = params.toString();
      return `/api/canvas/level${qs ? `?${qs}` : ''}`;
    },
    argsMode: 'body',
    transformArgs: (a) => ({
      children_hint: a.childrenHint ?? null,
    }),
  },

  get_global_canvas: {
    method: 'GET',
    path: '/api/canvas/global',
  },

  // ==================== Graph ====================
  get_semantic_edges: {
    method: 'GET',
    path: (a) => `/api/graph/edges?min_similarity=${a.minSimilarity ?? 0.5}`,
  },
  get_atom_neighborhood: {
    method: 'GET',
    path: (a) => `/api/graph/neighborhood/${encodeURIComponent(a.atomId as string)}?depth=${a.depth ?? 1}&min_similarity=${a.minSimilarity ?? 0.5}`,
  },
  rebuild_semantic_edges: {
    method: 'POST',
    path: '/api/graph/rebuild-edges',
    transformResponse: (d: any) => d.total_edges as number,
  },

  // ==================== Clustering ====================
  compute_clusters: {
    method: 'POST',
    path: '/api/clustering/compute',
    argsMode: 'body',
    transformArgs: (a) => ({
      min_similarity: a.minSimilarity,
      min_cluster_size: a.minClusterSize,
    }),
  },
  get_clusters: {
    method: 'GET',
    path: '/api/clustering',
  },
  get_connection_counts: {
    method: 'GET',
    path: (a) => `/api/clustering/connection-counts?min_similarity=${a.minSimilarity ?? 0.5}`,
  },

  // ==================== Chat ====================
  create_conversation: {
    method: 'POST',
    path: '/api/conversations',
    argsMode: 'body',
    transformArgs: (a) => ({ tag_ids: a.tagIds ?? [], title: a.title ?? null }),
  },
  get_conversations: {
    method: 'GET',
    path: (a) => {
      const params = new URLSearchParams();
      if (a.filterTagId) params.set('filter_tag_id', a.filterTagId as string);
      if (a.limit != null) params.set('limit', String(a.limit));
      if (a.offset != null) params.set('offset', String(a.offset));
      const qs = params.toString();
      return `/api/conversations${qs ? `?${qs}` : ''}`;
    },
  },
  get_conversation: {
    method: 'GET',
    path: (a) => `/api/conversations/${encodeURIComponent(a.conversationId as string)}`,
  },
  update_conversation: {
    method: 'PUT',
    path: (a) => `/api/conversations/${encodeURIComponent(a.id as string)}`,
    argsMode: 'body',
    transformArgs: (a) => ({ title: a.title ?? null, is_archived: a.isArchived ?? null }),
  },
  delete_conversation: {
    method: 'DELETE',
    path: (a) => `/api/conversations/${encodeURIComponent(a.id as string)}`,
  },
  set_conversation_scope: {
    method: 'PUT',
    path: (a) => `/api/conversations/${encodeURIComponent(a.conversationId as string)}/scope`,
    argsMode: 'body',
    transformArgs: (a) => ({ tag_ids: a.tagIds }),
  },
  add_tag_to_scope: {
    method: 'POST',
    path: (a) => `/api/conversations/${encodeURIComponent(a.conversationId as string)}/scope/tags`,
    argsMode: 'body',
    transformArgs: (a) => ({ tag_id: a.tagId }),
  },
  remove_tag_from_scope: {
    method: 'DELETE',
    path: (a) => `/api/conversations/${encodeURIComponent(a.conversationId as string)}/scope/tags/${encodeURIComponent(a.tagId as string)}`,
  },
  send_chat_message: {
    method: 'POST',
    path: (a) => `/api/conversations/${encodeURIComponent(a.conversationId as string)}/messages`,
    argsMode: 'body',
    transformArgs: (a) => ({
      content: a.content,
      ...(a.canvasContext ? { canvas_context: a.canvasContext } : {}),
      ...(a.pageContext ? { page_context: a.pageContext } : {}),
    }),
  },

  // ==================== Ollama ====================
  test_ollama: {
    method: 'POST',
    path: '/api/ollama/test',
    argsMode: 'body',
    transformArgs: (a) => ({ host: a.host }),
    transformResponse: (d: any) => d.success as boolean,
  },
  get_ollama_models: {
    method: 'GET',
    path: (a) => `/api/ollama/models?host=${encodeURIComponent(a.host as string)}`,
  },
  get_ollama_embedding_models_cmd: {
    method: 'GET',
    path: (a) => `/api/ollama/embedding-models?host=${encodeURIComponent(a.host as string)}`,
  },
  get_ollama_llm_models_cmd: {
    method: 'GET',
    path: (a) => `/api/ollama/llm-models?host=${encodeURIComponent(a.host as string)}`,
  },
  verify_provider_configured: {
    method: 'GET',
    path: '/api/provider/verify',
    transformResponse: (d: any) => d.configured as boolean,
  },

  // ==================== Setup (public, no auth) ====================
  get_setup_status: {
    method: 'GET',
    path: '/api/setup/status',
  },
  claim_instance: {
    method: 'POST',
    path: '/api/setup/claim',
    argsMode: 'body',
    transformArgs: (a) => ({ name: a.name, setup_token: a.setupToken ?? a.setup_token }),
  },

  // ==================== Auth / Tokens ====================
  create_api_token: {
    method: 'POST',
    path: '/api/auth/tokens',
    argsMode: 'body',
    transformArgs: (a) => ({ name: a.name }),
  },
  list_api_tokens: {
    method: 'GET',
    path: '/api/auth/tokens',
  },
  revoke_api_token: {
    method: 'DELETE',
    path: (a) => `/api/auth/tokens/${encodeURIComponent(a.id as string)}`,
  },

  // ==================== Utils ====================
  check_sqlite_vec: {
    method: 'GET',
    path: '/api/utils/sqlite-vec',
    transformResponse: (d: any) => d.version as string,
  },
  compact_tags: {
    method: 'POST',
    path: '/api/utils/compact-tags',
  },

  // ==================== Ingestion ====================
  ingest_url: {
    method: 'POST',
    path: '/api/ingest/url',
    argsMode: 'body',
    transformArgs: (a) => ({
      url: a.url,
      tag_ids: a.tagIds ?? [],
      title_hint: a.titleHint ?? null,
      published_at: a.publishedAt ?? null,
    }),
  },
  ingest_urls: {
    method: 'POST',
    path: '/api/ingest/urls',
    argsMode: 'body',
    transformArgs: (a) => ({
      urls: (a.urls as Array<Record<string, unknown>>).map((u) => ({
        url: u.url,
        tag_ids: u.tagIds ?? [],
        title_hint: u.titleHint ?? null,
        published_at: u.publishedAt ?? null,
      })),
    }),
  },

  // ==================== Feeds ====================
  list_feeds: {
    method: 'GET',
    path: '/api/feeds',
  },
  get_feed: {
    method: 'GET',
    path: (a) => `/api/feeds/${encodeURIComponent(a.id as string)}`,
  },
  create_feed: {
    method: 'POST',
    path: '/api/feeds',
    argsMode: 'body',
    transformArgs: (a) => ({
      url: a.url,
      poll_interval: a.pollInterval ?? 60,
      tag_ids: a.tagIds ?? [],
    }),
  },
  update_feed: {
    method: 'PUT',
    path: (a) => `/api/feeds/${encodeURIComponent(a.id as string)}`,
    argsMode: 'body',
    transformArgs: (a) => ({
      poll_interval: a.pollInterval ?? null,
      is_paused: a.isPaused ?? null,
      tag_ids: a.tagIds ?? null,
    }),
  },
  delete_feed: {
    method: 'DELETE',
    path: (a) => `/api/feeds/${encodeURIComponent(a.id as string)}`,
  },
  poll_feed: {
    method: 'POST',
    path: (a) => `/api/feeds/${encodeURIComponent(a.id as string)}/poll`,
  },

  // ==================== Import ====================
  import_obsidian_vault: {
    method: 'POST',
    path: '/api/import/obsidian',
    argsMode: 'body',
    transformArgs: (a) => ({
      vault_path: a.vaultPath,
      max_notes: a.maxNotes ?? null,
    }),
  },

  // ==================== Databases ====================
  list_databases: {
    method: 'GET',
    path: '/api/databases',
  },
  create_database: {
    method: 'POST',
    path: '/api/databases',
    argsMode: 'body',
    transformArgs: (a) => ({ name: a.name }),
  },
  rename_database: {
    method: 'PUT',
    path: (a) => `/api/databases/${encodeURIComponent(a.id as string)}`,
    argsMode: 'body',
    transformArgs: (a) => ({ name: a.name }),
  },
  delete_database: {
    method: 'DELETE',
    path: (a) => `/api/databases/${encodeURIComponent(a.id as string)}`,
  },
  activate_database: {
    method: 'PUT',
    path: (a) => `/api/databases/${encodeURIComponent(a.id as string)}/activate`,
  },
  set_default_database: {
    method: 'PUT',
    path: (a) => `/api/databases/${encodeURIComponent(a.id as string)}/default`,
  },
  get_database_stats: {
    method: 'GET',
    path: (a) => `/api/databases/${encodeURIComponent(a.id as string)}/stats`,
  },
  start_markdown_export: {
    method: 'POST',
    path: (a) => `/api/databases/${encodeURIComponent(a.id as string)}/exports/markdown`,
  },
  get_export_job: {
    method: 'GET',
    path: (a) => `/api/exports/${encodeURIComponent(a.id as string)}`,
  },
  cancel_export_job: {
    method: 'DELETE',
    path: (a) => `/api/exports/${encodeURIComponent(a.id as string)}`,
  },

  // ==================== Logs ====================
  get_logs: {
    method: 'GET',
    path: '/api/logs',
  },
};
