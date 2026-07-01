import { useState } from 'react';
import type { FormEvent } from 'react';
import { Select } from '../ui/Select';
import { Button } from '../ui/Button';
import { Banner } from '../ui/Banner';
import { ApiError, updateModels } from '../../lib/api';
import type { ProviderWriteResult } from '../../lib/api';
import {
  MANAGED_EMBEDDING_MODEL,
  MANAGED_TAGGING_MODEL,
  managedAgenticModels,
} from '../../lib/models';

interface ManagedModelsProps {
  /** The current `llm_model` (agentic model) from the stored managed config. */
  currentLlmModel: string | null;
  /** Whether the account's plan unlocks the premium agentic model list. */
  premium: boolean;
  onSaved: (result: ProviderWriteResult) => void;
}

/**
 * Model selection for the managed provider. Two models are platform-pinned and
 * shown read-only: the embedding model (changing it would invalidate every
 * stored vector) and the tagging model (a cheap single-shot utility). The user
 * chooses only the agentic model — wiki/chat/reports — from the plan's curated
 * list. Submits just the user-writable `llm_model` to
 * `PUT /api/account/provider/models`; the server merges it over the
 * platform-owned config and re-checks it against the account's tier.
 */
export function ManagedModels({ currentLlmModel, premium, onSaved }: ManagedModelsProps) {
  const agenticModels = managedAgenticModels(premium);
  const initial = currentLlmModel ?? agenticModels[0].id;
  const [llmModel, setLlmModel] = useState(initial);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const changed = llmModel !== (currentLlmModel ?? initial);

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!changed || submitting) return;
    setError(null);
    setSubmitting(true);
    try {
      const result = await updateModels({ llm_model: llmModel });
      onSaved(result);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : 'Couldn’t update the model. Try again.');
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <form onSubmit={handleSubmit} className="space-y-5">
      <div className="rounded-xl border border-border-light bg-bg-secondary/40 px-4 py-3">
        <p className="text-sm font-medium text-text-primary">Embedding model</p>
        <p className="mt-0.5 font-mono text-sm text-text-secondary">{MANAGED_EMBEDDING_MODEL}</p>
        <p className="mt-1 text-xs text-text-muted">
          Pinned on the managed plan — changing it would invalidate every stored
          embedding. Bring your own key to choose a different one.
        </p>
      </div>

      <div className="rounded-xl border border-border-light bg-bg-secondary/40 px-4 py-3">
        <p className="text-sm font-medium text-text-primary">Tagging model</p>
        <p className="mt-0.5 font-mono text-sm text-text-secondary">{MANAGED_TAGGING_MODEL}</p>
        <p className="mt-1 text-xs text-text-muted">
          Auto-tagging runs on a fast, low-cost model — fixed on the managed plan.
        </p>
      </div>

      {error && (
        <Banner tone="error" title="Couldn’t update the model">
          {error}
        </Banner>
      )}

      <Select
        label="Agentic model"
        value={llmModel}
        onChange={(e) => setLlmModel(e.target.value)}
        disabled={submitting}
        options={agenticModels.map((m) => ({ value: m.id, label: m.label }))}
        help="Powers wiki synthesis, chat, and reports."
      />

      <Button type="submit" disabled={!changed} loading={submitting}>
        {submitting ? 'Saving…' : 'Save model'}
      </Button>
    </form>
  );
}
