import { ApiError } from '../../lib/api';

/**
 * Map a failed account deletion to user-facing copy. The server's three
 * relevant outcomes get tailored guidance; everything else falls back to the
 * server's own message (or a generic transport message).
 *
 * - `503 deletion_busy` — a backup pass holds the account's lock; retryable,
 *   nothing was changed.
 * - `403 account_scope_required` — a database/MCP-pinned token reached the
 *   route (it can't destroy the whole account). This shouldn't happen from the
 *   cookie-authed dashboard, but it's handled rather than swallowed.
 *
 * Lives in its own module (not alongside the `Danger` component) so the
 * component file exports only components — keeping Vite's fast-refresh boundary
 * clean, the same convention as `accountContext.ts`.
 */
export function deletionErrorMessage(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.status === 503 && err.code === 'deletion_busy') {
      return 'A backup of your account is in progress. Nothing was changed — try again in a few seconds.';
    }
    if (err.status === 403 && err.code === 'account_scope_required') {
      return 'This session can’t delete the whole account. Sign in to your account on the web to delete it.';
    }
    return err.message;
  }
  return 'Something went wrong. Please try again.';
}
