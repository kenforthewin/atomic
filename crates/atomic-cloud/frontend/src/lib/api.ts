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
  /**
   * The raw parsed error body, for the handful of structured fields beyond
   * `error`/`message` a caller needs (e.g. the suspended gate's `upgrade_url`,
   * a dimension error's `required_dimension`). `null` for transport failures
   * or non-JSON bodies.
   */
  readonly body: Record<string, unknown> | null;

  constructor(
    status: number,
    code: string | null,
    message: string,
    retryAfterSeconds: number | null = null,
    body: Record<string, unknown> | null = null,
  ) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.retryAfterSeconds = retryAfterSeconds;
    this.body = body;
  }

  /** A string field from the parsed error body, or null. */
  field(key: string): string | null {
    const value = this.body?.[key];
    return typeof value === 'string' ? value : null;
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

  return new ApiError(response.status, code, message, retryAfter, record);
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

// --- Tenant dashboard methods (cookie-authed, same-origin) ------------------
//
// Every method below runs on a tenant subdomain against a same-origin route
// under CloudAuth, authenticated by the `.<base>` session cookie that rides on
// `credentials: 'include'`. A `401` bounces to the app-host login (the default
// `redirectOnUnauthorized`).

/** A purchasable/active plan, as the overview reports it. */
export interface PlanSummary {
  id: string;
  name: string;
}

/**
 * The account's billing serving state (`accounts.billing_state`). Drives the
 * dashboard banners; mirrors the server's `BillingState`.
 */
export type BillingState =
  | 'active'
  | 'trialing'
  | 'past_due'
  | 'read_only'
  | 'suspended';

/** Live resource usage against the plan's ceilings. */
export interface UsageSummary {
  atoms_used: number | null;
  atom_limit: number | null;
  kb_count: number | null;
  kb_limit: number | null;
  /** The plan's monthly managed-AI allowance, in cents (advisory). */
  ai_credits_monthly_cents: number | null;
}

/**
 * The active provider's status — never any key material. `model_config` is the
 * plaintext config bag (model ids, base-URL overrides); `configured` is false
 * when no provider is set up.
 */
export interface ProviderSummary {
  configured: boolean;
  origin: 'managed' | 'user' | null;
  provider: 'openrouter' | 'openai_compat' | null;
  model_config: Record<string, unknown> | null;
  last_validated_at: string | null;
  last_validation_error: string | null;
}

/** The dashboard's single read: everything the overview view needs. */
export interface AccountOverview {
  subdomain: string;
  email: string;
  plan: PlanSummary;
  billing_state: BillingState;
  /**
   * Whether Stripe is configured on this deployment. When false, the
   * portal/checkout routes 503 (`billing_not_configured`), so the billing page
   * disables those actions and explains why instead of navigating onto a raw
   * error.
   */
  billing_configured: boolean;
  trial_ends_at: string | null;
  usage: UsageSummary;
  provider: ProviderSummary | null;
  mcp_url: string;
}

/** `GET /api/account/overview`. */
export function getOverview(signal?: AbortSignal): Promise<AccountOverview> {
  return request<AccountOverview>('/api/account/overview', { signal });
}

/**
 * The full provider status (`GET /api/account/provider`) — the same `origin` /
 * `provider` / `model_config` / validation surface as the overview's
 * `provider`, plus the managed allowance `usage` and the create/rotate
 * timestamps. Never carries a key.
 */
export interface ProviderStatus {
  configured: boolean;
  origin: 'managed' | 'user' | null;
  provider: 'openrouter' | 'openai_compat' | null;
  model_config: Record<string, unknown> | null;
  created_at: string | null;
  rotated_at: string | null;
  last_used_at: string | null;
  last_validated_at: string | null;
  last_validation_error: string | null;
  /** Managed-key allowance usage, best-effort; `null` for BYOK or on lookup
   * failure. */
  usage: ManagedUsage | null;
}

/** Managed-key allowance usage from the provisioning API. */
export interface ManagedUsage {
  usage_usd: number;
  limit_usd: number | null;
  limit_remaining_usd: number | null;
  disabled: boolean;
}

/** `GET /api/account/provider`. */
export function getProviderStatus(signal?: AbortSignal): Promise<ProviderStatus> {
  return request<ProviderStatus>('/api/account/provider', { signal });
}

/** The BYOK providers the cloud accepts. */
export type ByokProvider = 'openrouter' | 'openai_compat';

/** The BYOK `model_config` vocabulary (server: `BYOK_ALLOWED_KEYS`). */
export interface ByokModelConfig {
  embedding_model?: string;
  llm_model?: string;
  openrouter_base_url?: string;
  openai_compat_base_url?: string;
  embedding_dimension?: number;
}

export interface SaveByokParams {
  provider: ByokProvider;
  api_key: string;
  model_config?: ByokModelConfig;
}

/** The success body of a provider write — carries the loud re-embed warning
 * when an embedding-model change invalidated the stored vectors. */
export interface ProviderWriteResult {
  status: string;
  provider?: string;
  origin?: string;
  reembed_warning?: string | null;
  model_config?: Record<string, unknown>;
}

/**
 * `PUT /api/account/provider` — BYOK save. The key is validated against the
 * provider **before** anything is stored; a validation failure rejects with an
 * {@link ApiError} (`code: 'provider_validation_failed'`) carrying the
 * provider's message verbatim, and nothing is stored.
 */
export function saveByokProvider(
  params: SaveByokParams,
  signal?: AbortSignal,
): Promise<ProviderWriteResult> {
  return request<ProviderWriteResult>('/api/account/provider', {
    method: 'PUT',
    body: params,
    signal,
  });
}

/** `POST /api/account/provider/activate` — flip the active stored row. */
export function activateProvider(
  params: { provider: ByokProvider; origin: 'managed' | 'user' },
  signal?: AbortSignal,
): Promise<ProviderWriteResult> {
  return request<ProviderWriteResult>('/api/account/provider/activate', {
    method: 'POST',
    body: params,
    signal,
  });
}

/**
 * `PUT /api/account/provider/models` — model selection on the active row.
 * A dimension change is rejected (`code: 'embedding_dimension_unsupported'`);
 * an uncurated managed choice is `model_not_curated`; a same-dimension
 * embedding-model change succeeds with a `reembed_warning`.
 */
export function updateModels(
  modelConfig: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<ProviderWriteResult> {
  return request<ProviderWriteResult>('/api/account/provider/models', {
    method: 'PUT',
    body: { model_config: modelConfig },
    signal,
  });
}

/**
 * `DELETE /api/account` — hard-delete the account. The body must name the
 * account's own subdomain (`{confirm}`) or the server 400s
 * (`confirmation_mismatch`). On success the credentials are revoked, so the
 * caller should navigate away immediately.
 */
export function deleteAccount(
  confirm: string,
  signal?: AbortSignal,
): Promise<{ status: string; subdomain: string }> {
  return request('/api/account', {
    method: 'DELETE',
    body: { confirm },
    // The success response revokes this session; don't treat a follow-up 401
    // as "log in again" — the caller navigates to the app host itself.
    redirectOnUnauthorized: false,
    signal,
  });
}

/** Low-level escape hatch for any route without a typed helper. */
export const api = { request };

// --- Admin plane (app host, is_admin sessions; 404 to everyone else) ---------

/** One row of `GET /admin/api/accounts`. */
export interface AdminAccount {
  id: string;
  subdomain: string;
  email: string;
  status: string;
  plan_id: string | null;
  plan_pinned: boolean;
  is_admin: boolean;
  billing_state: string;
  trial_ends_at: string | null;
  created_at: string;
  last_backup_at: string | null;
}

/** One `plans` catalogue row (`GET /admin/api/plans`) — the picker's source. */
export interface AdminPlan {
  id: string;
  name: string;
  monthly_price_cents: number;
  atom_limit: number | null;
  kb_limit: number | null;
  storage_bytes_limit: number | null;
  ai_credits_monthly_cents: number;
}

/** `GET /admin/api/accounts/{id}` — the detail drawer's read. */
export interface AdminAccountDetail {
  id: string;
  subdomain: string;
  email: string;
  status: string;
  plan_id: string | null;
  plan_pinned: boolean;
  billing_state: string;
  recent_transitions: {
    from: string | null;
    to: string | null;
    trigger: string;
    at: string;
  }[];
}

/**
 * The admin plane answers 404 to non-admins by design — callers treat a 404
 * as "this page does not exist for you", never as a data error, so none of
 * these redirect on unauthorized.
 */
export function adminListAccounts(signal?: AbortSignal): Promise<AdminAccount[]> {
  return request<AdminAccount[]>('/admin/api/accounts', {
    method: 'GET',
    redirectOnUnauthorized: false,
    signal,
  });
}

export function adminGetAccount(
  id: string,
  signal?: AbortSignal,
): Promise<AdminAccountDetail> {
  return request<AdminAccountDetail>(`/admin/api/accounts/${encodeURIComponent(id)}`, {
    method: 'GET',
    redirectOnUnauthorized: false,
    signal,
  });
}

export function adminListPlans(signal?: AbortSignal): Promise<AdminPlan[]> {
  return request<AdminPlan[]>('/admin/api/plans', {
    method: 'GET',
    redirectOnUnauthorized: false,
    signal,
  });
}

export function adminSetPlan(
  id: string,
  planId: string,
  pinned: boolean,
): Promise<{ plan_id: string; pinned: boolean }> {
  return request<{ plan_id: string; pinned: boolean }>(
    `/admin/api/accounts/${encodeURIComponent(id)}/plan`,
    {
      method: 'PUT',
      body: { plan_id: planId, pinned },
      redirectOnUnauthorized: false,
    },
  );
}

export function adminEvict(id: string): Promise<{ evicted: boolean }> {
  return request<{ evicted: boolean }>(
    `/admin/api/accounts/${encodeURIComponent(id)}/evict`,
    {
      method: 'POST',
      redirectOnUnauthorized: false,
    },
  );
}
