/**
 * Typed errors for Atomic Cloud data-plane guards.
 *
 * Cloud tenants can hit a handful of guard responses on the two highest-
 * frequency flows (atom create, chat): plan-quota exhaustion, out-of-AI-credits,
 * the dunning read-only / storage-restricted write block, and the anti-abuse
 * rate limiter. The server already returns a friendly human sentence plus an
 * `upgrade_url` and (for 429s) a `Retry-After` in the response body — see the
 * guard responses in `atomic-cloud` (`quota.rs`, `backpressure.rs`,
 * `billing_guard.rs`, `rate_limit.rs`, `chat_streams.rs`).
 *
 * `HttpTransport` parses those 402/429 responses into a {@link CloudGuardError}
 * so the UI can show the human message and an upgrade CTA instead of the bare
 * machine code (`quota_exceeded`, `out_of_ai_credits`, …). The error's
 * `message` is the human sentence, so existing `String(error)` callers keep
 * showing readable text with no change.
 */

/** Path the upgrade CTA routes to when the server doesn't supply an explicit one. */
export const BILLING_PATH = '/account/billing';

/** Machine error codes the cloud data-plane guards return on 402/429. */
export type CloudGuardCode =
  | 'quota_exceeded'
  | 'out_of_ai_credits'
  | 'account_read_only'
  | 'account_storage_restricted'
  | 'account_suspended'
  | 'rate_limited'
  | 'too_many_streams';

/**
 * A cloud guard denial (HTTP 402 or 429) carrying everything the UI needs to
 * render a friendly banner with an upgrade CTA.
 *
 * Thrown only in cloud-tenant mode; self-hosted / Tauri error handling is
 * unchanged. Extends `Error` with `message` set to the server's human sentence
 * so `String(error)` and generic `error.message` reads stay readable.
 */
export class CloudGuardError extends Error {
  readonly name = 'CloudGuardError';
  /** HTTP status — 402 (payment/quota/credits/write-block) or 429 (rate limit). */
  readonly status: number;
  /** Machine code from the response body's `error` field. */
  readonly code: CloudGuardCode | string;
  /** Where the upgrade CTA should link, always populated (falls back to {@link BILLING_PATH}). */
  readonly upgradeUrl: string;
  /** Seconds to wait before retrying, when the server supplied a hint (429s). */
  readonly retryAfter?: number;

  constructor(args: {
    status: number;
    code: CloudGuardCode | string;
    message: string;
    upgradeUrl: string;
    retryAfter?: number;
  }) {
    super(args.message);
    this.status = args.status;
    this.code = args.code;
    this.upgradeUrl = args.upgradeUrl;
    this.retryAfter = args.retryAfter;
  }
}

/** Type guard for {@link CloudGuardError} (works across realms via the `name` brand). */
export function isCloudGuardError(value: unknown): value is CloudGuardError {
  return value instanceof CloudGuardError;
}

/**
 * Parse a cloud guard 402/429 response body into a {@link CloudGuardError}.
 *
 * Returns `null` when the body isn't a recognizable cloud-guard payload (no
 * `error` code and no `message`), so the caller can fall back to the existing
 * raw-text error path. `retryAfterHeader` is the response's `Retry-After`
 * header value, used when the body omits `retry_after_seconds`.
 */
export function parseCloudGuardError(
  status: number,
  body: unknown,
  retryAfterHeader: string | null,
): CloudGuardError | null {
  if (typeof body !== 'object' || body === null) return null;
  const obj = body as Record<string, unknown>;

  const code = typeof obj.error === 'string' ? obj.error : undefined;
  const message = typeof obj.message === 'string' ? obj.message : undefined;
  // Not a structured guard payload — let the caller use the legacy path.
  if (!code && !message) return null;

  const upgradeUrl =
    typeof obj.upgrade_url === 'string' && obj.upgrade_url.length > 0
      ? obj.upgrade_url
      : BILLING_PATH;

  const retryAfter =
    typeof obj.retry_after_seconds === 'number'
      ? obj.retry_after_seconds
      : parseRetryAfterHeader(retryAfterHeader);

  return new CloudGuardError({
    status,
    code: code ?? 'unknown',
    // The human sentence is what reaches the UI; fall back to the code only if
    // the server somehow omitted it.
    message: message ?? code ?? 'Request blocked.',
    upgradeUrl,
    retryAfter,
  });
}

/** A `Retry-After` header is either delta-seconds or an HTTP-date; we only honor seconds. */
function parseRetryAfterHeader(value: string | null): number | undefined {
  if (!value) return undefined;
  const seconds = Number(value);
  return Number.isFinite(seconds) && seconds >= 0 ? seconds : undefined;
}
