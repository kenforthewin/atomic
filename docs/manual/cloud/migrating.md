---
title: Migrating In and Out
description: Move an existing knowledge base into Atomic Cloud from the desktop app or a self-hosted server — and export everything back out.
---

Atomic Cloud runs the same server as self-hosted Atomic, so knowledge bases move between them with their content intact. Embeddings are always regenerated on the destination (they're derived data), so semantic search comes back on its own shortly after a migration finishes.

## From the desktop app

Open **Settings → Migrate to Cloud** in the desktop app. It walks you through pointing at your tenant, choosing what to migrate, and watching progress. Your local data stays untouched — migration copies, it doesn't move.

## From a self-hosted server

A single command pushes a knowledge base from a self-hosted (SQLite) `atomic-server` into your cloud tenant:

```bash
atomic-server --data-dir /path/to/data migrate push \
  --target-url https://<your-subdomain>.atomicapp.ai \
  --target-token <account-token>
```

- The token must be an **account-scoped** token from your cloud tenant (Settings → API tokens). Prefer passing it via the `ATOMIC_MIGRATE_TARGET_TOKEN` environment variable so it stays out of shell history.
- With multiple local databases, name one with `--database <id-or-name>`; the command lists your databases if the choice is ambiguous.
- Progress streams in the terminal; the job runs snapshot-first, so the source server keeps serving while it copies.
- Feed polling on the migrated knowledge base is paused after import so the cloud copy doesn't immediately re-fetch everything; re-enable feeds when you're ready (or pass `--resume-feeds`).

Plan limits apply on arrival: a knowledge base larger than the Free plan's 250 atoms needs an active trial or Pro. See [Plans & Billing](/cloud/plans-and-billing/).

## Getting everything back out

Your whole knowledge base exports as portable markdown at any time — every atom with its content, tags, and metadata:

```bash
curl -X POST https://<your-subdomain>.atomicapp.ai/api/databases/<db-id>/exports/markdown \
  -H "Authorization: Bearer <token>"
```

Poll the returned job and download the archive when it's ready (the same export API as self-hosted — see [Data and Backups](/self-hosting/data-and-backups/)). The archive imports directly into a self-hosted Atomic via [Importing Data](/guides/importing-data/), which re-tags and re-embeds on your own hardware.

Exports work on every plan and in every account state — including read-only and paused. Leaving is always possible; that's the point of running on open source.
