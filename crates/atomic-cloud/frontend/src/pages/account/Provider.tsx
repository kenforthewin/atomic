import { useState } from 'react';
import { Cpu, ShieldCheck } from 'lucide-react';
import { useAccount } from '../../lib/accountContext';
import { useProviderStatus } from '../../lib/useProviderStatus';
import { Card } from '../../components/ui/Card';
import { Banner } from '../../components/ui/Banner';
import { Button } from '../../components/ui/Button';
import { Spinner } from '../../components/ui/Spinner';
import { StatusPill } from '../../components/ui/StatusPill';
import { ByokForm } from '../../components/account/ByokForm';
import { ManagedModels } from '../../components/account/ManagedModels';
import { ApiError, activateProvider } from '../../lib/api';
import type { ProviderStatus, ProviderWriteResult } from '../../lib/api';
import { formatRelative, formatUsd } from '../../lib/format';
import { originLabel, providerLabel, readModelConfig } from '../../lib/provider';

/**
 * AI provider settings: show the active provider's status (managed vs BYOK,
 * configured, last validated — never the key), and let the user switch the
 * active credential, tune the managed model, or bring their own key.
 *
 * Reads the full provider status (its own fetch, since the overview carries
 * only a summary); a successful write refreshes both the status here and the
 * shell's overview (so the summary card and usage stay in sync).
 */
export function Provider() {
  const { overview, reload: reloadOverview } = useAccount();
  const { state, reload: reloadStatus } = useProviderStatus();
  // Paid plans unlock the premium agentic model list. The server is
  // authoritative (it re-checks the plan's `premium_models` flag on write);
  // this only picks which options the managed picker offers.
  const isPremiumPlan = overview.plan.id !== 'free';
  const [notice, setNotice] = useState<{ tone: 'success' | 'warning'; message: string } | null>(
    null,
  );

  function onWriteSucceeded(result: ProviderWriteResult, fallback: string) {
    // A re-embed warning is the loud case (an embedding-model change); a plain
    // success otherwise.
    if (result.reembed_warning) {
      setNotice({ tone: 'warning', message: result.reembed_warning });
    } else {
      setNotice({ tone: 'success', message: fallback });
    }
    reloadStatus();
    reloadOverview();
  }

  return (
    <div className="space-y-8">
      <header>
        <p className="text-xs font-medium uppercase tracking-wide text-text-muted">AI provider</p>
        <h1 className="mt-1 font-display text-3xl tracking-tight md:text-4xl">
          Power your <span className="italic">AI.</span>
        </h1>
        <p className="mt-2 text-text-secondary">
          Atomic uses an AI provider for embeddings, tag extraction, wiki
          synthesis, and chat. Use the managed provider, or bring your own key.
        </p>
      </header>

      {notice && (
        <Banner
          tone={notice.tone}
          title={notice.tone === 'success' ? 'Saved' : 'Heads up — re-embedding required'}
        >
          {notice.message}
        </Banner>
      )}

      {state.status === 'loading' && (
        <Card>
          <div className="flex items-center gap-3 text-text-secondary">
            <Spinner className="h-5 w-5 text-accent" />
            <span className="text-sm">Loading your provider settings…</span>
          </div>
        </Card>
      )}

      {state.status === 'error' && (
        <Banner
          tone="error"
          title="Couldn’t load your provider"
          action={
            <Button size="sm" variant="secondary" onClick={reloadStatus}>
              Retry
            </Button>
          }
        >
          {state.message}
        </Banner>
      )}

      {state.status === 'ready' && (
        <ProviderBody
          provider={state.provider}
          premiumPlan={isPremiumPlan}
          onWriteSucceeded={onWriteSucceeded}
          onActivateError={(message) => setNotice({ tone: 'warning', message })}
          reloadStatus={reloadStatus}
          reloadOverview={reloadOverview}
        />
      )}
    </div>
  );
}

function ProviderBody({
  provider,
  premiumPlan,
  onWriteSucceeded,
  onActivateError,
  reloadStatus,
  reloadOverview,
}: {
  provider: ProviderStatus;
  premiumPlan: boolean;
  onWriteSucceeded: (result: ProviderWriteResult, fallback: string) => void;
  onActivateError: (message: string) => void;
  reloadStatus: () => void;
  reloadOverview: () => void;
}) {
  const isManaged = provider.origin === 'managed';
  const isByok = provider.origin === 'user';
  const config = readModelConfig(provider.model_config);

  return (
    <>
      {/* Current status */}
      <Card>
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="flex items-start gap-3">
            <span className="flex h-10 w-10 items-center justify-center rounded-lg bg-accent-subtle text-accent">
              <Cpu className="h-5 w-5" strokeWidth={1.5} aria-hidden="true" />
            </span>
            <div>
              <p className="font-medium">
                {provider.configured ? providerLabel(provider.provider) : 'No provider configured'}
              </p>
              <p className="text-sm text-text-muted">{originLabel(provider.origin)}</p>
            </div>
          </div>
          {provider.configured ? (
            <StatusPill tone="success" dot>
              Active
            </StatusPill>
          ) : (
            <StatusPill tone="warning" dot>
              Not set up
            </StatusPill>
          )}
        </div>

        {provider.configured && (
          <dl className="mt-5 grid gap-4 border-t border-border-light pt-5 text-sm sm:grid-cols-2">
            {config.llm_model && (
              <Detail label="LLM model" value={config.llm_model} mono />
            )}
            {config.embedding_model && (
              <Detail label="Embedding model" value={config.embedding_model} mono />
            )}
            <Detail
              label="Last validated"
              value={formatRelative(provider.last_validated_at)}
            />
            <Detail label="Last used" value={formatRelative(provider.last_used_at)} />
          </dl>
        )}

        {provider.last_validation_error && (
          <p className="mt-4 rounded-lg bg-red-50 px-3 py-2 text-sm text-red-700">
            Last validation failed: {provider.last_validation_error}
          </p>
        )}

        {/* Managed allowance/usage */}
        {isManaged && provider.usage && (
          <div className="mt-5 rounded-xl border border-border-light bg-bg-secondary/40 p-4">
            <div className="flex items-center gap-2 text-text-secondary">
              <ShieldCheck className="h-4 w-4 text-accent" strokeWidth={1.75} aria-hidden="true" />
              <span className="text-sm font-medium">Managed AI allowance</span>
            </div>
            <p className="mt-2 text-sm text-text-secondary">
              Used <span className="font-medium text-text-primary">{formatUsd(provider.usage.usage_usd)}</span>
              {provider.usage.limit_usd !== null && (
                <> of {formatUsd(provider.usage.limit_usd)} this period</>
              )}
              {provider.usage.limit_remaining_usd !== null && (
                <> · {formatUsd(provider.usage.limit_remaining_usd)} remaining</>
              )}
              .
            </p>
            {provider.usage.disabled && (
              <p className="mt-2 text-sm text-amber-700">
                Your managed key is paused — you may be out of allowance for this
                period.
              </p>
            )}
          </div>
        )}
      </Card>

      {/* Switch active credential (only when both rows plausibly exist) */}
      <ActivateSwitch
        activeOrigin={provider.origin}
        onActivated={(result) => onWriteSucceeded(result, 'Switched your active provider.')}
        onError={onActivateError}
        reloadStatus={reloadStatus}
        reloadOverview={reloadOverview}
      />

      {/* Managed model selection */}
      {isManaged && (
        <Card>
          <h2 className="font-medium text-lg">Managed models</h2>
          <p className="mt-1 text-sm text-text-secondary">
            Choose the model that powers tagging, wikis, and chat. The embedding
            model is fixed on the managed plan.
          </p>
          <div className="mt-5">
            <ManagedModels
              currentLlmModel={config.llm_model ?? null}
              premium={premiumPlan}
              onSaved={(result) => onWriteSucceeded(result, 'Updated your managed model.')}
            />
          </div>
        </Card>
      )}

      {/* Bring your own key */}
      <Card>
        <h2 className="font-medium text-lg">
          {isByok ? 'Your API key' : 'Bring your own key'}
        </h2>
        <p className="mt-1 text-sm text-text-secondary">
          {isByok
            ? 'Rotate your key or change models. Your current key is never shown — entering a new one replaces it.'
            : 'Use your own OpenRouter or OpenAI-compatible key. It’s validated before it’s stored, and encrypted at rest.'}
        </p>
        <div className="mt-5">
          <ByokForm
            hasExistingKey={isByok}
            onSaved={(result) =>
              onWriteSucceeded(
                result,
                isByok ? 'Your provider key was replaced.' : 'Your provider key was saved.',
              )
            }
          />
        </div>
      </Card>
    </>
  );
}

/**
 * Switch back to the managed provider from BYOK. Only this direction is
 * offered: managed is always OpenRouter, so its `(provider, origin)` is
 * unambiguous — whereas the status route reports only the *active* row, so a
 * BYOK row's provider is unknown while managed is active. Switching *to* a
 * stored BYOK key is therefore done by re-saving it below (a save activates
 * it). Shown only when a managed key is available to fall back to; a missing
 * managed row 404s cleanly and is surfaced as a friendly note.
 */
function ActivateSwitch({
  activeOrigin,
  onActivated,
  onError,
  reloadStatus,
  reloadOverview,
}: {
  activeOrigin: 'managed' | 'user' | null;
  onActivated: (result: ProviderWriteResult) => void;
  onError: (message: string) => void;
  reloadStatus: () => void;
  reloadOverview: () => void;
}) {
  const [switching, setSwitching] = useState(false);
  // Only offered from BYOK → managed (the unambiguous direction).
  if (activeOrigin !== 'user') return null;

  async function handleSwitch() {
    setSwitching(true);
    try {
      // Managed is always OpenRouter; the server 404s if no managed row exists.
      const result = await activateProvider({ provider: 'openrouter', origin: 'managed' });
      onActivated(result);
      reloadStatus();
      reloadOverview();
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) {
        onError('No managed provider is available for this account.');
      } else {
        onError(err instanceof ApiError ? err.message : 'Couldn’t switch providers.');
      }
    } finally {
      setSwitching(false);
    }
  }

  return (
    <Card>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="font-medium">Switch to the managed provider</h2>
          <p className="mt-0.5 text-sm text-text-muted">
            You’re using your own key. Switch back to Atomic’s managed provider
            and its included AI allowance.
          </p>
        </div>
        <Button variant="secondary" size="sm" onClick={handleSwitch} loading={switching}>
          Use managed provider
        </Button>
      </div>
    </Card>
  );
}

function Detail({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) {
  return (
    <div>
      <dt className="text-text-muted">{label}</dt>
      <dd className={mono ? 'mt-0.5 font-mono text-text-primary break-all' : 'mt-0.5 text-text-primary'}>
        {value}
      </dd>
    </div>
  );
}
