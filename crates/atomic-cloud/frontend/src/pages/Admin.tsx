import { useCallback, useEffect, useMemo, useState } from 'react';
import { PublicLayout } from '../layouts/PublicLayout';
import { NotFound } from './NotFound';
import { Banner } from '../components/ui/Banner';
import { Button } from '../components/ui/Button';
import { Card } from '../components/ui/Card';
import { Spinner } from '../components/ui/Spinner';
import { StatusPill } from '../components/ui/StatusPill';
import {
  adminEvict,
  adminGetAccount,
  adminListAccounts,
  adminListPlans,
  adminSetPlan,
  ApiError,
  type AdminAccount,
  type AdminAccountDetail,
  type AdminPlan,
} from '../lib/api';

/**
 * The operator portal (app host, `/admin`). The server answers 404 to
 * anything but an is_admin session, and this page mirrors that posture:
 * a denied fetch renders the ordinary NotFound page, so the portal is
 * indistinguishable from a dead link to everyone but an admin.
 *
 * The plan picker renders from the `plans` catalogue — a new (comp) tier
 * added by migration appears here with no UI change.
 */
export function Admin() {
  const [state, setState] = useState<
    | { status: 'loading' }
    | { status: 'denied' }
    | { status: 'error'; message: string }
    | { status: 'ready'; accounts: AdminAccount[]; plans: AdminPlan[] }
  >({ status: 'loading' });
  const [notice, setNotice] = useState<{ tone: 'success' | 'warning'; message: string } | null>(
    null,
  );

  const load = useCallback(async () => {
    try {
      const [accounts, plans] = await Promise.all([adminListAccounts(), adminListPlans()]);
      setState({ status: 'ready', accounts, plans });
    } catch (e) {
      if (e instanceof ApiError && (e.status === 404 || e.status === 401)) {
        setState({ status: 'denied' });
      } else {
        setState({ status: 'error', message: String(e) });
      }
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  if (state.status === 'denied') return <NotFound />;

  return (
    <PublicLayout>
      <div className="max-w-5xl mx-auto px-6 py-16">
        <header className="mb-8">
          <p className="text-xs font-medium uppercase tracking-wide text-text-muted">Operator</p>
          <h1 className="mt-1 font-display text-3xl tracking-tight md:text-4xl">
            Accounts<span className="italic">.</span>
          </h1>
        </header>

        {notice && (
          <div className="mb-6">
            <Banner tone={notice.tone} title={notice.tone === 'success' ? 'Done' : 'Heads up'}>
              {notice.message}
            </Banner>
          </div>
        )}

        {state.status === 'loading' && (
          <Card>
            <div className="flex items-center gap-3 text-text-secondary">
              <Spinner className="h-5 w-5 text-accent" />
              <span className="text-sm">Loading accounts…</span>
            </div>
          </Card>
        )}

        {state.status === 'error' && (
          <Banner tone="warning" title="Couldn't load the portal">
            {state.message}
          </Banner>
        )}

        {state.status === 'ready' && (
          <AccountsTable
            accounts={state.accounts}
            plans={state.plans}
            onChanged={(message) => {
              setNotice({ tone: 'success', message });
              void load();
            }}
            onError={(message) => setNotice({ tone: 'warning', message })}
          />
        )}
      </div>
    </PublicLayout>
  );
}

function AccountsTable({
  accounts,
  plans,
  onChanged,
  onError,
}: {
  accounts: AdminAccount[];
  plans: AdminPlan[];
  onChanged: (message: string) => void;
  onError: (message: string) => void;
}) {
  const [openId, setOpenId] = useState<string | null>(null);

  return (
    <div className="space-y-3">
      {accounts.map((account) => (
        <Card key={account.id}>
          <button
            type="button"
            onClick={() => setOpenId(openId === account.id ? null : account.id)}
            className="flex w-full flex-wrap items-center justify-between gap-3 text-left"
          >
            <div>
              <p className="font-medium">
                {account.subdomain}
                {account.is_admin && (
                  <span className="ml-2 text-xs uppercase tracking-wide text-accent">admin</span>
                )}
              </p>
              <p className="text-sm text-text-muted">{account.email}</p>
            </div>
            <div className="flex items-center gap-2">
              <StatusPill tone={account.billing_state === 'active' ? 'success' : 'warning'} dot>
                {account.billing_state}
              </StatusPill>
              <StatusPill tone="neutral">
                {account.plan_id ?? 'no plan'}
                {account.plan_pinned ? ' · pinned' : ''}
              </StatusPill>
            </div>
          </button>
          <dl className="mt-3 grid gap-2 text-xs text-text-muted sm:grid-cols-3">
            <div>Created {new Date(account.created_at).toLocaleDateString()}</div>
            <div>
              {account.last_backup_at
                ? `Backed up ${new Date(account.last_backup_at).toLocaleString()}`
                : 'Never backed up'}
            </div>
            <div>
              {account.trial_ends_at
                ? `Trial ends ${new Date(account.trial_ends_at).toLocaleDateString()}`
                : ''}
            </div>
          </dl>
          {openId === account.id && (
            <AccountActions
              account={account}
              plans={plans}
              onChanged={onChanged}
              onError={onError}
            />
          )}
        </Card>
      ))}
      {accounts.length === 0 && (
        <Card>
          <p className="text-sm text-text-secondary">No accounts yet.</p>
        </Card>
      )}
    </div>
  );
}

function AccountActions({
  account,
  plans,
  onChanged,
  onError,
}: {
  account: AdminAccount;
  plans: AdminPlan[];
  onChanged: (message: string) => void;
  onError: (message: string) => void;
}) {
  const [detail, setDetail] = useState<AdminAccountDetail | null>(null);
  const [planId, setPlanId] = useState(account.plan_id ?? 'free');
  const [pinned, setPinned] = useState(true);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    adminGetAccount(account.id)
      .then((d) => {
        if (!cancelled) setDetail(d);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [account.id]);

  const planLabel = useMemo(() => {
    const fmt = (p: AdminPlan) => {
      const atoms = p.atom_limit === null ? '∞ atoms' : `${p.atom_limit} atoms`;
      const ai = `$${(p.ai_credits_monthly_cents / 100).toFixed(2)}/mo AI`;
      return `${p.name} (${atoms}, ${ai})`;
    };
    return new Map(plans.map((p) => [p.id, fmt(p)]));
  }, [plans]);

  const savePlan = async () => {
    setBusy(true);
    try {
      await adminSetPlan(account.id, planId, pinned);
      onChanged(
        `${account.subdomain} → ${planId}${pinned ? ' (pinned against automated downgrades)' : ''}`,
      );
    } catch (e) {
      onError(e instanceof ApiError ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const evict = async () => {
    setBusy(true);
    try {
      await adminEvict(account.id);
      onChanged(`Evicted ${account.subdomain} from the serving cache.`);
    } catch (e) {
      onError(e instanceof ApiError ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mt-4 space-y-4 border-t border-border-light pt-4">
      <div className="flex flex-wrap items-end gap-3">
        <label className="block">
          <span className="mb-1 block text-xs font-medium text-text-muted">Plan</span>
          <select
            value={planId}
            onChange={(e) => setPlanId(e.target.value)}
            className="rounded-lg border border-border-light bg-bg-primary px-3 py-2 text-sm"
          >
            {plans.map((p) => (
              <option key={p.id} value={p.id}>
                {planLabel.get(p.id)}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-2 pb-2 text-sm text-text-secondary">
          <input
            type="checkbox"
            checked={pinned}
            onChange={(e) => setPinned(e.target.checked)}
          />
          Pin (hold against trial expiry &amp; billing events)
        </label>
        <Button onClick={savePlan} disabled={busy}>
          {busy ? 'Saving…' : 'Set plan'}
        </Button>
        <Button variant="secondary" onClick={evict} disabled={busy}>
          Evict cache
        </Button>
      </div>
      {detail && detail.recent_transitions.length > 0 && (
        <div>
          <p className="mb-1 text-xs font-medium uppercase tracking-wide text-text-muted">
            Recent plan transitions
          </p>
          <ul className="space-y-1 text-xs text-text-secondary">
            {detail.recent_transitions.map((t, i) => (
              <li key={i}>
                {new Date(t.at).toLocaleString()} — {t.from ?? '∅'} → {t.to ?? '∅'} (
                {t.trigger})
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
