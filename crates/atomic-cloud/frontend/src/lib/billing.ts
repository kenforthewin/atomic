/**
 * Pure, presentation-only billing logic shared by the dashboard's billing
 * surfaces (the global {@link ../components/account/BillingBanner}, the
 * {@link ../pages/account/Billing} page, and the {@link ../pages/account/Overview}
 * status pill). Centralizing it keeps one source of truth for "which message
 * for which `billing_state`" — the thing the unit tests pin — instead of three
 * drifting copies.
 *
 * Nothing here fetches or renders; it maps a {@link BillingState} (+ the trial
 * deadline) to a tone and copy. The portal/checkout *actions* live in the
 * components, since they navigate the browser.
 */

import type { BillingState } from './api';
import type { BannerTone } from '../components/ui/Banner';
import type { PillTone } from '../components/ui/StatusPill';
import { daysUntil } from './format';

/** The compact status badge shown next to the plan name. */
export interface BillingDescriptor {
  tone: PillTone;
  label: string;
}

/** Map a billing serving state to its status-pill tone + short label. */
export function billingDescriptor(state: BillingState): BillingDescriptor {
  switch (state) {
    case 'active':
      return { tone: 'success', label: 'Active' };
    case 'trialing':
      return { tone: 'accent', label: 'Trial' };
    case 'past_due':
      return { tone: 'warning', label: 'Past due' };
    case 'read_only':
      return { tone: 'warning', label: 'Read-only' };
    case 'suspended':
      return { tone: 'error', label: 'Suspended' };
  }
}

/**
 * The full notice for a billing state: the banner tone, a title, a body, and
 * whether a "Manage billing" action is warranted (every non-active state
 * resolves through Stripe, so all four show it). `active` returns `null` — no
 * banner, nothing to nag about.
 *
 * `trialEndsAt` shapes the trial title's day count; it's ignored for the other
 * states.
 */
export interface BillingNotice {
  tone: BannerTone;
  title: string;
  body: string;
  action: boolean;
}

export function billingNotice(
  state: BillingState,
  trialEndsAt: string | null,
): BillingNotice | null {
  switch (state) {
    case 'active':
      return null;
    case 'trialing': {
      const days = daysUntil(trialEndsAt);
      const title =
        days === null
          ? 'Your free trial is active.'
          : days <= 0
            ? 'Your free trial ends today.'
            : `${days} day${days === 1 ? '' : 's'} left in your free trial.`;
      return {
        tone: 'info',
        title,
        body: 'You have full access to the paid tier. Add billing before it ends to keep your provider, higher limits, and AI allowance.',
        action: true,
      };
    }
    case 'past_due':
      return {
        tone: 'warning',
        title: 'Your last payment didn’t go through.',
        body: 'You still have full access for now. Update your payment method to avoid an interruption.',
        action: true,
      };
    case 'read_only':
      return {
        tone: 'warning',
        title: 'Your account is read-only.',
        body: 'A payment is overdue, so writes are paused — your data is safe and fully readable. Update billing to restore full access.',
        action: true,
      };
    case 'suspended':
      return {
        tone: 'error',
        title: 'Your account is suspended.',
        body: 'Serving is paused for non-payment. Your data is retained in full — update billing to restore access.',
        action: true,
      };
  }
}

/**
 * Start a Stripe-owned billing flow (the `/api/billing/checkout` or
 * `/api/billing/portal` GET routes). Both 302 cross-origin to a Stripe URL on
 * success, or return a JSON error (`unknown_plan`, `billing_not_configured`,
 * `409` no-customer-yet, upstream `502`) on a misconfig/edge.
 *
 * Stripe owns checkout — there are no Elements here. We just must not navigate
 * the browser straight onto the route, because a JSON error would paint a raw
 * error page. So we *probe* it first with `redirect: 'manual'`:
 *
 * - On the cross-origin redirect the response is opaque (`type:
 *   'opaqueredirect'`, status `0`) — we can't read the `Location`, so we let the
 *   browser follow it by navigating to the route URL (a top-level navigation,
 *   which is what the Customer Portal / Checkout require anyway). The probe is a
 *   cheap idempotent GET; re-running it to redirect for real is fine.
 * - On a JSON error response we read its message and reject, so the caller can
 *   surface an in-app banner instead of a raw error page.
 *
 * Rejects with a {@link BillingFlowError} carrying a friendly message; never
 * resolves on the success path (the navigation supersedes this promise).
 */
export class BillingFlowError extends Error {}

export async function startBillingFlow(path: string): Promise<never> {
  let response: Response;
  try {
    response = await fetch(path, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      credentials: 'include',
      redirect: 'manual',
    });
  } catch {
    throw new BillingFlowError(
      "We couldn't reach the billing service. Check your connection and try again.",
    );
  }

  // The cross-origin 302 to Stripe surfaces as an opaque redirect; follow it
  // with a real top-level navigation.
  if (response.type === 'opaqueredirect' || (response.status >= 300 && response.status < 400)) {
    window.location.assign(path);
    // The navigation supersedes this promise; keep the caller's `await` pending.
    return new Promise<never>(() => {});
  }

  // Anything else is an error the route reported as JSON — surface it in-app
  // rather than navigating onto it.
  throw new BillingFlowError(await billingErrorMessage(response));
}

/** Map a non-redirect billing route response to a friendly, in-app message. */
async function billingErrorMessage(response: Response): Promise<string> {
  let body: unknown = null;
  try {
    const text = await response.text();
    body = text ? JSON.parse(text) : null;
  } catch {
    body = null;
  }
  const code =
    body && typeof body === 'object' && typeof (body as Record<string, unknown>).error === 'string'
      ? ((body as Record<string, unknown>).error as string)
      : null;
  const message =
    body && typeof body === 'object' && typeof (body as Record<string, unknown>).message === 'string'
      ? ((body as Record<string, unknown>).message as string)
      : null;

  switch (code) {
    case 'unknown_plan':
      return 'That plan isn’t available for checkout on this deployment. Please contact support.';
    case 'billing_not_configured':
      return 'Billing isn’t enabled on this deployment, so there’s nothing to set up.';
    default:
      return (
        message ?? 'We couldn’t start the billing session. Please try again in a moment.'
      );
  }
}
