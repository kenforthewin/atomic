---
title: Atomic Cloud
description: Hosted Atomic with managed AI — what it is, how to sign up, and what's preconfigured for you.
---

Atomic Cloud is the hosted version of Atomic. It runs the same open-source server as a self-hosted deployment, at a subdomain you choose (`you.atomicapp.ai`), with AI built in — embeddings, auto-tagging, wiki synthesis, and chat work the moment your account exists, with no API keys to create or paste.

If you'd rather run Atomic on your own hardware, that's fully supported and free — see [Self-Hosting](/getting-started/self-hosting/). The two are the same product, and your data can [move between them](/cloud/migrating/).

## Signing up

1. Go to [app.atomicapp.ai/signup](https://app.atomicapp.ai/signup), pick a subdomain, and enter your email.
2. Click the magic link that arrives — there is no password to create. Magic links are how you sign in from then on, too.
3. Your knowledge base is provisioned at `https://<your-subdomain>.atomicapp.ai` and a short onboarding wizard sets up tag categories and any integrations you want.

Every new account starts with a **14-day trial of the Pro plan** — no card required. See [Plans & Billing](/cloud/plans-and-billing/) for what happens after.

## What's managed for you

- **AI provider** — each account gets a managed AI key with a monthly allowance included in your plan. Embeddings, tagging, wiki generation, and chat are preconfigured; you never sign up with an AI provider yourself.
- **Backups** — your knowledge base is backed up automatically on a fixed schedule, to encrypted off-site storage.
- **Isolation** — every account gets its own database. Your content is never commingled with other tenants'.
- **Updates** — the server is upgraded for you; new Atomic features arrive without any action on your part.

## Your account dashboard

Everything account-level lives at `https://<your-subdomain>.atomicapp.ai/account`:

- **Overview** — plan, status, and usage at a glance.
- **Billing** — upgrade, manage your subscription, invoices, and payment method (via Stripe).
- **Provider** — optionally bring your own AI key instead of the managed one.
- **MCP & tokens** — connect agents and other clients; see [API Tokens & MCP](/cloud/api-tokens-and-mcp/).

## Every client works

Your cloud tenant speaks the same API as any Atomic server, so every client connects the same way a self-hosted server would:

- The **web app** at your subdomain (installable as a PWA on desktop and mobile).
- The **[browser extension](/cloud/browser-extension/)** for clipping pages.
- The **[iOS app](/guides/ios-app/)**, pointed at your subdomain.
- **[MCP](/cloud/api-tokens-and-mcp/)** for Claude, Cursor, and other agents.
- The **REST API** — see the [API reference](https://atomicapp.ai/api/explorer).
