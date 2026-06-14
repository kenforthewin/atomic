import { useState } from 'react';
import { Check, Copy, Plug } from 'lucide-react';
import { useAccount } from '../../lib/accountContext';
import { Card } from '../../components/ui/Card';

/**
 * MCP setup: display this account's MCP endpoint and the connect-and-authorize
 * instructions. The endpoint is `<tenant-origin>/mcp` (the overview computes
 * it); the OAuth discovery/authorize flow is server-handled — a client like
 * Claude Desktop connects to the URL and is walked through authorization. This
 * page only shows the URL and how to use it.
 */
export function Mcp() {
  const { overview } = useAccount();

  return (
    <div className="space-y-8">
      <header>
        <p className="text-xs font-medium uppercase tracking-wide text-text-muted">MCP</p>
        <h1 className="mt-1 font-display text-3xl tracking-tight md:text-4xl">
          Connect your <span className="italic">tools.</span>
        </h1>
        <p className="mt-2 text-text-secondary">
          Atomic speaks the Model Context Protocol, so AI clients like Claude can
          search and read your knowledge base directly. Add the endpoint below to
          your client and authorize it once.
        </p>
      </header>

      <Card>
        <div className="mb-3 flex items-center gap-2 text-text-secondary">
          <Plug className="h-4 w-4 text-accent" strokeWidth={1.75} aria-hidden="true" />
          <span className="text-sm font-medium">Your MCP endpoint</span>
        </div>
        <CopyField value={overview.mcp_url} label="MCP endpoint URL" />
        <p className="mt-3 text-sm text-text-muted">
          One endpoint covers your whole account — every knowledge base, under a
          single account-scoped grant. The first time a client connects, you’ll
          be asked to sign in and approve access; there’s no token to copy by
          hand.
        </p>
      </Card>

      <Card>
        <h2 className="font-medium text-lg">Add it to Claude Desktop</h2>
        <ol className="mt-4 space-y-3 text-sm text-text-secondary">
          <Step n={1}>
            Open <span className="font-medium text-text-primary">Settings → Connectors</span> in
            Claude Desktop.
          </Step>
          <Step n={2}>
            Choose <span className="font-medium text-text-primary">Add custom connector</span> and
            paste your MCP endpoint URL.
          </Step>
          <Step n={3}>
            When prompted, sign in to Atomic and approve access. Claude can then
            search and cite your atoms in conversation.
          </Step>
        </ol>
        <p className="mt-4 text-sm text-text-muted">
          Any MCP-compatible client works the same way — point it at the endpoint
          and complete the one-time authorization.
        </p>
      </Card>

      <Card>
        <h2 className="font-medium text-lg">How authorization works</h2>
        <p className="mt-2 text-sm text-text-secondary leading-relaxed">
          Atomic uses OAuth 2.0, so your client registers itself and walks you
          through a standard sign-in-and-approve flow — no secret keys to paste
          or rotate. The grant is scoped to your account and you can revoke it at
          any time by removing the connector. Clients discover the flow
          automatically from{' '}
          <a
            href={discoveryUrl(overview.mcp_url)}
            target="_blank"
            rel="noreferrer"
            className="font-medium text-accent transition-colors hover:text-accent-dark"
          >
            the resource metadata
          </a>
          .
        </p>
      </Card>
    </div>
  );
}

/**
 * The MCP protected-resource discovery URL for this endpoint. The MCP URL is
 * `<origin>/mcp`; its discovery document lives at
 * `<origin>/.well-known/oauth-protected-resource/mcp` (the per-resource form
 * cloud serves — see `oauth_routes.rs`). We derive the origin from the MCP URL
 * so this stays correct regardless of the deployment's base domain.
 */
function discoveryUrl(mcpUrl: string): string {
  try {
    const url = new URL(mcpUrl);
    return `${url.origin}/.well-known/oauth-protected-resource/mcp`;
  } catch {
    // Defensive: if mcp_url is ever malformed, fall back to a relative path on
    // the current origin (the dashboard is served from the tenant origin).
    return '/.well-known/oauth-protected-resource/mcp';
  }
}

function Step({ n, children }: { n: number; children: React.ReactNode }) {
  return (
    <li className="flex gap-3">
      <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-accent-subtle text-xs font-semibold text-accent">
        {n}
      </span>
      <span className="leading-relaxed">{children}</span>
    </li>
  );
}

/** A read-only URL field with copy-to-clipboard feedback. */
function CopyField({ value, label }: { value: string; label: string }) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard blocked (insecure context, denied permission): leave the
      // field selectable so the user can copy manually. No error surface
      // needed — the value is right there to select.
    }
  }

  return (
    <div className="flex items-stretch gap-2">
      <input
        readOnly
        aria-label={label}
        value={value}
        onFocus={(e) => e.currentTarget.select()}
        className="min-w-0 flex-1 rounded-xl border border-border bg-bg-secondary px-3.5 py-2.5 font-mono text-sm text-text-primary focus:border-accent focus:outline-none"
      />
      <button
        type="button"
        onClick={copy}
        aria-label={copied ? 'Copied' : 'Copy MCP endpoint'}
        className="inline-flex shrink-0 items-center gap-1.5 rounded-xl border border-border bg-bg-white px-4 text-sm font-medium text-text-primary transition-colors hover:border-accent/30 hover:bg-accent-subtle/50 focus-visible:outline-2"
      >
        {copied ? (
          <>
            <Check className="h-4 w-4 text-emerald-600" aria-hidden="true" />
            Copied
          </>
        ) : (
          <>
            <Copy className="h-4 w-4" aria-hidden="true" />
            Copy
          </>
        )}
      </button>
    </div>
  );
}
