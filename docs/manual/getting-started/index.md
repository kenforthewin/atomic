---
title: Getting Started
description: Get up and running with Atomic, a personal knowledge base powered by semantic search and AI.
---

Atomic turns freeform markdown notes ("atoms") into a semantically connected, AI-augmented knowledge graph.

## What Is Atomic?

Atomic has three common ways to run:

- **[Atomic Cloud](/cloud/)** - the hosted service at a subdomain you choose, with AI included and nothing to configure. The fastest way to start.
- **Desktop app** - a local-first Tauri app that starts its own `atomic-server` sidecar on your machine.
- **Self-hosted server** - a headless `atomic-server` plus optional web frontend for remote access, mobile use, browser clipping, and MCP over HTTP.

All three use the same core engine and HTTP API, and your data can [migrate between them](/cloud/migrating/). Cloud is the quickest start; the desktop app is simplest for one-person local use; self-hosting is best when you want everything on your own hardware.

When you create or update a note in Atomic, an asynchronous pipeline can automatically:

1. **Chunk** the content using markdown-aware boundaries
2. **Generate vector embeddings** via your configured AI provider
3. **Extract and assign tags** using LLM structured outputs
4. **Build semantic edges** to related notes based on embedding similarity

This happens in the background. You can keep writing while Atomic processes the note.

## Key Features

- **Semantic Search** - Find ideas by meaning, not just exact keywords.
- **Wiki Synthesis** - Generate articles with inline citations to your atoms.
- **Agentic Chat** - Converse with your knowledge base using RAG.
- **Spatial Canvas** - Visualize atoms and relationships as a force-directed graph.
- **Auto-Tagging** - Extract hierarchical tags from new content.
- **Reports** - Scheduled research over your notes; each run produces a cited finding atom.
- **RSS and URL Ingestion** - Save web pages and subscribe to feeds.
- **MCP Integration** - Connect Claude and other AI assistants.
- **Mobile Access** - Connect the iOS app to a self-hosted server.

## Choose Your Setup

| Setup | Best For |
|-------|----------|
| [Desktop App](/getting-started/installation/) | Personal local use, bundled server, no separate hosting |
| [Self-Hosted Server](/getting-started/self-hosting/) | Remote access, web UI, mobile, browser extension, MCP over HTTP |
| [iOS App](/guides/ios-app/) | Mobile reading, writing, search, and sharing to a self-hosted server |

## Next Steps

- [Install the desktop app](/getting-started/installation/)
- [Set up a self-hosted server](/getting-started/self-hosting/)
- [Configure AI providers](/getting-started/ai-providers/)
- [Connect the browser extension](/guides/browser-extension/)
- [Connect the iOS app](/guides/ios-app/)
