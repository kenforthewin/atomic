import { Link } from 'react-router-dom';
import {
  ArrowRight,
  Cpu,
  CreditCard,
  Database,
  FileText,
  Sparkles,
} from 'lucide-react';
import { useAccount } from '../../lib/accountContext';
import { Card } from '../../components/ui/Card';
import { StatusPill } from '../../components/ui/StatusPill';
import { UsageMeter } from '../../components/account/UsageMeter';
import { formatCents, formatUsage } from '../../lib/format';
import { billingDescriptor } from '../../lib/billing';
import { originLabel, providerLabel } from '../../lib/provider';

/**
 * The dashboard landing view: the account's plan and status, live resource
 * usage (atoms, knowledge bases, AI allowance), the AI-provider summary, and
 * quick links onward. Reads the overview the shell already loaded — no fetch of
 * its own — so it renders instantly with real states everywhere.
 */
export function Overview() {
  const { overview } = useAccount();
  const { plan, usage, provider, email, subdomain } = overview;
  const billing = billingDescriptor(overview.billing_state);

  return (
    <div className="space-y-8">
      <header>
        <p className="text-xs font-medium uppercase tracking-wide text-text-muted">Overview</p>
        <h1 className="mt-1 font-display text-3xl tracking-tight md:text-4xl">
          Your <span className="italic">workspace.</span>
        </h1>
        <p className="mt-2 text-text-secondary">
          {subdomain} · {email}
        </p>
      </header>

      {/* Plan + status */}
      <Card>
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div>
            <p className="text-sm text-text-muted">Current plan</p>
            <p className="mt-1 font-display text-2xl tracking-tight">{plan.name}</p>
          </div>
          <StatusPill tone={billing.tone} dot>
            {billing.label}
          </StatusPill>
        </div>
      </Card>

      {/* Usage grid */}
      <section aria-labelledby="usage-heading" className="space-y-4">
        <h2 id="usage-heading" className="font-medium text-lg">
          Usage
        </h2>
        <div className="grid gap-6 sm:grid-cols-2">
          <Card>
            <div className="mb-3 flex items-center gap-2 text-text-secondary">
              <FileText className="h-4 w-4 text-accent" strokeWidth={1.75} aria-hidden="true" />
              <span className="text-sm font-medium">Atoms</span>
            </div>
            <p className="font-display text-2xl tracking-tight">
              {formatUsage(usage.atoms_used, usage.atom_limit)}
            </p>
            <div className="mt-3">
              <UsageMeter used={usage.atoms_used} limit={usage.atom_limit} label="Atoms used" />
            </div>
          </Card>

          <Card>
            <div className="mb-3 flex items-center gap-2 text-text-secondary">
              <Database className="h-4 w-4 text-accent" strokeWidth={1.75} aria-hidden="true" />
              <span className="text-sm font-medium">Knowledge bases</span>
            </div>
            <p className="font-display text-2xl tracking-tight">
              {formatUsage(usage.kb_count, usage.kb_limit)}
            </p>
            <div className="mt-3">
              <UsageMeter
                used={usage.kb_count}
                limit={usage.kb_limit}
                label="Knowledge bases used"
              />
            </div>
          </Card>
        </div>

        {/* AI allowance — only meaningful for the managed provider. */}
        {provider?.origin === 'managed' && (
          <Card>
            <div className="mb-1 flex items-center gap-2 text-text-secondary">
              <Sparkles className="h-4 w-4 text-accent" strokeWidth={1.75} aria-hidden="true" />
              <span className="text-sm font-medium">Monthly AI allowance</span>
            </div>
            <p className="font-display text-2xl tracking-tight">
              {formatCents(usage.ai_credits_monthly_cents)}
            </p>
            <p className="mt-1 text-sm text-text-muted">
              Your plan’s managed-AI budget, refreshed monthly. Detailed usage
              lives on the{' '}
              <Link to="/account/provider" className="text-accent hover:text-accent-dark">
                provider page
              </Link>
              .
            </p>
          </Card>
        )}
      </section>

      {/* Provider summary */}
      <section aria-labelledby="provider-heading" className="space-y-4">
        <h2 id="provider-heading" className="font-medium text-lg">
          AI provider
        </h2>
        <Card interactive className="block">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-3">
              <span className="flex h-10 w-10 items-center justify-center rounded-lg bg-accent-subtle text-accent">
                <Cpu className="h-5 w-5" strokeWidth={1.5} aria-hidden="true" />
              </span>
              <div>
                <p className="font-medium">
                  {provider?.configured ? providerLabel(provider.provider) : 'No provider yet'}
                </p>
                <p className="text-sm text-text-muted">
                  {provider?.configured
                    ? originLabel(provider.origin)
                    : 'Connect a provider to enable embeddings, tagging, and chat.'}
                </p>
              </div>
            </div>
            <Link
              to="/account/provider"
              className="inline-flex items-center gap-1.5 text-sm font-medium text-accent transition-colors hover:text-accent-dark focus-visible:outline-2"
            >
              {provider?.configured ? 'Manage' : 'Set up'}
              <ArrowRight className="h-4 w-4" aria-hidden="true" />
            </Link>
          </div>
          {provider?.last_validation_error && (
            <p className="mt-3 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-700">
              Last validation failed: {provider.last_validation_error}
            </p>
          )}
        </Card>
      </section>

      {/* Quick links */}
      <section aria-labelledby="links-heading" className="space-y-4">
        <h2 id="links-heading" className="font-medium text-lg">
          Manage
        </h2>
        <div className="grid gap-4 sm:grid-cols-2">
          <QuickLink
            to="/account/billing"
            Icon={CreditCard}
            title="Billing"
            body="Plan, invoices, and payment — managed through Stripe."
          />
          <QuickLink
            to="/account/mcp"
            Icon={Cpu}
            title="MCP"
            body="Connect Claude and other MCP clients to your knowledge base."
          />
        </div>
      </section>
    </div>
  );
}

function QuickLink({
  to,
  Icon,
  title,
  body,
}: {
  to: string;
  Icon: typeof CreditCard;
  title: string;
  body: string;
}) {
  return (
    <Link
      to={to}
      className="group block rounded-xl border border-border-light bg-bg-white p-5 transition-all hover:border-accent/20 hover:shadow-md focus-visible:outline-2"
    >
      <div className="flex items-start gap-3">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-accent-subtle text-accent">
          <Icon className="h-4 w-4" strokeWidth={1.5} aria-hidden="true" />
        </span>
        <div className="min-w-0">
          <p className="font-medium">{title}</p>
          <p className="mt-0.5 text-sm text-text-secondary leading-relaxed">{body}</p>
        </div>
        <ArrowRight className="ml-auto h-4 w-4 shrink-0 text-text-muted transition-transform group-hover:translate-x-0.5" aria-hidden="true" />
      </div>
    </Link>
  );
}
