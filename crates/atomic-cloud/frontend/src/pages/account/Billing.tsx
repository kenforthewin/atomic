import { useState } from 'react';
import { CreditCard, ExternalLink } from 'lucide-react';
import { useSearchParams } from 'react-router-dom';
import { useAccount } from '../../lib/accountContext';
import { Card } from '../../components/ui/Card';
import { StatusPill } from '../../components/ui/StatusPill';
import { Banner } from '../../components/ui/Banner';
import { Spinner } from '../../components/ui/Spinner';
import { UsageMeter } from '../../components/account/UsageMeter';
import type { BillingState } from '../../lib/api';
import { billingDescriptor, BillingFlowError, startBillingFlow } from '../../lib/billing';
import { daysUntil, formatCents, formatDate, formatUsage } from '../../lib/format';

/**
 * Billing = Stripe portal + status. We render the plan, the serving state, and
 * the trial/dunning context, then hand off to Stripe for everything money:
 * "Manage billing" opens the portal route (a server 302 into the Customer
 * Portal), and the upgrade CTA opens the checkout route. No card entry, invoice
 * tables, or Stripe Elements live here — Stripe owns that.
 *
 * The portal/checkout routes are `GET` redirects to Stripe, but a *misconfig*
 * (no price mapped for a plan, no customer yet, an upstream error) makes them
 * answer with JSON instead. Navigating the browser straight onto the route
 * would then paint a raw JSON error page, so both actions go through
 * {@link startBillingFlow}: it probes the route (`redirect: 'manual'`), follows
 * the real Stripe redirect with a top-level navigation, and on a JSON error
 * surfaces an in-app banner instead. The full-page redirect to Stripe is still
 * exactly the intended flow.
 *
 * When Stripe isn't configured on the deployment (`billing_configured: false`),
 * those routes 503 `billing_not_configured`; we disable the actions up front and
 * explain, so the flow isn't even attempted.
 */
export function Billing() {
  const { overview } = useAccount();
  const [params] = useSearchParams();
  const { plan, usage, billing_configured: configured } = overview;
  const status = billingDescriptor(overview.billing_state);
  const trialDays = daysUntil(overview.trial_ends_at);
  // Which flow (if any) is in flight, plus any in-app error from a misconfig —
  // so a bad checkout/portal lands as a banner here, not a raw JSON page.
  const [pending, setPending] = useState<'checkout' | 'portal' | null>(null);
  const [flowError, setFlowError] = useState<string | null>(null);

  async function openBillingFlow(kind: 'checkout' | 'portal', path: string) {
    setFlowError(null);
    setPending(kind);
    try {
      // Resolves only into a navigation away; a rejection means a misconfig we
      // surface in-app rather than letting the browser land on raw JSON.
      await startBillingFlow(path);
    } catch (err) {
      setFlowError(
        err instanceof BillingFlowError
          ? err.message
          : 'We couldn’t start the billing session. Please try again in a moment.',
      );
      setPending(null);
    }
  }
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

        {flowError && (
          <Banner tone="error" title="Couldn’t open billing" className="mt-6">
            {flowError}
          </Banner>
        )}

        <div className="mt-6 flex flex-wrap gap-3">
          {canUpgrade && configured && (
            <button
              type="button"
              onClick={() => openBillingFlow('checkout', '/api/billing/checkout?plan=pro')}
              disabled={pending !== null}
              aria-busy={pending === 'checkout' || undefined}
              className="group inline-flex items-center justify-center gap-2.5 rounded-xl bg-accent px-7 py-3.5 text-base font-medium text-white transition-all hover:bg-accent-dark hover:shadow-lg hover:shadow-accent/20 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {pending === 'checkout' ? (
                <Spinner className="h-5 w-5" label="Starting checkout" />
              ) : (
                <ExternalLink className="h-4 w-4 opacity-80" aria-hidden="true" />
              )}
              Upgrade to Pro
            </button>
          )}
          {configured && (
            <button
              type="button"
              onClick={() => openBillingFlow('portal', '/api/billing/portal')}
              disabled={pending !== null}
              aria-busy={pending === 'portal' || undefined}
              className="inline-flex items-center justify-center gap-2.5 rounded-xl border border-border bg-bg-white px-7 py-3.5 text-base font-medium text-text-primary transition-all hover:border-accent/30 hover:bg-accent-subtle/50 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {pending === 'portal' ? (
                <Spinner className="h-5 w-5 text-accent" label="Opening billing" />
              ) : (
                <CreditCard className="h-5 w-5" strokeWidth={1.5} aria-hidden="true" />
              )}
              Manage billing
              <ExternalLink className="h-4 w-4 text-text-muted" aria-hidden="true" />
            </button>
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
