/**
 * Typed same-origin fetch client for the cloud account plane.
 *
 * Every call is same-origin with `credentials: 'include'` so the `.<base>`
 * session cookie rides along on the tenant dashboard; the public app-host
 * routes (signup/login) ignore it. Responses are parsed into typed results and
 * the cloud error shapes — field validation, rate limiting, and the structured
 * billing/auth states — are surfaced as a discriminated {@link ApiError}.
 *
 * A `401` on an authenticated route bounces the browser to the app-host login
 * (the dashboard is cookie-authed; an expired cookie means "log in again").
 */

import { appHostLoginUrl } from './host';

/** A structured error parsed from a non-2xx cloud response. */
export class ApiError extends Error {
  /** HTTP status code. */
  readonly status: number;
  /**
   * The cloud error code, when the body carried one (`invalid_email`,
   * `subdomain_taken`, `rate_limited`, `account_read_only`, …). `null` for
   * transport failures or bodies without an `error` field.
   */
  readonly code: string | null;
  /** `Retry-After` seconds, when the server supplied one (429 / 503). */
  readonly retryAfterSeconds: number | null;

  constructor(
    status: number,
    code: string | null,
    message: string,
    retryAfterSeconds: number | null = null,
  ) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.retryAfterSeconds = retryAfterSeconds;
  }

  /** True when this is a network/transport failure (no HTTP response). */
  get isNetwork(): boolean {
    return this.status === 0;
  }
}

interface RequestOptions {
  method?: string;
  body?: unknown;
  /**
   * Whether a `401` should redirect to the app-host login. On by default for
   * authenticated dashboard calls; the public signup/login routes never 401,
   * so it's moot there.
   */
  redirectOnUnauthorized?: boolean;
  signal?: AbortSignal;
}

const NETWORK_MESSAGE =
  "We couldn't reach the server. Check your connection and try again.";
const GENERIC_MESSAGE = 'Something went wrong. Please try again.';

async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const { method = 'GET', body, redirectOnUnauthorized = true, signal } = options;

  const headers: Record<string, string> = { Accept: 'application/json' };
  let payload: BodyInit | undefined;
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    payload = JSON.stringify(body);
  }

  let response: Response;
  try {
    response = await fetch(path, {
      method,
      headers,
      body: payload,
      credentials: 'include',
      signal,
    });
  } catch (err) {
    if (err instanceof DOMException && err.name === 'AbortError') throw err;
    throw new ApiError(0, null, NETWORK_MESSAGE);
  }

  if (response.status === 401 && redirectOnUnauthorized) {
    // The cookie is missing or expired — send the browser to the login page on
    // the app host. We never resolve; the navigation supersedes this promise.
    window.location.assign(appHostLoginUrl());
    throw new ApiError(401, 'unauthorized', 'Your session has expired.');
  }

  const data = await safeJson(response);

  if (!response.ok) {
    throw toApiError(response, data);
  }

  return data as T;
}

async function safeJson(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function toApiError(response: Response, data: unknown): ApiError {
  const record = isRecord(data) ? data : {};
  const code = typeof record.error === 'string' ? record.error : null;
  const message =
    (typeof record.message === 'string' && record.message) ||
    fallbackMessage(response.status);

  const retryAfter =
    numberOrNull(record.retry_after_seconds) ?? headerSeconds(response);

  return new ApiError(response.status, code, message, retryAfter);
}

function fallbackMessage(status: number): string {
  if (status === 429) return 'Too many requests. Please wait a moment and try again.';
  if (status >= 500) return GENERIC_MESSAGE;
  return GENERIC_MESSAGE;
}

function headerSeconds(response: Response): number | null {
  const raw = response.headers.get('Retry-After');
  if (!raw) return null;
  const seconds = Number.parseInt(raw, 10);
  return Number.isFinite(seconds) ? seconds : null;
}

function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

// --- Account-plane methods (this phase: public signup/login) ----------------

export interface SignupLinkParams {
  email: string;
  subdomain: string;
}

export interface LoginLinkParams {
  email: string;
}

/** The neutral 200 both request-link routes return. */
export interface LinkRequestedResponse {
  status: string;
  message: string;
}

/**
 * `POST /signup/request-link`. Resolves on the neutral 200 ("a link is on its
 * way"); rejects with an {@link ApiError} on `400` validation
 * (`invalid_email` | `invalid_subdomain` | `subdomain_taken` |
 * `subdomain_reserved`) or `429` rate-limit.
 */
export function requestSignupLink(
  params: SignupLinkParams,
  signal?: AbortSignal,
): Promise<LinkRequestedResponse> {
  return request<LinkRequestedResponse>('/signup/request-link', {
    method: 'POST',
    body: params,
    redirectOnUnauthorized: false,
    signal,
  });
}

/**
 * `POST /login/request-link`. Always resolves on the neutral 200 when the
 * input is well-formed — the server is deliberately indistinguishable about
 * whether the account exists. Rejects only on `400 invalid_email` or `429`.
 */
export function requestLoginLink(
  params: LoginLinkParams,
  signal?: AbortSignal,
): Promise<LinkRequestedResponse> {
  return request<LinkRequestedResponse>('/login/request-link', {
    method: 'POST',
    body: params,
    redirectOnUnauthorized: false,
    signal,
  });
}

/** Low-level escape hatch for later dashboard methods. */
export const api = { request };
