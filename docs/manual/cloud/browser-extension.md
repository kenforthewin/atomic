---
title: Browser Extension on Cloud
description: Connect the Atomic web clipper to your Atomic Cloud tenant.
---

The [Atomic web clipper](/guides/browser-extension/) works with cloud tenants exactly like self-hosted servers — it just needs your tenant URL and a token.

## Setup

1. Install the extension and open its options page.
2. **Server URL**: your full tenant address, including the scheme:

   ```
   https://<your-subdomain>.atomicapp.ai
   ```

3. **API token**: create one in the web app under **Settings → API tokens** (see [API Tokens & MCP](/cloud/api-tokens-and-mcp/)) and paste it in.
4. Click **Test connection** — you should see it succeed, and clipped pages will land in your knowledge base as atoms, tagged and embedded automatically.

## Troubleshooting

- **"Connection failed" with a correct token** — make sure the URL starts with `https://` and has no path after the domain. The extension talks directly to your tenant's API; a URL typo is the most common cause.
- **Clips not appearing** — check the atom list's newest items (clips process asynchronously; tagging can take a few seconds), and confirm the token hasn't been revoked in Settings → API tokens.
