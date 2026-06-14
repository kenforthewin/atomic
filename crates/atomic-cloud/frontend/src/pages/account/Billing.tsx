import { CreditCard, ExternalLink } from 'lucide-react';
import { useSearchParams } from 'react-router-dom';
import { useAccount } from '../../lib/accountContext';
import { Card } from '../../components/ui/Card';
import { StatusPill } from '../../components/ui/StatusPill';
import { Banner } from '../../components/ui/Banner';
import { UsageMeter } from '../../components/account/UsageMeter';
import type { BillingState } from '../../lib/api';
import { billingDescriptor } from '../../lib/billing';
import { daysUntil, formatCents, formatDate, formatUsage } from '../../lib/format';

/**
 * Billing = Stripe portal + status. We render the plan, the serving state, and
 * the trial/dunning context, then hand off to Stripe for everything money:
 * "Manage billing" navigates to the portal route (a server 302 into the
 * Customer Portal), and the upgrade CTA navigates to the checkout route. No
 * card entry, invoice tables, or Stripe Elements live here — Stripe owns that.
 *
 * The portal/checkout routes are `GET` redirects, so these are plain
 * navigations (`window.location` via a real `<a href>`), not fetches; a
 * full-page redirect to Stripe is exactly the intended flow, and a top-level
 * navigation is what the Customer Portal requires.
 *
 * When Stripe isn't configured on the deployment (`billing_configured: false`),
 * those routes 503 `billing_not_configured`; rather than navigate the browser
 * onto a raw error, we disable the actions and explain.
 */
export function Billing() {
  const { overview } = useAccount();
  const [params] = useSearchParams();
  const { plan, usage, billing_configured: configured } = overview;
  const status = billingDescriptor(overview.billing_state);
  const trialDays = daysUntil(overview.trial_ends_at);
  // The free tier (and the trial of it) can upgrade; a paid plan manages
  // through the portal. `pro` is the seeded purchasable tier.
  const canUpgrade = plan.id === 'free' || overview.billing_state === 'trialing';
  // Stripe redirects back to `/billing?status=success|cancel` after a checkout.
  // The subscription itself lands via the webhook (asynchronously), so we show
  // a gentle acknowledgement rather than asserting the new plan immediately.
  const checkoutResult = params.get('status');

  return (
    <div className="space-y-8">
      <header>
        <p className="text-xs font-medium uppercase tracking-wide text-text-muted">Billing</p>
        <h1 className="mt-1 font-display text-3xl tracking-tight md:text-4xl">
          Plan &amp; <span className="italic">billing.</span>
        </h1>
        <p className="mt-2 text-text-secondary">
          Manage your subscription and payment method. Payments are handled
          securely by Stripe — Atomic never sees your card.
        </p>
      </header>

      {checkoutResult === 'success' && (
        <Banner tone="success" title="Thanks — your checkout is complete.">
          Your subscription is being activated. It can take a moment to reflect
          here; refresh if your plan hasn’t updated.
        </Banner>
      )}
      {checkoutResult === 'cancel' && (
        <Banner tone="info" title="Checkout canceled.">
          No charge was made. You can upgrade whenever you’re ready.
        </Banner>
      )}

      {/* Plan + serving state */}
      <Card>
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div>
            <p className="text-sm text-text-muted">Current plan</p>
            <p className="mt-1 font-display text-2xl tracking-tight">{plan.name}</p>
            <p className="mt-1 text-sm text-text-muted">
              Monthly AI allowance: {formatCents(usage.ai_credits_monthly_cents)}
            </p>
          </div>
          <StatusPill tone={status.tone} dot>
            {status.label}
          </StatusPill>
        </div>

        <RecoveryNote
          state={overview.billing_state}
          trialDays={trialDays}
          trialEndsAt={overview.trial_ends_at}
        />

        <div className="mt-6 flex flex-wrap gap-3">
          {canUpgrade && configured && (
            <a
              href="/api/billing/checkout?plan=pro"
              className="group inline-flex items-center justify-center gap-2.5 rounded-xl bg-accent px-7 py-3.5 text-base font-medium text-white transition-all hover:bg-accent-dark hover:shadow-lg hover:shadow-accent/20 focus-visible:outline-2"
            >
              Upgrade to Pro
              <ExternalLink className="h-4 w-4 opacity-80" aria-hidden="true" />
            </a>
          )}
          {configured && (
            <a
              href="/api/billing/portal"
              className="inline-flex items-center justify-center gap-2.5 rounded-xl border border-border bg-bg-white px-7 py-3.5 text-base font-medium text-text-primary transition-all hover:border-accent/30 hover:bg-accent-subtle/50 focus-visible:outline-2"
            >
              <CreditCard className="h-5 w-5" strokeWidth={1.5} aria-hidden="true" />
              Manage billing
              <ExternalLink className="h-4 w-4 text-text-muted" aria-hidden="true" />
            </a>
          )}
        </div>

        {configured ? (
          <p className="mt-3 text-xs text-text-muted">
            “Manage billing” opens the Stripe Customer Portal, where you can
            update your card, view invoices, or cancel. Your knowledge base is
            never deleted for non-payment — it’s retained until you say
            otherwise.
          </p>
        ) : (
          <Banner tone="info" title="Billing isn’t enabled on this deployment." className="mt-4">
            This Atomic instance runs without Stripe, so there’s nothing to pay
            and no portal to open. Plan limits still apply.
          </Banner>
        )}
      </Card>

      {/* Usage against the plan's ceilings — the numbers that make the plan
          concrete. The fuller breakdown (with per-metric cards) lives on the
          overview; here we summarize the two limits the plan governs. */}
      <section aria-labelledby="usage-heading" className="space-y-4">
        <h2 id="usage-heading" className="font-medium text-lg">
          Usage this plan
        </h2>
        <Card className="space-y-5">
          <UsageRow
            label="Atoms"
            used={usage.atoms_used}
            limit={usage.atom_limit}
          />
          <UsageRow
            label="Knowledge bases"
            used={usage.kb_count}
            limit={usage.kb_limit}
          />
        </Card>
      </section>
    </div>
  );
}

/** A labeled usage meter row: the metric name, its `used / limit`, and a bar. */
function UsageRow({
  label,
  used,
  limit,
}: {
  label: string;
  used: number | null;
  limit: number | null;
}) {
  return (
    <div>
      <div className="mb-2 flex items-baseline justify-between gap-3">
        <span className="text-sm font-medium text-text-secondary">{label}</span>
        <span className="font-mono text-sm text-text-muted">
          {formatUsage(used, limit)}
        </span>
      </div>
      <UsageMeter used={used} limit={limit} label={`${label} used`} />
    </div>
  );
}

/**
 * The in-card recovery message for the current serving state. Mirrors the
 * global banner's intent but lives next to the actions so the user reads it
 * right before clicking "Manage billing".
 */
function RecoveryNote({
  state,
  trialDays,
  trialEndsAt,
}: {
  state: BillingState;
  trialDays: number | null;
  trialEndsAt: string | null;
}) {
  if (state === 'trialing') {
    return (
      <p className="mt-4 rounded-lg bg-accent-subtle px-3 py-2 text-sm text-accent-dark">
        {trialDays !== null && trialDays > 0
          ? `Your free trial ends in ${trialDays} day${trialDays === 1 ? '' : 's'}`
          : 'Your free trial ends today'}
        {trialEndsAt ? ` — on ${formatDate(trialEndsAt)}.` : '.'} Add billing now
        to keep the paid tier without interruption.
      </p>
    );
  }
  if (state === 'past_due') {
    return (
      <p className="mt-4 rounded-lg bg-amber-50 px-3 py-2 text-sm text-amber-800">
        Your last payment didn’t go through. You still have full access for now —
        update your payment method through “Manage billing” to avoid an
        interruption.
      </p>
    );
  }
  if (state === 'read_only') {
    return (
      <p className="mt-4 rounded-lg bg-amber-50 px-3 py-2 text-sm text-amber-800">
        Your account is read-only because a payment is overdue. Your data is
        safe and fully readable — update billing to restore writes.
      </p>
    );
  }
  if (state === 'suspended') {
    return (
      <p className="mt-4 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-700">
        Serving is paused for non-payment. Your data is retained in full —
        update billing through “Manage billing” to restore access.
      </p>
    );
  }
  return null;
}
