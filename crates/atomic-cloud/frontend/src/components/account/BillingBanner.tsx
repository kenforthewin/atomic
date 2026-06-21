import { useState } from 'react';
import { Banner } from '../ui/Banner';
import type { BillingState } from '../../lib/api';
import { BillingFlowError, billingNotice, startBillingFlow } from '../../lib/billing';

interface BillingBannerProps {
  billingState: BillingState;
  trialEndsAt: string | null;
  /**
   * Whether Stripe is configured. When false the portal route 503s, so we drop
   * the "Manage billing" action (the page itself explains the deployment has no
   * billing) and keep the informational notice.
   */
  billingConfigured: boolean;
}

/**
 * A global banner driven by the account's billing serving state. `active`
 * renders nothing; the trial and dunning states each get a tone-appropriate
 * notice with an action that hands off to the right Stripe-owned route.
 *
 * A trialing account has no Stripe customer yet, so the portal route 409s
 * (`no_billing_customer`) — paying for the first time means *checkout*, not the
 * portal. Every other actionable state already has a customer and resolves
 * through the portal. Either way we go through {@link startBillingFlow} rather
 * than a bare `<a href>`: it probes the route, follows the real Stripe redirect
 * with a top-level navigation, and on a JSON error surfaces an in-app message
 * instead of painting a raw error page.
 *
 * The `suspended` state never reaches here — CloudAuth blocks a suspended
 * account before the overview loads, and the shell renders a dedicated
 * blocking screen — but it's handled defensively for completeness.
 */
export function BillingBanner({
  billingState,
  trialEndsAt,
  billingConfigured,
}: BillingBannerProps) {
  const notice = billingNotice(billingState, trialEndsAt);
  const [pending, setPending] = useState(false);
  const [flowError, setFlowError] = useState<string | null>(null);
  if (!notice) return null;

  // Trialing accounts have no Stripe customer yet, so they checkout; everyone
  // else already has one and manages through the portal.
  const path =
    billingState === 'trialing' ? '/api/billing/checkout?plan=pro' : '/api/billing/portal';
  const label = billingState === 'trialing' ? 'Add billing' : 'Manage billing';

  async function openBillingFlow() {
    setFlowError(null);
    setPending(true);
    try {
      // Resolves only into a navigation away; a rejection is a misconfig we
      // surface in-app rather than letting the browser land on raw JSON.
      await startBillingFlow(path);
    } catch (err) {
      setFlowError(
        err instanceof BillingFlowError
          ? err.message
          : 'We couldn’t start the billing session. Please try again in a moment.',
      );
      setPending(false);
    }
  }

  return (
    <Banner
      tone={notice.tone}
      title={notice.title}
      action={
        notice.action && billingConfigured ? (
          <button
            type="button"
            onClick={openBillingFlow}
            disabled={pending}
            aria-busy={pending || undefined}
            className="inline-flex items-center rounded-lg bg-bg-white/70 px-3 py-1.5 text-sm font-medium text-text-primary ring-1 ring-inset ring-current/20 transition-colors hover:bg-bg-white focus-visible:outline-2 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {pending ? 'Opening…' : label}
          </button>
        ) : undefined
      }
    >
      {notice.body}
      {flowError && <span className="mt-1 block text-sm font-medium">{flowError}</span>}
    </Banner>
  );
}
