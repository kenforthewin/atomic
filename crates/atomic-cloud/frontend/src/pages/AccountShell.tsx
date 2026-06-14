import { NodeGraphBackdrop } from '../components/NodeGraphBackdrop';
import { Logo } from '../components/ui/Logo';
import { Spinner } from '../components/ui/Spinner';

/**
 * Placeholder shell for the authenticated tenant dashboard at `/account/*`.
 * The full overview / provider / billing / MCP / danger surfaces are built in
 * the next phase; this renders a branded loading frame so a deep link doesn't
 * hit a blank page in the interim.
 *
 * The cloud server redirects `/signup/complete` and `/login/complete` itself
 * (provision + 302 to the tenant), so this SPA never owns those — it only
 * shows a graceful frame if one is somehow rendered client-side.
 */
export function AccountShell() {
  return (
    <div className="relative min-h-dvh flex flex-col items-center justify-center bg-bg-primary text-text-primary overflow-hidden">
      <NodeGraphBackdrop />
      <div className="relative text-center px-6">
        <Logo className="h-7 mx-auto mb-6" />
        <div className="flex items-center justify-center gap-3 text-text-secondary">
          <Spinner className="w-5 h-5 text-accent" />
          <span className="text-sm">Loading your workspace…</span>
        </div>
      </div>
    </div>
  );
}
