import { Outlet } from 'react-router-dom';
import type { AccountContext } from '../../lib/accountContext';
import { useOverview } from '../../lib/useOverview';
import { AccountTopbar } from '../../components/account/AccountTopbar';
import { AccountNav } from '../../components/account/AccountNav';
import { BillingBanner } from '../../components/account/BillingBanner';
import {
  DashboardError,
  DashboardLoading,
  HoldScreen,
} from '../../components/account/HoldScreen';
import { Button } from '../../components/ui/Button';

/**
 * The authenticated tenant dashboard shell. Loads the account overview on
 * mount and routes the structured cloud states into branded full-screen
 * frames:
 *
 * - **loading** → a branded spinner frame;
 * - **provisioning / upgrading** → a friendly "setting up / upgrading" hold
 *   (the hook auto-retries);
 * - **suspended** → a blocking notice with the billing upgrade link;
 * - **error** → a retryable error frame;
 * - **ready** → the full chrome (top bar, nav, the billing banner) wrapping
 *   the active section via `<Outlet>`.
 *
 * A `401` never lands here — the API client redirects an expired session to
 * the app-host login.
 */
export function AccountShell() {
  const { state, reload } = useOverview();

  if (state.status === 'loading') {
    return <DashboardLoading />;
  }

  if (state.status === 'provisioning') {
    return (
      <HoldScreen busy title="Setting up your account…">
        <p>
          We’re provisioning your private workspace. This page will refresh on
          its own the moment it’s ready.
        </p>
      </HoldScreen>
    );
  }

  if (state.status === 'upgrading') {
    return (
      <HoldScreen busy title="Upgrading your account…">
        <p>
          Your workspace is being upgraded to the latest version. It’ll be back
          in a moment — this page refreshes automatically.
        </p>
      </HoldScreen>
    );
  }

  if (state.status === 'suspended') {
    // The server usually hands us an `upgrade_url` straight into Stripe. When
    // it doesn't (billing not configured, or the field was omitted), fall back
    // to the in-dashboard billing page rather than dead-ending the screen — it
    // loads under the suspended state too and offers the manage/upgrade actions.
    return (
      <HoldScreen
        title="Your account is suspended."
        action={
          <Button
            onClick={() =>
              window.location.assign(state.upgradeUrl ?? '/account/billing')
            }
          >
            Update billing
          </Button>
        }
      >
        <p>
          Serving is paused for non-payment. Your data is retained in full —
          update your billing to restore access.
        </p>
      </HoldScreen>
    );
  }

  if (state.status === 'error') {
    return <DashboardError message={state.message} onRetry={reload} />;
  }

  const { overview } = state;
  const context: AccountContext = { overview, reload };

  return (
    <div className="min-h-dvh bg-bg-primary text-text-primary">
      <AccountTopbar subdomain={overview.subdomain} />
      <main className="mx-auto max-w-6xl px-6 pt-24 pb-20">
        <div className="mb-6">
          <BillingBanner
            billingState={overview.billing_state}
            trialEndsAt={overview.trial_ends_at}
            billingConfigured={overview.billing_configured}
          />
        </div>
        <div className="grid gap-8 lg:grid-cols-[200px_minmax(0,1fr)]">
          <AccountNav />
          <div className="min-w-0">
            <Outlet context={context} />
          </div>
        </div>
      </main>
    </div>
  );
}
