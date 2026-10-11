---
title: AI Providers
description: Configure OpenRouter, OrcaRouter, Ollama, or an OpenAI-compatible provider for embeddings and LLM features.
---

Atomic's AI features require a configured provider. The provider is used for:

- Embeddings for semantic search, graph links, canvas clustering, and wiki retrieval
- Auto-tagging for new and updated atoms
- Wiki synthesis and wiki proposals
- Chat responses and chat tool use
- Daily briefings

Atomic supports OpenRouter, OrcaRouter, Ollama, and OpenAI-compatible APIs.

## OpenRouter

[OpenRouter](https://openrouter.ai/) is the default cloud provider and gives access to many hosted models.

1. Create an OpenRouter account.
2. Generate an API key.
3. In Atomic, go to Settings and select **OpenRouter** as the provider.
4. Paste your API key.
5. Choose models for embedding, tagging, wiki, and chat.

OpenRouter uses separate model settings for:

- **Embedding** - generating vector embeddings for semantic search
- **Tagging** - extracting tags from note content
- **Wiki** - synthesizing wiki articles
- **Chat** - agentic RAG conversations
- **Briefings** - generated with the wiki model

## Ollama

[Ollama](https://ollama.com/) runs models locally on your machine or on another host you control.

1. Install Ollama.
2. Pull the models you want to use.
3. In Atomic, go to Settings and select **Ollama** as the provider.
4. Confirm the Ollama host. The default is `http://127.0.0.1:11434`.
5. Select embedding and LLM models from the discovered model list.

Example:

```bash
ollama pull nomic-embed-text
ollama pull llama3.2
```

For best results with local models, use an embedding model such as `nomic-embed-text` and a capable chat model for tagging, wiki, chat, and briefings.

## OpenAI-Compatible APIs

Use the OpenAI-compatible provider for servers that expose OpenAI-style `/embeddings` and `/chat/completions` endpoints. This can include hosted gateways and local model servers.

Configure:

- Base URL
- Optional API key
- Embedding model
- LLM model
- Embedding dimension
- Context length
- Timeout

The server has a connection-test endpoint at `POST /api/settings/test-openai-compat`.

## OrcaRouter

[OrcaRouter](https://www.orcarouter.ai) is a model-routing gateway that unifies hosted models behind one OpenAI-compatible endpoint. It also runs gateway-level, zero-trust security for AI agents on the same endpoint — screening every prompt/response and governing every tool call on a default-deny basis, with no application code changes.

1. Create an OrcaRouter account and generate an API key (prefixed `sk-orca-`).
2. In Atomic, go to Settings and select **OrcaRouter** as the provider.
3. Paste your API key.
4. Choose the embedding and LLM models — use OrcaRouter model ids such as `openai/text-embedding-3-small` for embeddings and `openai/gpt-5-nano` for LLM tasks.

OrcaRouter routes to many hosted providers, so model ids are namespaced (`vendor/model`). The connection-test endpoint is at `POST /api/settings/test-orcarouter`.

## Defaults

Fresh databases seed these defaults:

| Setting | Default |
|---------|---------|
| `provider` | `openrouter` |
| `embedding_model` | `qwen/qwen3-embedding-8b` |
| `tagging_model` | `openai/gpt-5-nano` |
| `wiki_model` | `anthropic/claude-sonnet-5` |
| `chat_model` | `anthropic/claude-sonnet-5` |
| `ollama_host` | `http://127.0.0.1:11434` |
| `ollama_embedding_model` | `nomic-embed-text` |
| `ollama_llm_model` | `llama3.2` |
| `orcarouter_embedding_model` | `openai/text-embedding-3-small` |
| `orcarouter_llm_model` | `openai/gpt-5-nano` |
| `orcarouter_agentic_model` | `openai/gpt-5-nano` |
| `orcarouter_context_length` | `65536` |
| `orcarouter_timeout_secs` | `300` |
| `auto_tagging_enabled` | `true` |

## Changing Embedding Models

Changing embedding provider, model, or dimensions can require re-embedding existing atoms so semantic search and graph edges are consistent. Use the UI controls where available, or call:

```bash
curl -X POST http://localhost:8080/api/embeddings/reembed-all \
  -H "Authorization: Bearer <token>"
```

Check progress with:

```bash
curl http://localhost:8080/api/embeddings/status \
  -H "Authorization: Bearer <token>"
```

## Troubleshooting

- If semantic or hybrid search returns few results, check that embeddings are complete in `/api/embeddings/status`.
- If Ollama models do not appear, verify Ollama is running and reachable from the server host, not just your laptop.
- If local models fail during tagging or wiki generation, use a larger context length or a stronger LLM model.
- If OpenRouter calls fail, verify the API key and selected model IDs in Settings.
