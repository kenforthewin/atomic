---
title: API Overview
description: Authentication, database selection, endpoint groups, generated docs, and API conventions.
---

`atomic-server` exposes the same REST API used by the desktop app, web UI, iOS app, browser extension, and MCP bridge.

## Base URL

Local self-hosted server:

```text
http://localhost:8080
```

Desktop app sidecar:

```text
http://127.0.0.1:44380
```

API routes are under `/api`. Public operational routes include `/health`, `/api/docs`, and `/api/docs/openapi.json`.

## Authentication

Most API routes require Bearer token authentication:

```http
Authorization: Bearer <your-token>
```

Public routes:

- `GET /health`
- `GET /api/docs`
- `GET /api/docs/openapi.json`
- `GET /api/setup/status`
- `POST /api/setup/claim` before the instance has been claimed; requires `ATOMIC_SETUP_TOKEN` unless the server uses the insecure setup-token bypass flag
- OAuth discovery/register/authorize/token routes when enabled by `PUBLIC_URL`

See [Token Management](/self-hosting/token-management/) for creating and revoking tokens.

## Database Selection

Requests use the active database unless a database is specified.

To target a database explicitly:

```bash
curl "http://localhost:8080/api/atoms?limit=20" \
  -H "Authorization: Bearer <token>" \
  -H "X-Atomic-Database: <db-id>"
```

Most routes also accept `?db=<db-id>` because the server resolver checks both the `X-Atomic-Database` header and query string.

## Interactive Explorer

The generated API explorer is available at:

```text
/api/docs
```

The raw OpenAPI document is available at:

```text
/api/docs/openapi.json
```

The Atomic website also has an API explorer at `/api/explorer`. It loads the OpenAPI JSON published by the Atomic release workflow at:

```text
https://kenforthewin.github.io/atomic/openapi.json
```

Website deployments can override that source with `PUBLIC_ATOMIC_OPENAPI_URL`, which is useful for previews or forks.

## Endpoint Groups

| Category | Routes |
|----------|--------|
| Atoms | `/api/atoms`, bulk create, source list, source lookup, link suggestions, links, similar atoms |
| Tags | `/api/tags`, children, auto-tag targets |
| Search | `/api/search`, `/api/search/global` |
| Wiki | `/api/wiki`, generate, update, propose, proposal accept/dismiss, related tags, links, versions |
| Reports | `/api/reports`, `/api/reports/{id}`, `/api/reports/{id}/enabled`, `/api/reports/{id}/run`, `/api/reports/{id}/findings` |
| Findings | `/api/findings/{atom_id}/citations` |
| Dashboard | `/api/dashboard/featured-report` |
| Chat | `/api/conversations`, scopes, messages |
| Canvas | `/api/canvas/positions`, `/api/canvas/level`, `/api/canvas/global` |
| Graph | `/api/graph/edges`, `/api/graph/neighborhood/{atom_id}`, rebuild edges |
| Clustering | `/api/clustering/compute`, `/api/clustering`, connection counts |
| Embeddings | process pending, retry failed, re-embed all, reset stuck, status |
| Settings | `/api/settings`, provider tests, model lists |
| Providers | Ollama model/test routes and provider verification |
| Auth | `/api/auth/tokens` |
| Setup | `/api/setup/status`, `/api/setup/claim` |
| Databases | list, create, rename, delete, activate, set default, stats |
| Exports | markdown export jobs and downloads |
| Import | Obsidian vault import |
| Ingestion | single and batch URL ingestion |
| Feeds | feed CRUD and manual polling |
| Logs | `/api/logs` |
| Utils | sqlite-vec check and tag compaction |

## Common Request Examples

Create an atom:

```bash
curl -X POST http://localhost:8080/api/atoms \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "content": "# Note\n\nMarkdown content",
    "source_url": null,
    "published_at": null,
    "tag_ids": [],
    "skip_if_source_exists": false
  }'
```

Search:

```bash
curl -X POST http://localhost:8080/api/search \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"query": "vector databases", "mode": "hybrid", "limit": 20, "threshold": 0.3}'
```

Check pipeline status:

```bash
curl http://localhost:8080/api/embeddings/status \
  -H "Authorization: Bearer <token>"
```

## Response Format

Successful responses return JSON unless a route explicitly downloads a file.

Errors use:

```json
{
  "error": "Description of what went wrong"
}
```

## Pagination

List endpoints use endpoint-specific pagination. Atom lists support `limit`, `offset`, and cursor parameters:

```bash
curl "http://localhost:8080/api/atoms?limit=50&offset=0" \
  -H "Authorization: Bearer <token>"
```

## Realtime Events

Long-running operations emit WebSocket events on `/ws?token=<token>`. See [WebSocket Events](/api/websocket-events/).

## Related

- [Token Management](/self-hosting/token-management/)
- [Multi-Database](/guides/multi-database/)
- [WebSocket Events](/api/websocket-events/)
