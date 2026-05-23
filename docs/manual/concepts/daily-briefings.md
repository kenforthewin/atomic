---
title: Daily Briefings
description: The default seeded report — a daily cited recap of recently captured atoms.
---

The Daily Briefing is the default **report** that's seeded into every Atomic database on first run. It fills the dashboard widget out of the box: each morning it summarizes recently captured atoms into a short cited briefing.

It's no longer its own primitive — the briefing was generalized into **Reports** in v1.39, and the Daily Briefing is now just one instance of that. Everything you used to do with briefings (configure the schedule, edit the prompt, view history) you now do as a report.

## The Reframe

| Before | Now |
|---|---|
| A hard-coded scheduled task. | A seeded report you can edit, scope, disable, or delete. |
| One per database. | Any number of reports per database; one is "featured" on the dashboard. |
| Briefing stored in its own table. | Each run writes a regular atom with `kind = 'report'`. |
| `/api/briefings/*` REST routes. | `/api/reports/*` and `/api/dashboard/featured-report`. |
| `BriefingReady` WebSocket event. | Standard `atom-created` event (filter on `kind === 'report'`). |
| `task.daily_briefing.*` settings. | The report row itself — schedule, prompt, scope all live on the `reports` table. |

If you had a customized briefing prompt or schedule before upgrading, it carried forward into the seeded Daily Briefing report. Past briefings were migrated into finding atoms with their citations preserved.

## Editing the Daily Briefing

Open the Reports view (Telescope icon in the top nav), find the seeded "Daily Briefing" row, and click into it. The schedule editor offers daily / weekly / hourly presets and a custom-cron escape hatch. The prompt is freely editable.

You can also add **additional** topic-scoped briefings from the template gallery (e.g. "Daily AI Briefing" scoped to an AI tag subtree) and feature whichever one you want on the dashboard via the star icon on the detail view.

## Disabling It

Open the Daily Briefing's detail view and toggle the "Enabled" switch in the editor, or delete the report outright. Past findings stay in your knowledge base; the dashboard widget will show its empty state.

## Related

- [Reports](/concepts/reports/) — the full primitive
- [Atoms](/concepts/atoms/) — what findings are
- [WebSocket Events](/api/websocket-events/) — `atom-created` for kind=report
