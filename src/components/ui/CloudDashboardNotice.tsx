import { ExternalLink } from 'lucide-react';

/**
 * Shared note shown to Atomic Cloud tenants in place of the self-hosted
 * "copy server URL + API token" external-client flows (MCP, browser extension,
 * mobile). On cloud those flows are broken — the server URL is empty, the token
 * endpoint 404s, and the generated MCP url is a bare relative '/mcp'. Cloud
 * tenants manage external clients via OAuth from the account dashboard, which is
 * served same-origin at `/account`.
 */
export function CloudDashboardNotice({
  message = 'Connect MCP clients, the browser extension, and mobile apps from your account dashboard.',
}: {
  message?: string;
}) {
  return (
    <div className="space-y-3">
      <p className="text-xs text-[var(--color-text-secondary)]">{message}</p>
      <a
        href="/account"
        className="inline-flex items-center gap-1.5 rounded-md bg-[var(--color-accent)] px-3 py-1.5 text-xs font-medium text-white hover:opacity-90 transition-opacity"
      >
        Open account dashboard
        <ExternalLink className="w-3.5 h-3.5" strokeWidth={2} />
      </a>
    </div>
  );
}
