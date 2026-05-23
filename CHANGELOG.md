# Changelog

All notable changes to Atomic are documented here.

## Unreleased

- Add **Reports** — scheduled research over your knowledge base, with each run writing a single cited finding as a first-class atom. Reports are configurable from a new top-nav view: schedule (cron + timezone, with presets and a custom escape hatch), source scope (tags + time window), context scope (same-as-source / all / specific tags), and citation policy (source-only / source-and-context).
- Add a curated template gallery for new reports: Daily Briefing, Weekly Contradiction Scan, Open Questions Status, and Themes This Month — each ships with a hand-written research prompt that pre-fills the editor.
- Add a dashboard featured-report picker so any report (not just the seeded Daily Briefing) can fill the dashboard widget. The pointer is per-database and syncs across windows via a new `dashboard-featured-changed` WebSocket event.
- Collapse the legacy daily briefing into Reports. The Daily Briefing is now the default seeded report (one per database); your existing briefing schedule, prompt, and history were migrated in place. Past briefings became finding atoms with their citations preserved. The `/api/briefings/*` REST routes and `BriefingReady` event are removed; equivalent functionality lives at `/api/reports/*` and the standard `atom-created` event filtered on `kind === 'report'`.
- Add a Run Now button on each report's detail view, with optimistic running state that clears via the success event or a periodic poll for the failure path.

## v1.38.0 — 2026-05-16

- Add `edit_atom` MCP tool for surgical atom edits (replace, insert after, append, replace all) usable by Claude and other MCP clients
- Improve wiki updates to recognize multi-level headings (h3+) as addressable sections, and gracefully skip invalid operations instead of aborting the entire update
- Fix wiki "N new atoms available" banner miscounting atoms in child tags, which could trigger spurious update prompts
- Fix wiki update banner reappearing after a no-op update by advancing the baseline when no changes are proposed

## v1.37.1 — 2026-05-14

- Improve MCP server reliability by replacing third-party HTTP transport with a custom implementation that handles strict client requirements for status codes and content types

## v1.37.0 — 2026-05-14

- Add configurable briefing schedule with daily or weekly frequency, custom time, and timezone support
- Add re-tag action in database settings to re-run auto-tagging across all atoms using the current tagging model while preserving manual tags and wiki-backed assignments
- Fix edge case where pipeline jobs could get stuck and stop processing

## v1.36.0 — 2026-05-13

- Add setup token requirement (`ATOMIC_SETUP_TOKEN`) for self-hosted instance claiming, with rate limiting and one-time-claim enforcement to prevent unauthorized access
- Add copy buttons for local server URL and API token in desktop Settings, making it easier to configure integrations like the Obsidian plugin
- Replace permissive CORS policy with an allowlist that permits only local origins and the configured public URL

## v1.35.2 — 2026-05-11

- Fix tag tree failing to scroll to the selected tag after the sidebar loads asynchronously or after expanding/collapsing tag groups
- Fix tag tree scroll area showing incorrect height after expanding or collapsing tag groups

## v1.35.1 — 2026-05-01

- Fix duplicate embedding runs caused by the pipeline scheduler, reducing unnecessary AI provider API calls and improving background processing efficiency

## v1.35.0 — 2026-05-01

- Add custom system prompt overrides for chat, briefing, and tagging — configurable from a new Prompts tab in Settings (wiki prompts moved here too)
- Add per-category descriptions for auto-tag targets, letting you guide the auto-tagger with natural-language instructions on what each category should capture

## v1.34.0 — 2026-04-30

- Add per-database settings overrides — when running multiple databases, AI provider, model, and other settings can now be customized independently for each database while inheriting workspace defaults
- Add override indicators in Settings showing whether each field uses the workspace default or a per-database override, with a one-click reset to restore the default
- Move the Auto-tagging toggle from the AI tab to the Tag Categories tab, where it can also be overridden per database

## v1.33.0 — 2026-04-29

- Add MCP `ingest_url` tool — AI clients (Claude, etc.) can now save web pages as atoms by URL, with automatic duplicate detection
- Remove the legacy native iOS app (replaced by the Capacitor-based mobile app)

## v1.32.4 — 2026-04-26

- Redesign briefing mini-canvas into an interactive subset view showing only the briefing's referenced atoms and their neighbors, with pan/zoom and clickable nodes that open the main canvas focused on the selected atom
- Improve hover pill rendering so long atom titles can extend past canvas edges instead of being clipped
- Improve atom label placement by automatically flipping labels to the side with more room near canvas edges

## v1.32.3 — 2026-04-26

- Rebuild the neighborhood graph view with Sigma — concentric layout sorted by similarity, always-visible title and tag labels, color-coded edges by relationship type (tag-only, semantic, or both), and hover dimming of non-neighbors

## v1.32.2 — 2026-04-26

- Add comprehensive user documentation covering concepts, getting started, guides, self-hosting, and API reference
- Expand OpenAPI spec with full annotations for OAuth, setup, export, embedding, and log endpoints
- Publish OpenAPI specification automatically with each release

## v1.32.1 — 2026-04-26

- Anchor atom preview popover to its canvas node so the popover follows during pan and zoom
- Fix canvas panning accidentally dismissing the atom preview popover by distinguishing drags from clicks
- Hide labels for unrelated atoms when a node is pinned, so only the selected atom and its neighbors are labeled
- Carry the dashboard preview's camera position into the full canvas view so it opens at the same framing

## v1.32.0 — 2026-04-26

- Add tab navigation — open atoms, wiki articles, and graphs in persistent tabs with per-tab back/forward history, drag-and-drop reordering, and Cmd/middle-click to open in a new tab
- Improve reader layout to use container queries so the two-column editor + side-panel split adapts to actual pane width instead of viewport width (fixes cramped layout when the chat sidebar is open)
- Fix URL and tab state drifting out of sync when dismissing overlays via Escape or tag-chip clicks, which previously caused a reload to silently reopen the dismissed entry

## v1.31.0 — 2026-04-26

- Add database export as a markdown ZIP archive with progress tracking, available from the Databases tab in Settings
- Fix toast notifications sometimes appearing behind other UI elements
- Improve sidecar process logging for better diagnostics

## v1.30.1 — 2026-04-26

- Add Gemini Embedding 2 Preview, Perplexity Embed V1 4B, and NVIDIA Nemotron VL (free) to the embedding model picker; remove the unavailable Codestral Embed 2505

## v1.30.0 — 2026-04-25

- Unify embedding and tagging into a single pipeline queue with a simplified progress banner showing remaining counts instead of separate progress bars
- Preserve existing text chunks when switching embedding models, making model changes faster by only re-embedding rather than re-chunking all atoms
- Fix settings modal layout on mobile with a horizontal scrollable tab bar instead of the sidebar navigation
- Automatically re-embed all databases when the embedding model or provider changes, not just when the vector dimension changes

## v1.29.0 — 2026-04-25

- Add atom links: type `[[` in the editor to insert Obsidian-style wiki links to other atoms, with autocomplete suggestions powered by title matching, keyword search, and semantic search fallback
- Add clickable link resolution in the editor — wiki links display the target atom's title and open it on click
- Add backend storage and API endpoints for atom links, automatically extracting and persisting `[[…]]` references when atoms are saved

## v1.28.1 — 2026-04-25

- Bundle all web fonts locally so the app renders correctly offline without needing to reach Google Fonts or Fontshare on launch

## v1.28.0 — 2026-04-24

- Add chat tools for creating, updating, and editing atoms — the chat agent can now create new notes, replace content, or apply targeted edits (replace, insert, append) when you ask it to
- Render atom references in chat messages as clickable titled links instead of raw IDs
- Refresh the open atom editor when content changes externally (e.g. after a chat agent edit) without disrupting in-progress typing
- Improve search palette with compact match rows, right-aligned expand/collapse toggle, and more readable keyboard-shortcut hints

## v1.27.2 — 2026-04-23

- Add expandable match sub-rows in the search palette — keyword results with multiple hits can be drilled into to see each match in context
- Improve keyboard navigation in the search palette: the selected row now stays in view during arrow-key scrolling, and hover highlights no longer flash on rows that slide past the cursor
- Fix search results silently losing atom previews when a keyword snippet collided with the preview field
- Fix match highlighting disappearing when search terms appear inside markdown links, images, or HTML content

## v1.27.1 — 2026-04-23

- Fix a crash in the editor when links or images contain multi-line titles

## v1.27.0 — 2026-04-23

- Add dedicated search palette (⌘P or /) that searches across atoms, wiki articles, chats, and tags with rich markdown snippets — the command palette for actions moves to ⌘⇧P
- Improve search-to-editor flow: selecting a search result now scrolls to and briefly highlights the matching text in the editor instead of opening the find panel
- Fix clicks near block widgets (e.g. tables) in the editor landing on the wrong line

## v1.26.1 — 2026-04-22

- Fix Docker build failing to resolve the @atomic/editor package introduced in v1.26.0

## v1.26.0 — 2026-04-22

- Replace the Milkdown/ProseMirror editor with a new CodeMirror 6 editor featuring Obsidian-style live preview — headings, emphasis, links, code blocks, and other markdown syntax render inline while editing, with raw tokens revealed only on the active line
- Add syntax highlighting for fenced code blocks using a Material Palenight palette, with per-token CSS variable overrides via `--atomic-editor-hl-*`
- Add WYSIWYG table rendering, inline image previews, task-list checkboxes, and bullet/ordered-list styling to the live-preview editor
- Fix mid-typing emphasis flicker where bold, italic, and strikethrough formatting toggled on and off while typing between delimiter pairs
- Improve editor performance: reduce bundle size from 2.66 MB to 1.12 MB by tree-shaking unused features and lazy-loading code-block grammars, and narrow widget invalidation so keystrokes in large documents no longer rebuild all decorations

## v1.25.0 — 2026-04-20

- Replace the CodeMirror markdown editor with Milkdown, a rich-text WYSIWYG editor built on ProseMirror — notes now render inline formatting, images, and code blocks live as you type
- Add slash command menu (type `/`) to quickly insert headings, lists, blockquotes, code blocks, and horizontal rules
- Add selection toolbar for toggling bold, italic, inline code, and links on highlighted text
- Add in-editor find (Cmd/Ctrl+F) with match highlighting and next/previous navigation
- Improve left panel transition animation when opening the note reader

## v1.24.1 — 2026-04-20

- Add manual "Auto-tag" button in the atom reader for tagless atoms, letting you trigger AI tagging on demand
- Improve embedding and tagging pipeline so autosaved drafts are reliably picked up and processed in the background
- Fix bug where editing an atom would not re-run AI tagging, leaving stale or missing tags after content changes
- Fix new-atom button getting hidden behind the chat sidebar when it opens

## v1.24.0 — 2026-04-19

- Add Obsidian-style live-preview markdown editor — edit mode now renders headings, links, emphasis, images, and lists as formatted text; clicking a line reveals its raw markdown for editing, with scroll position preserved across view/edit toggles
- Fix click-to-move-cursor and click-drag text selection in the editor, which previously landed on wrong positions in long documents
- Fix blank lines not appearing when pressing Enter multiple times, and fix list exit so typing after leaving a list is no longer styled as a list item

## v1.23.3 — 2026-04-18

- Improve diagnostic logging when auto-tagging is silently skipped due to missing API key, disabled setting, or no auto-tag targets configured

## v1.23.2 — 2026-04-18

- Fix OpenRouter onboarding flow failing on Docker/reverse-proxy deployments by moving the OAuth callback page out of the `/oauth/` path
- Fix MCP remote-auth consent screen (used by claude.ai) being incorrectly intercepted by the service worker, which caused users to land on the dashboard instead of the authorization page

## v1.23.1 — 2026-04-17

- Add collapsible and draggable popovers on the canvas — atom previews can now be collapsed to just the title bar, dragged freely around the viewport, and dismissed with a close button
- Add database selector to the browser extension — clipped atoms can now be sent to any database on the server, not just the default

## v1.23.0 — 2026-04-17

- Add a first-run welcome screen and guided capture options (URL, RSS feed, markdown folder, Apple Notes, MCP) shown on the dashboard when no atoms or briefings exist yet
- Add Capacitor Android app so the React frontend can run on Android devices alongside the existing iOS build
- Fix atom list layout overflow on mobile — titles now truncate properly and the source pill moves inline with tags on small screens
- Improve onboarding wizard by marking required steps and removing the redundant Skip button
- Fix OpenRouter connection test to use the free `/key` endpoint instead of burning credits on a chat completion

## v1.22.5 — 2026-04-16

- Add canvas hover emphasis that dims non-neighboring nodes and edges with an animated fade, making a hovered node's connections visually pop
- Improve server responsiveness by migrating core storage operations to async, preventing SQLite calls from blocking the request-handling runtime
- Add CI test workflow and expand integration test coverage for multi-database and embedding pipeline scenarios

## v1.22.4 — 2026-04-16

- Fix a startup crash when initializing the desktop app authentication token

## v1.22.3 — 2026-04-16

- Add Postgres-only deployment mode — the server no longer requires a local SQLite registry file, so Postgres deployments need no writable filesystem
- Add Postgres variants of Docker images (`atomic-server-postgres` and `atomic-postgres`) for containerized Postgres deployments
- Add `--storage` and `--database-url` flags to the `token` CLI command for managing API tokens against a Postgres backend
- Fix briefing citations leaking source URLs from other databases in shared-schema (Postgres) deployments
- Fix OAuth code redemption to use a single atomic update, preventing a partial-write race condition

## v1.22.2 — 2026-04-15

- Add recency filter (`since_days`) to Chat and MCP search tools, letting the AI agent narrow results to recent notes when answering time-sensitive questions (e.g. "what did I write last week?")

## v1.22.1 — 2026-04-15

- Fix scheduled tasks (e.g. daily briefing) only running for one database in multi-database deployments

## v1.22.0 — 2026-04-14

- Add Apple Notes importer — import notes directly from macOS Apple Notes with folder-based tags, duplicate detection, and protobuf-to-markdown conversion
- Clicking the source URL on an imported Apple Note now opens the original note in the Apple Notes app using the native `applenotes:` URL scheme
- Show a guided Full Disk Access prompt when Apple Notes import is blocked by macOS permissions, with a direct link to System Settings
- Reorganize the Integrations settings tab into collapsible sections (Markdown Folder, Apple Notes, MCP) for easier navigation

## v1.21.7 — 2026-04-13

- Improve internal release infrastructure

## v1.21.6 — 2026-04-13

- Add knowledge-graph canvas to the Obsidian plugin with curved edges, cluster-colored nodes, cluster labels, and five switchable color themes (Ember, Steel Violet, Aurora, Midnight, Mono)
- Add click-to-open on canvas nodes in the Obsidian plugin — clicking a node navigates to the corresponding Obsidian note
- Add real-time AI-processing progress (embedding and auto-tagging) to the Obsidian plugin onboarding flow so users can see indexing status after initial sync
- Surface previously-silent errors as user-visible notices in the Obsidian plugin (search failures, chat/wiki load errors, sync rename/delete failures)
- Add `source_prefix` filter to the server canvas endpoint, allowing clients to scope the knowledge graph to a specific vault or source
- Prepare the Obsidian plugin for community-directory distribution (MIT license, versions.json, user-facing README, Obsidian API-compliant icon rendering)

## v1.21.5 — 2026-04-13

- Add chat view to the Obsidian plugin with streaming messages, conversation history, and tag-scoped conversations
- Upgrade chat tool-call display: each retrieval step now shows as a persistent, collapsible card with status icon, tool name, and pretty-printed input/output — visible during streaming and preserved after completion
- Improve Obsidian wiki view with clickable citation cross-navigation, loading spinner, and filtered tag selector
- Fix canvas edges not appearing on initial load until a theme change
- Fix crash when viewing an empty atom via the MCP agent tools, and add pagination for large atoms to prevent context overflow
- Remove ~2,600 lines of unused legacy canvas views, drawer, and wiki components

## v1.21.4 — 2026-04-12

- Add URL-based routing — views, tag filters, and open atoms/wikis are now reflected in the URL, enabling browser back/forward navigation and deep links
- Add local cache for tag tree and atom list so the app paints instantly on launch instead of waiting for the network
- Add PWA support for the web build (manifest, service worker, app icons) so the hosted server can be installed as a standalone app on mobile and desktop
- Improve reconnect behavior: transient disconnects are hidden for 4 seconds instead of flashing a banner, and resuming from background reconnects immediately
- Fix overlay back/forward chevrons navigating outside the current overlay session; they now stay scoped to reader/graph/wiki entries and disable at stack boundaries
- Fix WebSocket reconnect race where resuming the app during a pending connection could orphan an in-flight socket

## v1.21.3 — 2026-04-12

- Bundle the MCP bridge with the desktop app and auto-discover auth tokens, so local MCP setup requires no manual token configuration
- Split MCP onboarding and settings into local (stdio) and remote (HTTP + token) modes, with a one-click token provisioning flow for remote connections
- Fix desktop users connected to a remote server seeing the local sidecar URL instead of the active server URL in Mobile and MCP setup sections
- Fix stale MCP config showing after switching between local and remote server modes in settings
- Fix SSE stream handling for multi-line data events in the MCP bridge

## v1.21.2 — 2026-04-12

- Add resizable chat sidebar with drag handle, default width increased to 480px (adjustable 320–800px), persisted across sessions
- Add animated thinking indicator with live retrieval step display while the chat agent searches your knowledge base
- Persist active chat conversation so reopening the sidebar or refreshing restores where you left off

## v1.21.1 — 2026-04-12

- Improve canvas label readability by preventing overlapping atom and cluster labels — largest nodes are prioritized in dense regions

## v1.21.0 — 2026-04-11

- Add Dashboard view with AI daily briefing — a new home screen featuring a scheduled, LLM-generated summary of recently captured atoms with clickable inline citations and an embedded canvas preview
- Add briefing history navigation with prev/next controls to browse past daily briefings
- Consolidate Grid and List into a single Atoms view with a compact layout sub-toggle, simplifying the top-level navigation to four modes: Dashboard, Atoms, Canvas, and Wiki
- Migrate ~170 inline SVG icons to Lucide React, reducing frontend bundle size by ~4 kB gzipped
- Improve reliability of structured LLM outputs (wiki synthesis, tag extraction, briefing) with unified retry logic, tolerant JSON parsing, and a prompt-based fallback for providers that ignore response_format

## v1.20.2 — 2026-04-11

- Cache the global canvas payload in memory with automatic invalidation on atom, tag, and edge changes — eliminates redundant PCA recomputation and makes the canvas load significantly faster after the first request
- Warm the canvas cache at server startup so the first canvas open is instant instead of waiting for a full recompute
- Optimize canvas metadata query from two correlated subqueries per atom to a single JOIN + GROUP BY, improving canvas load time for large knowledge bases
- Serialize concurrent cold-cache canvas rebuilds so multiple simultaneous requests share a single computation instead of racing

## v1.20.1 — 2026-04-11

- Fix release notification formatting in the CI pipeline (no user-facing changes).

## v1.20.0 — 2026-04-11

- Add configurable auto-tag categories — choose which top-level tags the AI auto-tagger is allowed to extend (e.g. disable People/Locations if you don't need them, or add your own like "Projects" or "Books"), manageable during onboarding and in Settings → Tags
- Add Obsidian plugin onboarding wizard with a 4-step setup flow, database selection, size-based sync batching, YAML frontmatter stripping, and real-time sync progress reporting
- Fix mobile layout — sidebar, chat, and filter controls now work correctly on small screens with a slide-in sidebar, full-width chat overlay, filter bottom-sheet, and an overflow menu for reader actions
- Fix Obsidian plugin resync loop when the target database already contains atoms — re-syncing to a populated database now deduplicates server-side instead of retrying endlessly
- Skip the onboarding wizard when connecting to a server that is already configured with an AI provider
- Fix Obsidian plugin wiki view to preserve citation markers for notes outside the current vault instead of stripping them
