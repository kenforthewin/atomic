---
title: Plans & Billing
description: Atomic Cloud plans, the 14-day trial, what happens when it ends, and how billing works.
---

## Plans

| | Free | Pro — $12/month |
|---|---|---|
| Atoms | 250 | Unlimited |
| Knowledge bases | 1 | Unlimited |
| Storage | 100 MB | 10 GB |
| Included AI | $0.50 / month | $10 / month |
| Frontier AI models | — | ✓ |

Every feature — semantic search, wiki synthesis, chat, canvas, reports, extension, iOS, API, and MCP — is available on both plans. The plans differ in capacity, AI allowance, and which models power chat and reports.

**Included AI** is a monthly allowance on your account's managed AI key. It covers embeddings, auto-tagging, wiki synthesis, chat, and reports, and resets every month. Typical note-taking uses a small fraction of it; if you hit the cap, AI features pause until the month resets or you upgrade — your notes themselves are never affected. You can also bring your own AI key at any time from the dashboard, which bypasses the allowance entirely.

## The trial

Every new account starts with **14 days of Pro, no card required**. Three days before it ends you'll get an email; when it ends:

- If you've upgraded, nothing changes.
- Otherwise your account moves to the Free plan automatically.
- If you're holding more than Free covers (more than 250 atoms, extra knowledge bases, or over 100 MB), your account becomes **read-only**: everything stays visible, searchable, and exportable, but writes are blocked until you trim below the limits or upgrade.

**Nothing is ever deleted** by a downgrade, an expired trial, or a failed payment. That's a hard rule, not a best effort.

## Upgrading, managing, cancelling

Everything runs through your dashboard's billing page at `https://<your-subdomain>.atomicapp.ai/account/billing`:

- **Upgrade** starts a Stripe Checkout — we never see or store your card.
- **Manage** opens the Stripe customer portal: invoices, payment method, plan changes.
- **Cancel** any time; your subscription runs to the end of the paid period, then the account moves to Free (with the same read-only-if-over-limits rule as trial expiry).

## Failed payments

If a renewal payment fails, Stripe retries it automatically and emails you. Meanwhile your account enters a grace period with **full access**; if the payment stays unresolved for several days the account becomes read-only, and eventually paused. Your data is retained through every stage and comes back the moment payment succeeds — or you can export it and leave. See [Migrating](/cloud/migrating/).
