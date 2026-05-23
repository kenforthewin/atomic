---
title: Reports
description: Scheduled research over your knowledge base. Each run produces a cited finding atom.
---

Reports are scheduled research tasks. Each report has a prompt, a source scope, a context scope, and a schedule; when it fires, an agent reads the configured corpus and writes a single **finding atom** with inline citations back to the source material. Findings are first-class atoms tagged into your knowledge base — they behave like any other note once written.

The default seeded report is the **Daily Briefing**, which fills the dashboard widget. You can edit it, scope it, disable it, or replace it with any other report.

## The Model

Every report has six configuration surfaces:

- **Name** — what shows up in the list, the tab strip, and the dashboard eyebrow.
- **Research prompt** — read verbatim by the agent. Be specific about scope, tone, and what counts as a citation.
- **Schedule** — a cron expression (6-field: `SEC MIN HOUR DOM MONTH DOW`) plus an IANA timezone. The editor exposes presets (daily, weekdays, weekly, hourly) plus a custom escape hatch.
- **Source scope** — which atoms count as "new material" for this run. A set of tags (or all tags) plus a time window: since last run, all time, last 24 hours, last 7 days, or last 30 days.
- **Context scope** — what the agent may search for related material *while* writing. Three modes:
  - *Same as source* — context = the source batch (cheap meta-investigation).
  - *All atoms* — the full per-database corpus (kind-filtered).
  - *Specific tags* — only atoms in the chosen tag subtree.
- **Citation policy** — *Cite source only* (the agent's `[N]` markers may only resolve to atoms in the source batch) or *Allow citing context atoms* (`semantic_search` results become citable too). The latter is what makes contradiction-detection and open-questions reports work.

## Findings

When a run succeeds, Atomic writes one atom with `kind = 'report'`, tagged with whatever output tags the report specifies. The atom carries:

- The agent's prose with `[N]` inline citation markers.
- Citation rows in a dedicated `report_findings_citations` table — markers resolve to specific atom IDs and an excerpt of the cited passage.
- Provenance pointing back at the report definition (survives report deletion via `ON DELETE SET NULL`).

Because findings are atoms, they show up in search, on the canvas, and in wiki synthesis like any other note. They are excluded from the default atom list (`?kinds=captured` is the UI's default filter); the Reports view and finding-specific endpoints surface them.

## Templates

The "New report" button opens a template gallery with four curated starting points:

| Template | What it does |
|---|---|
| **Daily Briefing** | A topic-scoped daily recap with citations. The seeded default already covers your full knowledge base; templates let you add briefings for specific tag subtrees. |
| **Weekly Contradiction Scan** | Finds statements in this week's new atoms that contradict, complicate, or qualify claims in older notes. Uses the dual-scope + context-citable design at its full capability. |
| **Open Questions Status** | Tracks unresolved questions across the corpus, reconciling them against new atoms each week. |
| **Themes This Month** | End-of-month synthesis: 3–5 themes that dominated the last 30 days, with a deep-dive recommendation. |

Each template pre-fills the editor; you can rename, retag, and rewrite the prompt before saving. "Start blank" skips the template and opens a blank editor.

## Run Now

The detail view has a Run Now button that dispatches a manual run. The server returns immediately with a 202 (the agent loop runs in the background); the UI marks the report as running, and:

- A successful run lands an `atom-created` WebSocket event for the new finding; the UI clears the running state and refreshes the findings list.
- A failed run records `last_error` on the report cache; the detail view polls every 30 seconds while running, detects the new error, and surfaces it as a toast.
- A 5-minute stale guard clears the optimistic running state if neither the event nor the poll resolves.

If a scheduled tick is in flight when you press Run Now, the backend's ledger returns `Skipped` and the UI honors the existing run.

## The Featured Report

The dashboard's "BRIEFING" widget reads from a per-database **featured report** pointer. By default it points at the seeded Daily Briefing. From the detail view, click the star icon next to the report name to feature it; the dashboard widget will swap to show its latest finding. On the dashboard, the eyebrow becomes a chevron-menu when more than one report exists, letting you switch between featured reports without leaving the page.

The pointer is cleared automatically when the featured report is deleted.

## REST API

| Endpoint | Description |
|---|---|
| `GET /api/reports` | List reports for the active database. |
| `GET /api/reports/{id}` | Read one report. |
| `POST /api/reports` | Create a report (`CreateReportRequest` body). |
| `PUT /api/reports/{id}` | Update a report (`UpdateReportRequest` body — all fields optional, minimal-patch). |
| `PATCH /api/reports/{id}/enabled` | Pause / resume without other changes. |
| `DELETE /api/reports/{id}` | Delete a report. Findings remain in your atoms; the featured pointer auto-clears if it referenced this id. |
| `POST /api/reports/{id}/run` | Manual run. Returns 202 + `RunNowResponse`. |
| `GET /api/reports/{id}/findings?limit=50` | List findings, most recent first. Returns `(ReportFinding, AtomWithTags)` tuples. |
| `GET /api/findings/{atom_id}/citations` | Citation rows for a finding atom. |
| `GET /api/dashboard/featured-report` | Read the per-DB featured pointer. |
| `PUT /api/dashboard/featured-report` | Set or clear the pointer. Broadcasts `DashboardFeaturedChanged` over WebSocket. |

## Schedule Format

The runner uses 6-field cron (`SEC MIN HOUR DOM MONTH DOW`, Sunday = 0). The editor accepts both 5- and 6-field input and normalizes 5-field to 6-field before saving. Examples:

```
0 0 9 * * *        # Daily at 09:00
0 0 9 * * 1-5      # Weekdays at 09:00
0 30 14 * * 1      # Monday at 14:30
0 0 * * * *        # Top of every hour
0 0 10 1 * *       # 1st of each month at 10:00
```

Timezone is optional but recommended; without one, schedules anchor on UTC.

## Multi-Database

Reports are database-scoped. Each data database keeps its own reports, findings, and featured pointer. The background scheduler iterates every database on each tick and runs whatever is due.

## Related

- [Atoms](/concepts/atoms/) — findings are atoms with `kind = 'report'`.
- [Semantic Search](/concepts/semantic-search/) — the agent's primary tool inside report runs.
- [AI Providers](/getting-started/ai-providers/) — reports use the configured chat model.
- [WebSocket Events](/api/websocket-events/) — `atom-created` (kind=report), `dashboard-featured-changed`.
