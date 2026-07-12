---
title: API Tokens & MCP
description: Create API tokens for your Atomic Cloud tenant and connect Claude, Cursor, or any MCP client.
---

Your cloud tenant exposes the same REST API and MCP endpoint as any Atomic server. Both authenticate with **API tokens** you create in the app.

## Creating a token

In the web app at your subdomain, open **Settings → API tokens** and create a named token. Save the raw token immediately — it's shown only once, and you can revoke it from the same screen at any time.

Tokens come in two scopes:

- **Account tokens** work across your whole account — required for migrations and account-level API use.
- **Database tokens** are pinned to a single knowledge base — the right choice for an agent or integration that should only see one.

## MCP for Claude, Cursor, and friends

Your MCP endpoint is your subdomain plus `/mcp`:

```
https://<your-subdomain>.atomicapp.ai/mcp
```

Configure any Streamable-HTTP MCP client with a Bearer token:

```json
{
  "mcpServers": {
    "atomic": {
      "url": "https://<your-subdomain>.atomicapp.ai/mcp",
      "headers": { "Authorization": "Bearer <your-token>" }
    }
  }
}
```

To pin the agent to one knowledge base, add the database id:

```
https://<your-subdomain>.atomicapp.ai/mcp?db=<database-id>
```

The MCP tools (semantic search, reading and writing atoms, URL ingestion) are advertised by the server, so new tools appear in your agent automatically as Atomic gains them. For the general MCP guide — including claude.ai custom connectors and the desktop stdio bridge — see [MCP Server](/guides/mcp-server/).

## REST API

The full REST API works against your subdomain with the same Bearer token — browse it in the [API explorer](https://atomicapp.ai/api/explorer). Rate limits apply per account and are generous for personal use.
