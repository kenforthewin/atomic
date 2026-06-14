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
