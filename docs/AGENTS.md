# Atomic Manual Documentation

This subtree contains documentation for Atomic. The public website syncs only `docs/manual/**`, so keep agent instructions and internal process notes outside `docs/manual/`. Treat the manual as user-facing product documentation, not internal notes. Every claim there must be checked against the codebase before publishing.

## Scope

- Update files in `docs/manual/**` when documenting installation, concepts, guides, self-hosting, API usage, mobile clients, MCP, or other user-visible product behavior.
- Do not put plans, architecture notes, or speculative future behavior in `docs/manual/`. Use `docs/plans/`, `docs/reference/`, or `docs/research/` instead.
- If a feature exists in code but has no clear home in the manual, add a page instead of hiding important behavior in an unrelated page.
- Keep the manual useful for real users: explain what to do, what should happen, how to verify it, and what to do when it fails.

## Website Sync

The Atomic website pulls manual docs from this repository at build/dev time:

- Website script: `../atomic-website/scripts/sync-docs.sh`.
- Source path: `docs/manual`.
- Website target path: `../atomic-website/src/content/docs`.
- `npm run sync-docs` in the website copies docs only if `src/content/docs` is missing.
- `npm run sync-docs:fresh` runs the same script with `--force`, removes the target, and copies a fresh tree.
- Without `ATOMIC_LOCAL_PATH`, the script shallow-clones `kenforthewin/atomic` from `main` or `ATOMIC_DOCS_BRANCH`.
- With `ATOMIC_LOCAL_PATH=/path/to/atomic`, the script copies from that local checkout instead of cloning.

Because the sync is a wholesale `cp -R docs/manual src/content/docs`, never put agent instructions, drafts, or non-public notes under `docs/manual/`.

## Source Of Truth

Verify docs against implementation before editing:

- Product architecture and domain behavior: `crates/atomic-core/src/lib.rs`, `crates/atomic-core/src/models.rs`, and focused modules such as `embedding.rs`, `search.rs`, `wiki/`, `agent.rs`, `canvas_level.rs`, `reports/`, `scheduler/`, and `ingest/`.
- REST routes and request/response behavior: `crates/atomic-server/src/routes/mod.rs`, the individual files in `crates/atomic-server/src/routes/`, and the generated OpenAPI annotations in those files.
- API explorer and OpenAPI URL: `GET /api/docs/openapi.json` and `/api/docs` from `crates/atomic-server/src/main.rs`.
- Frontend command names, argument transforms, and event subscriptions: `src/lib/transport/command-map.ts`, `src/lib/transport/event-normalizer.ts`, `src/stores/`, and the relevant components in `src/components/`.
- CLI flags and environment variables: `crates/atomic-server/src/config.rs`, `Dockerfile`, `server.dockerfile`, `docker-compose*.yml`, `package.json`, and `src-tauri/`.
- AI provider settings and defaults: `crates/atomic-core/src/settings.rs` and `crates/atomic-core/src/providers/`.
- MCP behavior: `crates/atomic-server/src/mcp/`, `crates/mcp-bridge/`, and the MCP integration UI under `src/components/settings/`.
- Mobile behavior: `mobile/ios/`, `mobile/android/`, `capacitor.config.ts`, and any shared HTTP API expectations in `src/lib/transport/`.

## Documentation Workflow

1. Inventory affected docs with `find docs/manual -type f | sort` and search for existing coverage with `rg "<feature-or-route>" docs/manual`.
2. Trace the implemented behavior end to end. For user-facing features, follow UI/store/transport to server route to `atomic-core`. For API docs, start at `routes/mod.rs`, then inspect the route handler and request/response types.
3. Compare docs to code and list mismatches before editing. Look for missing prerequisites, outdated command flags, undocumented settings, stale endpoint paths, incorrect defaults, missing events, and unsupported platforms.
4. Edit for completeness, not word count. Prefer a concrete setup path, exact commands, expected result, troubleshooting, and cross-links over broad product claims.
5. Add or update adjacent pages when needed. A feature spanning UI, REST, and background processing usually needs both a user guide and API/reference coverage.
6. Re-run targeted checks. At minimum, run `rg` searches for renamed routes/settings and inspect links you changed. If code changed too, run the relevant `cargo check`, `npm`, or iOS command.

## Quality Bar

Good manual pages are specific, verifiable, and task-oriented:

- Start with what the feature is for and when to use it.
- State prerequisites: server running, token required, provider configured, database selected, platform limits, network access, or model availability.
- Use exact commands and endpoint paths. Prefer copyable `curl`, `cargo`, `docker`, `npm`, or `xcodebuild` examples over prose.
- Show request bodies and important response fields for API workflows.
- Explain background behavior that affects user expectations, especially async embedding/tagging, WebSocket events, scheduled jobs, feed polling, and multi-database scope.
- Include verification steps such as checking `/health`, `/api/docs`, `/api/embeddings/status`, token list output, or UI state.
- Include failure modes when they are common: missing provider key, Ollama not running, stale token, duplicate source URL, empty-scope report runs, sqlite-vec issues, CORS/reverse proxy problems, or mobile server reachability.
- Link related pages with site-relative manual links such as `/getting-started/ai-providers/`.
- Keep descriptions accurate across desktop, headless server, web, and iOS. Do not imply a UI flow exists on every client unless code confirms it.

Avoid:

- Generic feature marketing without operational detail.
- Future-tense promises or roadmap claims.
- Copying route names from memory instead of checking `routes/mod.rs` and `command-map.ts`.
- Documenting internal APIs as user-supported unless they are exposed through REST, MCP, CLI, or UI.
- Overwriting website copy outside this repository. `docs/manual` is the source that syncs to the website.

## Page Structure

Every manual markdown page should have frontmatter:

```md
---
title: Short Title
description: One-sentence description of the user value or task.
---
```

Use this structure when it fits:

- Overview: one short paragraph that defines the feature in user terms.
- Prerequisites: bullets only when setup requirements matter.
- Quick Start: shortest working path with commands or UI steps.
- How It Works: implementation-backed explanation of the underlying behavior.
- Configuration: settings, flags, environment variables, and defaults.
- API or CLI Reference: exact methods, paths, bodies, status codes, and examples.
- Troubleshooting: symptoms, likely causes, and fixes.
- Related: links to adjacent manual pages.

Do not force every page into this structure. Concept pages can be shorter, but they still need concrete examples and links to practical guides.

## API Documentation Rules

- Keep `/api/overview.md` aligned with `crates/atomic-server/src/routes/mod.rs` and the OpenAPI annotations.
- Use `Authorization: Bearer <token>` in examples unless the route is explicitly public, such as `/health`, setup claim/status, OAuth discovery, or docs.
- Include the database-selection behavior when relevant. Routes that act on the active database may be affected by multi-database state or a `db` query parameter in transport/MCP flows.
- Mention WebSocket side effects for actions that emit events. The public WebSocket endpoint is `/ws?token=<token>`, and frontend normalized events are listed in `src/lib/transport/event-normalizer.ts`.
- Prefer generated OpenAPI as the detailed endpoint reference. Manual API pages should explain workflows, authentication, examples, and caveats that generated reference docs do not capture well.

## Known Coverage Gaps

The current manual is intentionally small and several implemented features need fuller coverage. When improving docs, prioritize these gaps:

- Reports primitive: CRUD endpoints, schedule + scope semantics, run-now behavior, finding atoms (`kind = 'report'`), citation rows, dashboard featured pointer, the curated template gallery, and migrating from the legacy daily-briefing routes.
- Setup and first-run claiming flow for self-hosted/web instances.
- Multi-database behavior: active vs default database, per-database settings caveats, export jobs, stats, and mobile/MCP implications.
- Full API workflows beyond the overview: atoms, tags, search modes, wiki proposal/version lifecycle, chat streaming, canvas/graph/clustering, feeds, ingestion, imports, exports, logs, and embedding maintenance.
- WebSocket events: embedding/tagging pipeline, chat streaming/tool events, ingestion/feed events, queue progress, atom lifecycle events (including report findings flowing through `AtomCreated`), `DashboardFeaturedChanged`, and lag handling.
- AI providers: OpenRouter, Ollama, and OpenAI-compatible providers, including model defaults, embedding dimensions, context length, connection tests, and re-embedding implications.
- Browser extension configuration and real installation/build steps from `extension/`.
- MCP remote OAuth/discovery behavior, Streamable HTTP endpoint details, bridge environment variables, and multi-database `db` query use.
- Docker and reverse proxy production details: bind address, `PUBLIC_URL`, storage backend, persistent volumes, WebSocket forwarding, and token bootstrapping.
- Capacitor mobile setup and capabilities as actually implemented in `mobile/ios/` and `mobile/android/`.

When adding any of these, verify the current behavior from code first. This list is a backlog, not a specification.

## Verification Checklist

Before finishing a documentation change:

- `rg "<changed-route-or-setting>" crates src docs/manual` confirms names and paths are consistent.
- New links point to existing manual pages or intentionally external URLs.
- Commands include required flags such as `--data-dir`, `--bind`, `--storage`, `--database-url`, or token headers when applicable.
- Examples do not expose real tokens, local private paths, or user data.
- The page says whether behavior is desktop-only, server-only, iOS-only, or available across transports.
- Any changed API statement matches `routes/mod.rs`, route handlers, and `command-map.ts`.
