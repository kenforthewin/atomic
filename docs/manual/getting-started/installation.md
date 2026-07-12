---
title: Installation
description: Download and install the Atomic desktop app, or run Atomic from source.
---

The Atomic desktop app is the easiest way to run Atomic for local personal use. It bundles the React UI, starts a local `atomic-server` sidecar, and manages a local API token automatically.

:::tip[No install at all]
[Atomic Cloud](/cloud/) runs the same product hosted at a subdomain you choose, with AI included — nothing to download or configure.
:::

## Download

Download the latest desktop build from [GitHub Releases](https://github.com/kenforthewin/atomic/releases/latest). Releases are the canonical distribution channel for desktop builds.

Atomic is built with Tauri. The repository publishes desktop artifacts for the platforms available in each release, commonly macOS, Linux, and Windows.

## macOS

1. Download the `.dmg` or macOS archive from the latest release.
2. If using a DMG, open it and drag Atomic to Applications.
3. Launch Atomic.
4. If macOS blocks the app on first launch, right-click Atomic and choose **Open**.

## Linux and Windows

Download the matching artifact from [GitHub Releases](https://github.com/kenforthewin/atomic/releases/latest). Artifact names can change by release, so use the file that matches your OS and architecture.

## First Run

When you first launch Atomic, it will:

1. Create a local database in `~/Library/Application Support/com.atomic.app/`
2. Create a local API token named `desktop`
3. Start a local server sidecar at `http://127.0.0.1:44380`
4. Open the main window and connect the UI to the sidecar

On Linux, the local data directory is typically `~/.local/share/com.atomic.app/`. The desktop app also keeps a `sidecar.pid` file so it can clean up a stale server process after crashes or force quits.

## Desktop vs Remote Server

You do not need Docker or a separate server to use the desktop app locally. Use self-hosting when you want:

- Access from another computer or phone
- The native [iOS app](/guides/ios-app/)
- The [browser extension](/guides/browser-extension/) against a server reachable from the browser
- Remote MCP access from cloud or non-local agents
- A web UI at a hosted URL

The desktop app can also connect to a remote server from Settings if you want to use the desktop UI against a self-hosted instance.

## Configure AI

To enable embeddings, semantic search, auto-tagging, wiki synthesis, chat, and briefings, configure an AI provider during setup or later in Settings. See [AI Providers](/getting-started/ai-providers/).

## Run from Source

For development:

```bash
git clone https://github.com/kenforthewin/atomic.git
cd atomic
npm install
npm run tauri dev
```

For a server-only local run, use [Self-Hosting](/getting-started/self-hosting/) instead.
