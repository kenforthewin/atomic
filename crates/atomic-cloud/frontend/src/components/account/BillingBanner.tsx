import { Banner } from '../ui/Banner';
import type { BillingState } from '../../lib/api';
import { billingNotice } from '../../lib/billing';

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
 * notice, with a "Manage billing" action that navigates to the portal route
 * (a server 302 into Stripe's Customer Portal) for the states where paying
 * resolves the issue.
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
  if (!notice) return null;

  return (
    <Banner
      tone={notice.tone}
      title={notice.title}
      action={
        notice.action && billingConfigured ? (
          <a
            href="/api/billing/portal"
            className="inline-flex items-center rounded-lg bg-bg-white/70 px-3 py-1.5 text-sm font-medium text-text-primary ring-1 ring-inset ring-current/20 transition-colors hover:bg-bg-white focus-visible:outline-2"
          >
            Manage billing
          </a>
        ) : undefined
      }
    >
      {notice.body}
    </Banner>
  );
}
