# Atomic Server E2E Coverage Expansion

**Status:** Implemented through slice 8 — search, wiki, chat, reports+feeds,
tags+settings, tokens+oauth+setup, ingest+import+export, and the
visualization/maintenance/misc cluster all landed against both SQLite and
Postgres. See the suite list at the bottom of the decisions log.

**Author:** Kenny + Claude, 2026-06-07.

---

## Context

Slice 1 + 2 (in the open `postgres-hardening` PR) landed a cross-backend e2e
harness at `crates/atomic-server/tests/`:

- 6 test binaries, ~26 tests (× 2 backends) covering auth, atom CRUD +
  pipeline, multi-DB routing via `X-Atomic-Database`, WebSocket event
  delivery, concurrent HTTP load, and MCP transport smoke.
- Every suite runs against SQLite and Postgres via the same `Backend` enum
  used in atomic-core's pipeline tests.
- Harness modules: `support::TestCtx` (AppState + token + mock), `test_app()`
  (in-process actix `App` for fast non-WS tests), `spawn_live_server()`
  (real `HttpServer` on `127.0.0.1:0` for tests that need a real socket).
- Mock AI provider speaks OpenAI-compat `/v1/embeddings` and
  `/v1/chat/completions` non-streaming with a tag-extraction responder.

What's still untested at the HTTP layer covers most of the remaining ~70
route surface. Of that surface, the three highest-value slices for the next
round are **search**, **wiki generation**, and **chat streaming**. They
share three properties:

1. **Cross-backend skew risk** — each touches storage paths where SQLite
   and Postgres diverge (vector distance metric, FTS implementation, tag
   lookup patterns).
2. **Load-bearing for real users** — these are the three features end
   users interact with most after atom CRUD.
3. **Mock-provider lift** — none can be tested without extending the
   wiremock-backed `MockAiServer` beyond its current happy-path tag-
   extraction shape. The lift is real but bounded, and the extensions
   compound (chat needs what wiki needs needs what search needs).

This doc plans those three slices and the cross-cutting harness work they
share.

---

## Goals

- Bring HTTP-layer coverage of search, wiki, and chat up to roughly the
  same depth we currently have for atom CRUD: positive path + a couple of
  contract negatives + cross-backend parity.
- Extend the mock provider once, in a way that supports all three.
- Keep every new suite parameterized over `Backend` so any Postgres-specific
  regression surfaces immediately on `rust-test-postgres`.
- Add ~25–35 tests total across the three slices.

## Non-goals (this iteration)

- Production-realistic LLM behavior — we are still mocking.
- Performance regression testing under sustained load.
- The remaining ~60+ routes not in these three slices (feeds, reports,
  ingestion, exports, OAuth flow, dashboard, etc.).
- WebSocket-only chat surfaces. v1 covers the SSE-streamed HTTP endpoint;
  if/when chat moves entirely to WS, add a WS arm.

---

## Slice 3a — Search

The cheapest and most foundational of the three. Doesn't extend the chat
mock at all; relies entirely on the existing embedding mock.

### What's covered today

`crates/atomic-core/tests/pipeline_tests.rs` has `search_threshold_parity`
which proves the storage-level threshold semantics match across backends.
That's at the `AtomicCore::search` boundary — the HTTP layer (`/api/search`)
is untested.

### Routes / contracts in scope

- `POST /api/search` (or whichever endpoint `routes::search` exposes —
  verify during implementation) with `mode = semantic | keyword | hybrid`.
- Tag-scoped search via `scope_tag_ids`.
- `since_days` filter.
- Empty corpus → empty results.

### Test plan

| # | Test | What it pins |
|---|------|--------------|
| 1 | `semantic_search_returns_matching_atoms` | Seed 3 atoms (2 physics + 1 biology). Query "quantum particles". Assert the two physics atoms come back, biology does not. Both backends. |
| 2 | `keyword_search_matches_substring` | Seed atoms with distinct vocabulary. Query a unique word. Assert keyword mode finds it, even when embedding doesn't (a near-orthogonal query). |
| 3 | `hybrid_search_combines_both` | Same corpus as #1 + #2. Assert hybrid returns the union with sensible ordering (RRF). |
| 4 | `tag_scoped_search_excludes_other_tags` | Tag one physics atom with "Topics/Physics" and one with no tag. Search scoped to Physics. Assert only the tagged one returns. |
| 5 | `empty_corpus_returns_empty_results` | Fresh DB, search any query, assert 200 with empty result array (not 4xx). |
| 6 | `unauthorized_search_rejected` | No Bearer header, assert 401. |

≈ 6 tests × 2 backends = **12 tests**.

### Mock provider work

**None.** The embedding mock already returns deterministic vectors for any
input. Search calls the same embedding endpoint for the query as it does
for atom ingestion. Bag-of-words ensures shared-vocab queries land near
their atoms.

### Risks / known sharp edges

- **Hybrid ordering is RRF-merged** — exact result order may be sensitive
  to the keyword score normalization. Plan: assert set membership, not
  exact order, for hybrid.
- **Keyword search uses backend-specific FTS** — SQLite uses FTS5, Postgres
  uses tsvector. Phrase-level semantics may differ subtly (stop words,
  stemming). Plan: pick test queries that are obvious matches (whole words,
  no stop words) and accept this as a known surface area.

---

## Slice 3b — Wiki

Mid-complexity. Needs the LLM mock to return article-shaped output, but the
output structure is simpler than chat (no streaming, no tool calls).

### What's covered today

Nothing at the HTTP layer. The wiki module has unit tests for citation
extraction and chunk selection; the route handlers, the
`POST /api/wiki/{tag_id}/generate` flow, and the incremental-update path
are untested through HTTP.

### Routes / contracts in scope

- Generate a wiki article for a tag.
- Fetch the generated article (with citations resolved).
- Incremental update: tag a new atom, regenerate, assert the article
  changes and includes the new citation.
- Delete a wiki article.

### Test plan

| # | Test | What it pins |
|---|------|--------------|
| 1 | `generate_wiki_for_tag_returns_article` | Seed 3 atoms all tagged "Physics". Generate. Assert 200, article body non-empty, citations point at the seeded atoms. |
| 2 | `generated_article_links_back_to_source_atoms` | Same setup. Assert every citation's `atom_id` resolves via `GET /api/atoms/{id}`. |
| 3 | `incremental_update_integrates_new_atoms` | Generate. Add a 4th tagged atom. Regenerate. Assert the new atom appears in citations and the article body changed. |
| 4 | `wiki_for_unknown_tag_returns_404` | Generate for a non-existent tag id. |
| 5 | `delete_wiki_article` | Generate, delete, assert subsequent GET returns 404. |
| 6 | `wiki_generation_requires_auth` | No Bearer → 401. |

≈ 6 tests × 2 backends = **12 tests**.

### Mock provider work

The `ChatResponder` needs a new branch keyed off the request payload shape
(or a custom system-prompt marker) that returns an article-shaped response.
The wiki generator calls `complete` (non-streaming) with a prompt that
embeds a list of chunk excerpts; the mock should:

- Inspect the prompt for chunk markers like `[atom:UUID]` (or whatever the
  prompt template uses — verify during implementation).
- Return a deterministic article body that interleaves a couple of those
  markers so the citation extractor produces a non-empty list.
- Continue to return tag-extraction JSON for the auto-tag prompt shape (the
  existing branch).

Implementation note: the responder switches on `response_format.json_schema.name`
already; if wiki uses a different schema name we can branch on that. If wiki
uses freeform completion, branch on a system-prompt substring.

### Wiki structured-output shape (from code, not a question)

Wiki calls the LLM with `response_format.json_schema` and a numbered-citation
contract. Two schemas in `crates/atomic-core/src/wiki/mod.rs`:

- **`wiki_generation_result`** — full-article rewrite:
  ```json
  { "article_content": "Markdown with [1] and [2] markers.",
    "citations_used": [1, 2] }
  ```
- **`wiki_update_section_ops`** — incremental update via append/replace/insert
  operations on named sections, also numeric `[N]` citations.

The prompt embeds source blocks numbered 1..N; post-call,
`extract_citations` parses `\[(\d+)\]` against the source list. The mock
branches on `response_format.json_schema.name` (the existing hook) and
returns content with markers like `[1]` and `[2]` plus
`citations_used: [1, 2]`. No freeform `[atom:UUID]` markers anywhere — the
mapping is positional through the source list the prompt built.

### Risks / known sharp edges

- **Incremental update logic is the load-bearing path** — most wiki bugs
  live in "what changed since last generation". The test must seed → gen
  → add atom → gen again → diff. Make sure the second gen actually sees
  the new atom (poll until embedding/tagging complete first).
- **`wiki_update_section_ops` preserves untouched section bytes** —
  per the code comment, gaps in existing citation indices (e.g. `[4] [6]
  [15]`) must round-trip without renumbering. If the mock's section-ops
  response renumbers, the parity test will surface it.

---

## Slice 3c — Chat

Highest complexity. Streaming SSE response, multi-turn (the agent calls
tools and gets results back before final response), tool dispatch through
`AtomicCore::chat_with_tools` or equivalent.

### What's covered today

Nothing at the HTTP layer. The chat module has unit tests for the agent
loop and tool wiring; the SSE handler, ChatEvent bridge, and the agentic
search-tool flow are untested through HTTP.

### Routes / contracts in scope

- Start a conversation.
- POST a message that should trigger a tool call (semantic search).
- Receive the streaming response over SSE (or WS — confirm v1 transport).
- The tool call should hit `semantic_search` internally and find atoms in
  the conversation's tag scope.
- Final response references the tools that ran.

### Test plan

| # | Test | What it pins |
|---|------|--------------|
| 1 | `chat_message_streams_response_chunks` | Send a message, collect SSE frames, assert at least one `ChatStreamDelta` + a terminal `ChatComplete`. |
| 2 | `chat_agent_uses_search_tool` | Seed atoms about physics. Ask "what do you know about quantum particles?". Assert the response stream includes a `ChatToolStart` for `semantic_search` and a `ChatToolComplete` with non-zero `results_count`. |
| 3 | `conversation_scoped_to_tag_filters_search` | Create conversation scoped to "Physics". Seed a biology atom outside scope. Ask question. Assert biology atom isn't surfaced as a citation. |
| 4 | `chat_message_persists_to_storage` | After a turn completes, GET the conversation's messages, assert both user and assistant messages are persisted in order. |
| 5 | `chat_requires_auth` | No Bearer → 401. |

≈ 5 tests × 2 backends = **10 tests**.

### Mock provider work

This is where the lift is. The chat agent calls `chat_stream` (streaming)
with tool definitions in the request. The mock currently doesn't support:

- **Streaming responses** — wiremock's `Respond` returns a single
  `ResponseTemplate`, but ResponseTemplate supports `set_body_raw` with a
  bytes payload. SSE is just `data: ...\n\n` chunks, so we can pre-build
  the full stream as one payload. The actix client should consume it
  chunk-by-chunk.
- **Tool calls in streamed output** — the OpenAI streaming format emits
  `tool_calls` deltas before the final `finish_reason`. The mock needs to:
  1. Decide based on request content whether to emit a tool call.
  2. On first request (no tool_results), emit a `tool_calls` chunk for
     `semantic_search` with a crafted query.
  3. On the second request (which now contains the tool results), emit a
     normal content stream.

State across the two requests is the trickiest part. wiremock's responders
are stateless — we need either:
- A request counter in the responder so the first call returns a tool
  call and the second call returns text.
- OR a request-body inspector that branches on whether `tool_results` is
  present in the request messages array.

Recommendation: the second. It's purer (deterministic based on input) and
matches how a real provider behaves.

### Chat transport (from code, not a question)

`routes::chat::send_chat_message` does **not** stream the HTTP response
itself. It does:

1. Builds `chat_event_callback(state.event_tx.clone())` to bridge each
   `ChatEvent` into the broadcast channel as a `ServerEvent` variant
   (`ChatStreamDelta`, `ChatToolStart`, `ChatToolComplete`, `ChatComplete`,
   etc. — all defined in `state.rs`).
2. Runs the agent loop to completion.
3. Returns the final `ChatMessageWithContext` synchronously as the HTTP
   body.

Streaming arrives over the WebSocket bus the e2e suite already uses. So the
chat e2e is shaped like `e2e_websocket.rs`: connect WS, POST the message,
collect `ChatStream*` / `ChatTool*` / `ChatComplete` frames against the
final HTTP response. **No SSE parsing needed.** Drop the
`eventsource-stream` discussion and the SSE helper from cross-cutting work.

### Risks / known sharp edges

- **Tool-call dispatch is async** — the test needs to wait for both the
  `ChatComplete` event on the WS *and* the synchronous HTTP response.
  Bound with a deadline like the existing WS test.
- **Mock state across two LLM requests** — the agent calls the LLM twice
  (initial → tool call → tool results → final answer). The mock must be
  deterministic on input: branch on whether `tool_results` (or `role:tool`
  messages) are present in the request, not on a call counter. Otherwise
  tests order-couple.

---

## Cross-cutting harness work

Two shared lifts before any slice starts:

### 1. Extend `ChatResponder` with input-driven branching

Move from the current "switch on `response_format.json_schema.name`" to a
small dispatch table:

```rust
enum MockChatMode {
    TagExtraction,      // existing: returns Physics/Biology/Cooking
    WikiArticle,        // slice 3b: returns markdown with [atom:UUID] markers
    ChatToolCall,       // slice 3c: streamed tool_calls chunk
    ChatFinalAnswer,    // slice 3c: streamed text after tool results
}
```

Selector inspects:
- `response_format.json_schema.name` (existing branch)
- presence of `tools` array → chat mode
- presence of `tool_results` / role: tool messages → final-answer mode
- system-prompt substring → wiki mode (fallback)

This refactor unblocks all three slices and keeps the mock cohesive.

### 2. Chat WS event collector

Chat streams over the existing WebSocket bus (confirmed from code; see
slice 3c). Factor the per-frame `ws.next()` loop from `e2e_websocket.rs`
into a `collect_ws_events_until<F>(ws, predicate)` helper so chat tests
can wait on `ChatComplete` and the WS event test can wait on
`TaggingComplete` with the same primitive. Cheap refactor; not strictly
required but keeps the chat test under ~80 LOC.

---

## Suggested order

1. **3a Search first.** No mock work needed. Builds confidence in the
   harness for the search-tool dependency that chat needs.
2. **3b Wiki next.** Forces the first `ChatResponder` extension. Wiki's
   article-shaped mock response is simpler than chat's tool-call dance, so
   it's a good rehearsal.
3. **3c Chat last.** Largest mock lift; benefits from search being already
   tested (so when the agent calls `semantic_search` internally, you know
   that path works).

Each slice is roughly a half-day to a day of focused work, given the
existing harness. Total: ~3 days plus mock-extension lift in 3b/3c.

---

## Open questions

**OQ-1: How much to mock vs. integrate?** For chat in particular, we
could pull in a real local LLM (Ollama with a tiny model) for some tests.
Tradeoff: realism vs. CI reliability + speed. Recommendation: keep
wiremock for v1, revisit when we ship the chat feature to cloud.

**OQ-2: Postgres test parallelization.** As the e2e suite grows, the
`--test-threads=1` constraint on the Postgres CI job will slow down. At
what test count do we invest in per-test PG schemas (or a pool of test
DBs) instead of truncation? Suggested trigger: when the PG job runtime
crosses ~5 minutes.

---

## Decisions log

| Date | Decision | Notes |
|------|----------|-------|
| 2026-06-07 | Three-slice scope (search, wiki, chat) over expanding to all ~70 routes. | Highest-value features; share mock-extension work; cross-backend skew risk highest here. |
| 2026-06-07 | Order: search → wiki → chat. | Builds mock complexity incrementally; gives chat a tested search dependency. |
| 2026-06-07 | Continue with parameterized `Backend` enum on every new suite. | Pattern works; no reason to break it. |
| 2026-06-07 | Mock provider extracted into `atomic-test-support` workspace crate before slice 3 starts. | Cleaner refactor surface for the slice 3 mock extensions (wiki article responder, chat tool-call branching). atomic-core and atomic-server both consume it via dev-dep; future atomic-cloud tests get the same surface for free. |
| 2026-06-07 | Mock provider stays in wiremock for the foreseeable future; no Ollama-backed realism tests. | Closes the "mock vs. real LLM" question. Revisit only if chat ships to cloud and we see prod bugs the mock would have caught. |
| 2026-06-07 | Postgres test parallelization is deferred. | `--test-threads=1` is fine at current scale; revisit only if the PG CI job runtime crosses ~5 minutes. |
| 2026-06-07 | Slices 4–8 planned in this same doc rather than splitting them out. | Total ~125 new tests across both backends; mock extensions concentrated in two crates (`atomic-test-support` + the route handlers themselves). Suites landed: `e2e_search.rs`, `e2e_wiki.rs`, `e2e_chat.rs`, `e2e_reports.rs`, `e2e_feeds.rs`, `e2e_tags_settings.rs`, `e2e_tokens.rs`, `e2e_oauth.rs`, `e2e_setup.rs`, `e2e_ingest_export.rs`, `e2e_misc.rs`. The Ollama provider-discovery branch is the one remaining gap — defer until Ollama-on-cloud lands. |
| 2026-06-09 | PG bug shaken out by the e2e suite: `atom_positions.x/y` were `REAL` (32-bit) but the Rust model is `f64`. sqlx's strict decoding rejected the read path and writes silently truncated. | Fixed in migration 020 (widens columns to `DOUBLE PRECISION`) plus `001_initial.sql` for fresh installs. SQLite was unaffected — its REAL is always 8 bytes. |
| 2026-06-09 | Obsidian import lifted to the storage trait so it runs on Postgres too. | Replaced the inline `as_sqlite().ok_or_else(...)` (which returned a 400 on PG) with two new trait methods: `get_or_create_tag_with_parent_id(name, parent_id) -> (id, created)` and `link_tags_to_atom_with_source(atom_id, tag_ids, source)`. Both backends now drive the same hierarchical-folder-tag path; the e2e test asserts success on both, including a nested `Topics/Science/` vault layout. |
| 2026-06-09 | Pipeline polling helper split into `poll_until_embedding_done` + `poll_until_tagging_done`. | `create_atom_runs_full_pipeline_postgres` was a known flake — the test polled embedding-status then immediately read `tags`, but auto-tagging fires after embedding completes and the race won on slower PG runs. Tests that assert on the tagged shape now wait on `tagging_status` explicitly. |

---

## Future slices — needs planning

This plan covers slice 3 (search, wiki, chat). After it lands, the
following feature areas are still un-touched at the HTTP layer. Each
deserves its own focused planning pass — scope, contracts, mock work,
test counts — when its turn comes up. The groupings below reflect natural
clustering, not commitments to bundling.

**By the numbers:** `configure_routes` registers 121 routes plus another
~8 outside the authenticated scope (`/health`, `/ws`, `/mcp`, OAuth
discovery, setup, export download). Slices 1+2+3 collectively touch
roughly 15–20 of them. So expect another 4–6 slices of comparable size
before we approach "exhaustive."

### Slice 4 (proposed): Reports & Feeds

The autonomous-researcher path that's also Atomic's current north-star
primitive. Likely the highest-value next slice after 3.

- **Reports:** create / list / update / delete report definitions; run
  on-demand; verify run lifecycle (`pending → running → success/failure`)
  via `task_runs`; assert findings persist as atoms tagged with
  `report:<id>`; assert citations resolve back to source atoms.
- **Feeds:** subscribe to a feed URL (need a wiremock-backed RSS server);
  trigger a poll; assert feed-item → atom auto-ingestion with the source
  URL preserved and embedding pipeline running.
- **Cross-cutting:** verify the scheduler ledger's per-DB locking works
  through the HTTP boundary (two tenants triggering reports concurrently
  don't starve each other).

**Mock work:** RSS server (wiremock; analogous to MockAiServer but for
the ingest fetch path). Possibly a `MockUrlServer` that serves HTML pages
for the URL-ingestion route to extract.

**Estimated:** ~12–18 tests × 2 backends.

### Slice 5 (proposed): Tags & Settings

The control plane that shapes how atoms get organized and what providers
back the pipeline.

- **Tag CRUD:** create / update / delete / hierarchy queries through
  `/api/tags*` (currently we only test that auto-tagging applies a tag —
  not the management surface).
- **Tag compaction:** the LLM-driven merge path that consolidates
  semantically similar tags. Needs a mock branch for the compaction
  schema.
- **Settings round-trip:** provider config change triggers re-embed
  (already covered in atomic-core pipeline tests; e2e adds the HTTP gate
  and event flow).
- **Autotag target configuration:** `configure_autotag_targets` through
  the API; verify the targets persist and influence subsequent tagging
  runs.

**Mock work:** New `ChatResponder` branch for the compaction schema.

**Estimated:** ~10–14 tests × 2 backends.

### Slice 6 (proposed): Token & OAuth surface

Security and external-client onboarding. Slice 2's auth tests prove the
`BearerAuth` middleware works on a single token; this slice covers the
issuance and lifecycle paths.

- **API tokens:** `POST /api/tokens` (create), `GET /api/tokens` (list),
  `DELETE /api/tokens/{id}` (revoke). Last-token revocation rule round-
  trips through the HTTP error path.
- **OAuth Dynamic Client Registration:** `POST /oauth/register` with a
  valid client request, capture the issued client_id.
- **OAuth Authorization Code + PKCE:** drive the `/oauth/authorize` →
  `/oauth/token` exchange via reqwest with a code_verifier; verify the
  issued bearer works against `/api`.
- **Setup flow:** `/api/setup/status` + `/api/setup/claim` with and
  without the setup-token gate; the rate limiter and claim lock.

**Mock work:** None (no LLM involved). Need a small OAuth client
simulator (PKCE challenge + verifier) — straightforward in Rust.

**Estimated:** ~12 tests × 2 backends. OAuth specifically may want a
dedicated sub-suite because the flow has many failure modes.

### Slice 7 (proposed): Ingestion, Import, Export

Bulk content I/O. Same wiremock URL-server work as slice 4 carries over.

- **URL ingestion:** `POST /api/ingest` with a URL; mock the upstream
  HTML; assert atom created with title extracted, embedding queued.
- **Obsidian import:** upload a zip via `POST /api/import` (or whatever
  the route is); assert atoms created with frontmatter parsed.
- **Markdown export:** `POST /api/exports`, poll the job until complete,
  download via the signed URL, unzip and verify content.
- **Cross-backend:** export on Postgres mode currently lacks snapshot
  isolation (flagged in the hardening audit) — the test should pin
  current behavior, not assert isolation we don't have.

**Mock work:** `MockUrlServer` for the ingest path (shared with slice
4's feeds work if implemented first).

**Estimated:** ~10–14 tests × 2 backends.

### Slice 8 (proposed): Visualization, Maintenance, Misc

The remaining surface. Lower priority individually but the routes need
basic coverage before we can claim "everything works."

- **Canvas positions:** `GET/PUT /api/canvas/positions`; verify atom
  positions persist across reads.
- **Clustering:** `GET /api/clusters`; verify cluster assignments after
  enough atoms exist for d3-force layout to mean something.
- **Graph routes:** semantic edges, atom link materialization endpoints.
- **Embedding management:** `POST /api/embeddings/reembed-all`,
  `/retag-all`, dimension-change endpoints. Already covered at the
  pipeline level in atomic-core; e2e adds the HTTP gate.
- **Dashboard:** the per-DB featured-report pointer; verify the
  `DashboardFeaturedChanged` server event fires on writes.
- **Logs:** `GET /api/logs/recent` — small smoke test; the ring buffer
  is unit-tested elsewhere.
- **Ollama:** provider discovery + model list endpoints; mock the local
  Ollama API.
- **Utils:** whatever lives in `routes/utils.rs` — small smoke tests.
- **Database management:** rename / delete / activate / set-default;
  slice 2 only tested create + cross-DB routing.

**Mock work:** Mock Ollama server (different shape than OpenAI-compat;
its `/api/tags` and `/api/embeddings` use Ollama-specific JSON).

**Estimated:** ~20–30 tests × 2 backends. Could split into two slices if
the surface ends up bigger than expected during planning.

---

---

## Slice 4 plan — Reports & Feeds (detailed)

### Mock provider lifts

**Reports.** The agent loop calls `complete_with_tools` (non-streaming, with
`tools` defined) on the **same** `/v1/chat/completions` endpoint we already
mock. The final pass calls again with `response_format.json_schema.name =
report_generation_result`. We add two branches to `ChatResponder`:

1. **Non-streaming + tools array present**: emit a `tool_calls` choice with
   a single call to `done`. The agent loop sees the tool, runs the trivial
   "done" handler, breaks out of research. Keeps the test deterministic
   without exercising `semantic_search` / `read_atom` (already covered by
   slice 3a's HTTP search tests + slice 3c's agent loop).
2. **`response_format.json_schema.name == "report_generation_result"`**:
   return a markdown body that cites the first source — `# Mock Finding\n\n
   Body. [1]` plus `citations_used: [1]`. Source numbering is positional
   through the `Source [N]: ...` blocks the agent puts in the user message,
   same convention as the wiki path.

**Feeds.** No LLM involvement. We need a **MockUrlServer** (a thin
wiremock wrapper for non-AI URL fetches) that:

- Serves an Atom XML feed on `GET /feed.xml` with two items whose `<link>`
  points back at the same wiremock host (`/article-1`, `/article-2`).
- Serves a minimal but readability-extractable HTML doc on `GET /article-N`:
  `<html><head><title>Article N</title></head><body><article><p>` plus
  enough prose for `extract_article` not to reject it as "too short".

`MockUrlServer` lives next to `MockAiServer` in `atomic-test-support` so a
single workspace crate owns every wire mock we need.

### Routes / contracts in scope

Reports:

- `POST /api/reports` — create
- `GET /api/reports` — list
- `GET /api/reports/{id}` — fetch
- `PUT /api/reports/{id}` — update name/prompt
- `PATCH /api/reports/{id}/enabled` — toggle
- `POST /api/reports/{id}/run` — manual trigger (202, async)
- `GET /api/reports/{id}/findings` — most-recent findings
- `GET /api/findings/{atom_id}` — finding provenance
- `GET /api/findings/{atom_id}/citations` — citations rows
- `DELETE /api/reports/{id}` — delete

Feeds:

- `POST /api/feeds` — create (fetches feed URL during validation)
- `GET /api/feeds` — list
- `GET /api/feeds/{id}` — fetch
- `PUT /api/feeds/{id}` — update interval / pause
- `POST /api/feeds/{id}/poll` — synchronous poll
- `DELETE /api/feeds/{id}` — delete

### Test plan

| # | Test | What it pins |
|---|------|--------------|
| R1 | `create_report_round_trip` | POST → GET → assert fields match. Both backends. |
| R2 | `list_reports_returns_created` | Create two, list returns both. |
| R3 | `update_report_changes_fields` | PUT updates name and prompt; subsequent GET reflects them. |
| R4 | `toggle_report_enabled` | PATCH enabled=false; subsequent GET shows disabled. |
| R5 | `manual_run_writes_finding_atom` | Seed two source atoms tagged "T". Create report scoped to "T". POST /run → 202. Poll /findings until non-empty; assert one finding row + one citation row pointing at one of the seeded atoms. |
| R6 | `delete_report_cascades` | Create, run, delete; subsequent GET 404 + findings list 200 with empty array (or 404 — pin actual). |
| F1 | `create_feed_validates_url` | Stand up MockUrlServer with an Atom feed. POST /api/feeds with the mock URL → 201 + title backfilled. |
| F2 | `poll_feed_ingests_items` | Same setup. POST /feeds/{id}/poll. Assert `new_items >= 1`. Look up the atom by source_url and assert markdown contains the article text. |
| F3 | `poll_feed_dedupes_items` | Poll twice. Second call returns `new_items == 0` and `skipped == 2` (or item-count). |
| F4 | `delete_feed_removes_row` | DELETE → subsequent GET surfaces 404 (or whatever the storage returns). |
| F5 | `feeds_require_auth` | No bearer → 401. |

≈ 11 tests × 2 backends = **22 tests**.

### Sharp edges anticipated

- **The manual-run handler returns 202 before the agent finishes.** Polling
  `GET /findings` keeps the wait bounded; bound at 15s like the embedding
  poller.
- **Empty-scope is a valid `RunOutcome::EmptyScope`** that writes no
  finding atom. The test seeds source atoms tagged with the report's
  source-scope tag *before* triggering the run; without that, the run
  succeeds but produces no finding and the poll loop times out.
- **Feed-item GUID dedup** runs on the storage side. We exercise it via
  F3 specifically because it surfaces backend-specific UPSERT semantics.
- **Readability rejects too-short HTML.** The mock article must hit the
  body-length threshold or the ingest step skips with "no content".
  Empirically ~300 chars of prose inside `<article>` is enough.

---

## Slice 5 plan — Tags & Settings (detailed)

### Mock provider lifts

A third branch on `ChatResponder` for tag compaction:
`response_format.json_schema.name == "tag_compaction_result"` returns a
deterministic merge proposal (one merge action). The exact schema is in
`crates/atomic-core/src/tag_compaction/` — verify the field names at
implementation time.

### Routes / contracts in scope

Tag CRUD: POST/PUT/DELETE/GET `/api/tags*`, `/tags/{id}/children`,
`/tags/{id}/autotag-target`, `/tags/{id}/autotag-description`,
`/tags/configure-autotag-targets`.

Settings: GET/PUT `/api/settings`, the provider-change → re-embed gate
(POST `/api/embeddings/reembed-all` is in slice 8; this slice only asserts
the settings round-trip).

### Test plan

| # | Test | What it pins |
|---|------|--------------|
| T1 | `create_tag_round_trip` | POST tag, GET tags, assert in list. |
| T2 | `update_tag_renames` | PUT updates name; GET reflects. |
| T3 | `delete_tag_recursive` | Parent + child; DELETE parent with `?recursive=true`; subsequent list excludes both. |
| T4 | `tag_hierarchy_query` | Parent + 3 children; GET `/tags/{id}/children` returns 3. |
| T5 | `autotag_target_flag_persists` | PUT `/tags/{id}/autotag-target` then GET; flag set. |
| T6 | `configure_autotag_targets_creates_and_flags` | POST `/tags/configure-autotag-targets` with one new name; assert tag created + flagged. |
| T7 | `tag_compaction_proposes_merge` | Create three near-duplicate tags ("AI", "Artificial Intelligence", "Machine Learning"). Trigger compaction. Assert a merge proposal is returned (mock emits one). |
| S1 | `set_setting_round_trip` | PUT setting key/value → GET reflects. |
| S2 | `provider_change_persists` | Switch provider field; GET reflects. (Doesn't trigger re-embed in this test — that's slice 8.) |

≈ 9 tests × 2 backends = **18 tests**.

---

## Slice 6 plan — Token & OAuth (detailed)

### Mock provider lifts

None. The Token + OAuth flow doesn't touch the AI provider.

### Routes / contracts in scope

- `POST /api/tokens` (create), `GET /api/tokens` (list),
  `DELETE /api/tokens/{id}` (revoke).
- `POST /oauth/register` (DCR).
- `/oauth/authorize` → `/oauth/token` exchange with PKCE.
- `/api/setup/status`, `/api/setup/claim` (rate limiter + claim lock).

### Test plan

| # | Test | What it pins |
|---|------|--------------|
| K1 | `create_token_round_trip` | POST → GET list → assert name + id present, raw value returned exactly once. |
| K2 | `list_tokens_returns_metadata_not_secret` | GET list never includes the raw bearer. |
| K3 | `revoke_token_rejects_subsequent_requests` | DELETE token → request with that token → 401. |
| K4 | `cannot_revoke_last_token` | Try to revoke the only token → 4xx error, token still works. |
| O1 | `oauth_dcr_issues_client_id` | POST `/oauth/register` → 201 with `client_id`, `client_secret_post` or PKCE-only response. |
| O2 | `oauth_authorize_then_token_with_pkce` | Drive `/authorize` → capture `code` → `/token` exchange → bearer works against `/api/atoms`. |
| O3 | `oauth_token_rejects_invalid_verifier` | Same flow but submit a wrong code_verifier → 4xx. |
| SU1 | `setup_status_reports_initial_state` | GET `/api/setup/status` on fresh DB. |
| SU2 | `setup_claim_rejects_after_first_success` | First claim 200, second 4xx. |
| SU3 | `setup_claim_rate_limited` | Spam claim endpoint; subsequent calls 429. |

≈ 10 tests × 2 backends = **20 tests**.

OAuth shape (DCR + authorize + token) deserves its own focused module
because the failure modes are numerous. The implementation will live in
`e2e_oauth.rs` separately from `e2e_tokens.rs` and `e2e_setup.rs`.

---

## Slice 7 plan — Ingestion / Import / Export (detailed)

### Mock provider lifts

None directly. URL ingest uses the same `MockUrlServer` introduced in
slice 4; export does no LLM work.

### Routes / contracts in scope

- `POST /api/ingest` (single URL).
- `POST /api/import` (Obsidian-style markdown bulk import).
- `POST /api/exports`, `GET /api/exports/{id}`, `GET /api/exports/{id}/download`.

### Test plan

| # | Test | What it pins |
|---|------|--------------|
| I1 | `ingest_url_creates_atom` | MockUrlServer serves HTML; POST `/api/ingest` with the URL → atom created, source_url stored, pipeline kicks off. |
| I2 | `ingest_dedups_existing_source_url` | Ingest same URL twice; second returns `skipped: true`. |
| I3 | `ingest_non_html_rejected` | MockUrlServer returns 200 + `text/plain`; ingest 4xx. |
| IM1 | `import_obsidian_creates_atoms` | POST `/api/import` with a small payload (frontmatter + body); assert atoms created with tags from frontmatter. |
| E1 | `export_job_lifecycle` | POST `/api/exports` → poll status until done; download via signed URL; assert non-empty zip body. |
| E2 | `export_postgres_no_snapshot_isolation` | Postgres-only: document current behavior (export reads live data). Skipped on SQLite. |

≈ 6 tests × 2 backends = **12 tests** (minus the PG-only one which is 1).

---

## Slice 8 plan — Visualization / Maintenance / Misc (detailed)

### Mock provider lifts

- **Ollama**: a separate mock that speaks `/api/tags` (list) and
  `/api/embeddings`. Lives alongside `MockAiServer` in `atomic-test-support`
  and is only mounted by tests that exercise the Ollama branch.

### Routes / contracts in scope

- **Canvas positions**: GET/PUT `/api/canvas/positions`.
- **Clustering**: GET `/api/clusters`.
- **Graph**: semantic edges, atom-link materialization.
- **Embedding management**: `/api/embeddings/reembed-all`, `/retag-all`.
- **Dashboard**: `/api/dashboard/featured-report`.
- **Logs**: `/api/logs/recent`.
- **Ollama**: `/api/providers/ollama/models` (or whatever the discovery
  endpoint is — verify at implementation time).
- **Utils**: `/api/utils/*`.
- **Database management**: rename / delete / activate / set-default.

### Test plan (clustered by domain)

| # | Test | What it pins |
|---|------|--------------|
| V1 | `canvas_positions_round_trip` | PUT positions → GET returns same positions. |
| V2 | `clustering_returns_assignments` | Seed N atoms; GET `/clusters` returns cluster ids. |
| V3 | `graph_edges_after_pipeline` | After pipeline, GET semantic-edges returns at least one edge. |
| M1 | `reembed_all_runs_pipeline` | POST `/embeddings/reembed-all` → embedding count rises in mock. |
| M2 | `retag_all_runs_pipeline` | POST `/embeddings/retag-all` → chat count rises in mock. |
| D1 | `dashboard_featured_round_trip` | PUT featured report id → GET reflects + WS broadcasts DashboardFeaturedChanged. |
| L1 | `logs_recent_returns_ring_buffer` | Hit a route that logs; GET `/logs/recent` returns frame containing that log. |
| OL1 | `ollama_models_proxies_upstream` | MockOllama returns model list; GET our endpoint returns it. |
| U1 | `utils_smoke` | One smoke test per util endpoint (TBD at implementation time). |
| DB1 | `rename_database_round_trip` | POST rename → list reflects new name. |
| DB2 | `delete_non_default_database` | Create extra DB; delete it; list no longer includes it. |
| DB3 | `cannot_delete_default_database` | DELETE default → 4xx, DB still listed. |
| DB4 | `set_default_database_switches_active` | Multi-DB; PATCH default; subsequent active_core resolves to new default. |

≈ 13 tests × 2 backends = **26 tests** (give or take, depending on the
util surface).

---

## Open after slice 8

Even an "exhaustive" pass leaves real gaps that aren't worth e2e tests:

- **UI-only behavior:** rendering, keyboard shortcuts, drag-and-drop on
  the canvas — these live in the React suite.
- **Operational behaviors:** graceful shutdown, pgbouncer compatibility,
  long-running memory growth. These want their own load-test or chaos
  fixture, not unit-style e2e.
- **Tauri sidecar lifecycle:** the desktop wrapper has its own tests
  outside the workspace e2e harness.
- **Cloud-specific routing:** subdomain → tenant resolution, tenant
  router, control-plane endpoints. Those belong in atomic-cloud's own
  suite (using the same `atomic-test-support` crate when atomic-cloud
  ships).

When (or if) slice 8 lands, we'll be at maybe 70–85% of HTTP routes
covered. That feels like the practical ceiling for end-to-end coverage;
beyond it, unit tests and operational fixtures are higher-value
investments than another slice.
| 2026-06-07 | Chat streams via the existing WebSocket broadcast bus, not SSE on the HTTP response. | Confirmed by reading `routes::chat::send_chat_message`; bridge is `chat_event_callback` → `state.event_tx` → WS. Test reuses the `e2e_websocket.rs` pattern. |
| 2026-06-07 | Wiki uses numeric `[N]` markers in markdown plus `citations_used: [N,...]` against a numbered source list embedded in the prompt. | Schemas `wiki_generation_result` and `wiki_update_section_ops` in `crates/atomic-core/src/wiki/mod.rs`. Mock branches on `response_format.json_schema.name` (existing hook). |
