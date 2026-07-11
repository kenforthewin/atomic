# Atomic

Atomic is a personal knowledge base that turns freeform markdown notes ("atoms") into a semantically-connected, AI-augmented knowledge graph. It runs as a Tauri desktop app, a headless HTTP server, or both simultaneously.

# You

You are an expert software developer and architect. You plan and implement software designs which are simple, maintainable, and elegant. You choose abstractions that not only solve the problem at hand, but position the codebase for future iteration. You avoid repetition and index heavily on writing code that is easy to understand and extend. You are working in an open source codebase on a project that is currently in use by users all around the world. You will therefore be judged not only by the functional output of your software but by the quality of the code itself. Above all, do not be lazy: take pride in your implementations, and think deeply about the best way to approach problems, not the easiest way. Quality is everything.

## Core Concepts

**Atoms** are the fundamental unit — markdown notes with optional source URLs and hierarchical tags. When an atom is created or updated, an asynchronous pipeline automatically:
1. Chunks the content using markdown-aware boundaries (respecting code blocks, headers, paragraphs)
2. Generates vector embeddings via the configured AI provider
3. Extracts and assigns tags using LLM structured outputs (if auto-tagging is enabled)
4. Builds semantic edges to other atoms based on embedding similarity

This pipeline is fire-and-forget from the caller's perspective — the caller receives the saved atom immediately while embedding/tagging runs in the background, with progress reported via callbacks.

**Tags** form a hierarchical tree. Auto-extracted tags are organized under category parents (Topics, People, Locations, Organizations, Events). Tags serve as both organizational structure and scoping mechanism for wiki generation and chat conversations.

**Wiki articles** are LLM-synthesized summaries of all atoms under a given tag, with inline citations linking back to source atoms. They support incremental updates — when new atoms are tagged, only the new content is sent to the LLM to integrate into the existing article.

**Chat** is an agentic RAG system. Conversations can be scoped to specific tags, and the agent has tools to search the knowledge base semantically during conversation. Responses stream back through the same callback system used by embeddings.

**Canvas** is a spatial visualization where atoms are positioned using d3-force simulation. Atoms sharing tags are linked, and a custom similarity force pulls semantically-related atoms together. Positions are persisted so the layout is stable across sessions.

## Architecture: Core + Thin Wrappers

The central architectural principle is the separation of **business logic** from **transport**. All domain logic lives in `atomic-core`, a standalone Rust crate with no framework dependencies. Every client is a thin wrapper that adapts `atomic-core` to a specific transport mechanism.

```
                    ┌─────────────────┐
                    │   atomic-core   │
                    │  (all logic)    │
                    └────────┬────────┘
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
    ┌─────────────┐  ┌──────────────┐  ┌───────────┐
    │  src-tauri   │  │atomic-server │  │ mcp-bridge│
    │ (Tauri IPC)  │  │(REST+WS+MCP)│  │(stdio→HTTP)│
    └──────┬──────┘  └──────┬───────┘  └─────┬─────┘
           │                │                 │
    ┌──────▼──────┐  ┌──────▼───────┐        │
    │   React UI   │  │  HTTP clients│  ┌─────▼──────┐
    │(Tauri or HTTP)│  │ (iOS, etc.) │  │ MCP clients│
    └─────────────┘  └──────────────┘  │(Claude,etc)│
                                       └────────────┘
```

### `atomic-core` — The Facade

`AtomicCore` is a `Clone` wrapper around `Arc<Database>` that exposes every operation: CRUD, search, embedding, wiki generation, chat, clustering, tag compaction, and import. It is completely transport-agnostic.

The key design decision is **callback-based eventing**: operations that produce async events (embedding, chat) accept `Fn(EmbeddingEvent)` or `Fn(ChatEvent)` closures. The core doesn't know or care how events are delivered — it just calls the closure. This makes it usable from any Rust context without pulling in Tauri, actix, or any framework.

### `src-tauri` — Desktop Wrapper

The Tauri app spawns `atomic-server` as a **sidecar process** and exposes a single IPC command (`get_local_server_config`) that returns the server's base URL and auth token. The frontend then connects to the sidecar over HTTP/WebSocket, exactly as it would to a standalone `atomic-server`. On exit, Tauri kills the sidecar.

### `atomic-server` — Headless HTTP Wrapper

The standalone server wraps `atomic-core` with a full REST API (~78 routes) plus a WebSocket endpoint and a Streamable HTTP MCP endpoint. The same thin-wrapper pattern applies: each route handler unpacks HTTP request params, calls `core.method()`, returns JSON.

Events flow through `tokio::sync::broadcast` — route handlers send `ServerEvent` variants into the channel, and WebSocket clients receive them. The event bridge converts `atomic-core` callbacks into broadcast messages, mirroring how Tauri bridges them to `app_handle.emit()`.

Authentication uses named, revocable API tokens stored as SHA-256 hashes. A default token is auto-created on first run. Managed via CLI subcommands or REST endpoints.

### Frontend Transport Abstraction

The React frontend defines a `Transport` interface with `invoke()` and `subscribe()` methods. Both Tauri and browser environments use `HttpTransport`, which maps command names to HTTP specs (method, path, body/query transforms) via a command map and receives events via WebSocket. In Tauri, the frontend first calls `get_local_server_config` via Tauri IPC to get the sidecar's URL and token, then uses `HttpTransport` for everything else.

This means the React code is transport-unaware — it calls `transport.invoke('create_atom', args)` and `transport.subscribe('embedding-complete', handler)` regardless of environment.

## AI Provider Abstraction

AI capabilities are pluggable via trait-based providers:
- `EmbeddingProvider` — batch embedding generation
- `LlmProvider` — chat completions
- `StreamingLlmProvider` — streaming completions with tool calling

Two implementations exist: **OpenRouter** (cloud, default) and **Ollama** (local). Factory functions return `Arc<dyn Trait>` based on the configured provider type. Adding a new provider requires implementing the traits and adding a factory branch — no changes to embedding, wiki, chat, or any consumer code.

Provider configuration is stored in the settings table (SQLite key-value pairs). OpenRouter uses separate model settings for embedding, tagging, wiki, and chat. Ollama auto-discovers available models from the running server.

## Workspace Structure

```
Cargo.toml                  # Workspace root
crates/atomic-core/         # All business logic (no framework deps)
crates/atomic-server/       # Headless REST + WS + MCP server
crates/mcp-bridge/          # stdio-to-HTTP MCP bridge (for Claude Desktop, etc.)
src-tauri/                  # Tauri desktop app (sidecar launcher)
src/                        # React frontend (TypeScript)
mobile/ios/                 # Capacitor iOS wrapper around the React frontend
mobile/android/             # Capacitor Android wrapper around the React frontend
scripts/                    # Import, build, and database utilities
databases/                  # Local data dir (registry.db + per-DB files)
```

## Tech Stack

- **Core**: Rust, SQLite + sqlite-vec (vector search), rusqlite, tokio, reqwest
- **Desktop**: Tauri v2
- **Server**: actix-web, clap (CLI), tokio broadcast channels
- **Frontend**: React 18, TypeScript, Vite 6, Tailwind CSS v4, Zustand 5
- **Editor**: CodeMirror 6 (markdown editing), react-markdown (rendering)
- **Canvas**: d3-force (simulation), react-zoom-pan-pinch (interaction)
- **Virtualization**: @tanstack/react-virtual
- **AI**: OpenRouter or Ollama (pluggable), tiktoken for token counting

## Common Commands

```bash
# Development
npm run tauri dev             # Desktop app (frontend + Tauri)
npm run dev                   # Frontend only
cargo check                   # Check all workspace crates
cargo test                    # Run all tests
cargo check -p atomic-core    # Check specific crate

# Standalone server (--data-dir defaults to current directory)
cargo run -p atomic-server -- serve --port 8080
cargo run -p atomic-server -- --data-dir /path/to/data serve --port 8080

# Token management
cargo run -p atomic-server -- token create --name "my-laptop"
cargo run -p atomic-server -- token list
cargo run -p atomic-server -- token revoke <token-id>

# Migrations (see also `migrate sqlite` for direct SQLite → Postgres copies)
# Push a local database to a remote Atomic server / cloud tenant; the token is
# the destination's (account-scoped for cloud), env: ATOMIC_MIGRATE_TARGET_TOKEN
cargo run -p atomic-server -- migrate push --target-url https://you.atomicapp.ai --target-token <token>

# Capacitor mobile (shared React frontend packaged as a webview)
npm run dev:mobile:ios         # Build, install, launch iOS Capacitor app with Vite HMR
npm run dev:mobile:android     # Build, install, launch Android Capacitor app with Vite HMR
npm run build:mobile           # Production build for both platforms (cap sync ios + android)
npm run cap:open:ios           # Open mobile/ios/App in Xcode
npm run cap:open:android       # Open mobile/android in Android Studio

# Production
npm run tauri build
npm run release:patch         # Bump version and build
```

## Database

SQLite with sqlite-vec extension is the default. A parallel Postgres backend lives under `storage/postgres/` and implements the same storage traits — used when running against shared infrastructure. Everything below describes the SQLite layout; Postgres swaps file-per-DB for schema-per-DB but exposes the same API. Multi-database support with a registry/data split:

### File Layout

When running via `atomic-server` from the repo root, databases live in `./databases/`:
```
databases/
  registry.db          # Shared config: settings, API tokens, database metadata
  default.db           # Default knowledge base
  {uuid}.db            # Additional databases (created via API or multi-db)
```

When running the Tauri desktop app, the base directory is platform-specific:
- macOS: `~/Library/Application Support/com.atomic.app/`
- Linux: `~/.local/share/com.atomic.app/`

### Registry vs Data Databases

- **`registry.db`** holds cross-database state: settings (provider config, model selection), API tokens, and the `databases` table mapping UUIDs to names.
- **Data databases** (`default.db`, `{uuid}.db`) each hold atoms, tags, chunks, embeddings, wiki articles, conversations, messages, semantic edges, and atom positions. Each data DB *also* has its own `settings` table (seeded by `migrate_settings` in `db.rs`) — this is where per-DB configuration lives.

### Multi-DB Gotchas (read before adding per-DB state)

Atomic can run with N data databases under a single process. `atomic-server`'s background loops (`main.rs`) fan out over every database returned by `manager.list_databases()` — feed polling, the scheduled-task runner, etc. Any code that reads or writes "per-database" state needs to actually be per-database. Two traps:

1. **`AtomicCore::get_settings()` / `set_setting()` silently route to `registry.db` when a registry is attached** (see `lib.rs` — it checks `self.registry` first). That's correct for *global* config (provider, models, wiki prompt), but it means any helper that calls these methods from within a per-DB context is writing a single shared key, not a per-DB key. If you want per-DB settings, call `core.storage().get_all_settings_sync()` / `set_setting_sync()` directly to bypass the registry. The scheduler state helpers (`scheduler::state`) are the canonical example.

2. **Background loops that iterate all DBs need per-(task, db) state and locks, or only one DB will ever make progress.** The scheduled-task runner holds a `(task_id, db_id)` keyed lock map and each task's `last_run` lives in its DB's settings table. Feed polling follows the same shape. If you add a new cross-DB background job, mirror that pattern — don't introduce a singleton timestamp or lock.

Quick checklist when touching anything that runs per-DB:
- Does this state need to be isolated between DBs? If yes, write via `core.storage()`, not `core.get_settings()` / `set_setting()`.
- Does this lock/guard need to be per-DB? If yes, key it on `db_id`, not just `task_id` or a process-global `Mutex`.
- If you're adding a scheduled or polling loop in `atomic-server::main`, iterate `manager.list_databases()` and resolve a fresh `core` per DB via `manager.get_core(&db_id)`.

### Direct Access with sqlite3

```bash
# List all databases
sqlite3 databases/registry.db "SELECT id, name, is_default FROM databases;"

# Check atom/embedding status in a specific database
sqlite3 databases/{uuid}.db "SELECT embedding_status, COUNT(*) FROM atoms GROUP BY embedding_status;"

# Check settings (provider, models, etc.)
sqlite3 databases/registry.db "SELECT key, value FROM settings;"
```

Key tables in data databases: `atoms`, `atom_chunks`, `atom_tags`, `tags`, `semantic_edges`, `vec_chunks` (sqlite-vec virtual table), `wiki_articles`, `conversations`, `chat_messages`.

### Similarity

Computed from sqlite-vec's Euclidean distance on normalized vectors: `similarity = 1.0 - (distance² / 2.0)`. Default thresholds: 0.5 for related atoms/semantic edges, 0.3 for semantic search and wiki chunk selection.

## Design System

Dark theme (Obsidian-inspired). Backgrounds: `#1e1e1e`/`#252525`/`#2d2d2d`. Accent: purple (`#7c3aed`). Three-panel layout: fixed-width left panel (tag tree, navigation), flexible main view (canvas/grid/list), overlay right drawer (editor, viewer, wiki, chat).

Frontend state is managed by Zustand stores: `atoms`, `tags`, `ui`, `settings`, `wiki`, `chat`, `databases`. The `ui` store tracks selected tag filter, drawer state, view mode, and search query. View mode (canvas/grid/list) persists to localStorage.
