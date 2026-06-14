import { useMemo, useState } from 'react';
import type { FormEvent } from 'react';
import { SegmentedControl } from '../ui/SegmentedControl';
import { Field } from '../ui/Field';
import { PasswordField } from '../ui/PasswordField';
import { Button } from '../ui/Button';
import { Banner } from '../ui/Banner';
import { ApiError, saveByokProvider } from '../../lib/api';
import type { ByokModelConfig, ByokProvider, ProviderWriteResult } from '../../lib/api';
import { PINNED_EMBEDDING_DIMENSION } from '../../lib/models';

interface ByokFormProps {
  /** Whether a key is already stored for this BYOK provider (rotation vs first
   * save). Affects copy only — the stored key is never shown either way. */
  hasExistingKey: boolean;
  /** Called with the server's success result (carries any re-embed warning) so
   * the parent can refresh status and surface the warning. */
  onSaved: (result: ProviderWriteResult) => void;
}

/**
 * The bring-your-own-key entry/rotation form. Mirrors the product app's
 * `AIProviderStep` structure — provider choice, key, model config — against the
 * cloud `PUT /api/account/provider` route, re-themed to the website's light
 * palette.
 *
 * Two contracts this form upholds:
 *
 * - **The stored key is never shown.** This form only ever holds a *new* key
 *   the user is typing; it has no field that could render an existing secret.
 *   Rotation is just submitting a new key.
 * - **Validation is server-side.** The key is verified against the provider
 *   before anything is stored; on failure the server's message is surfaced
 *   verbatim and nothing is saved. Submit is disabled until the form is
 *   minimally valid (a non-empty key, and a base URL for OpenAI-compatible),
 *   so the obvious mistakes never round-trip.
 */
export function ByokForm({ hasExistingKey, onSaved }: ByokFormProps) {
  const [provider, setProvider] = useState<ByokProvider>('openrouter');
  const [apiKey, setApiKey] = useState('');
  const [embeddingModel, setEmbeddingModel] = useState('');
  const [llmModel, setLlmModel] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const keyOk = apiKey.trim().length > 0;
  // OpenAI-compatible needs a base URL to function (the server errors without
  // one); OpenRouter has a sensible default.
  const baseUrlOk = provider === 'openrouter' || baseUrl.trim().length > 0;
  const valid = keyOk && baseUrlOk && !submitting;

  const modelConfig = useMemo<ByokModelConfig>(() => {
    const config: ByokModelConfig = {};
    if (embeddingModel.trim()) config.embedding_model = embeddingModel.trim();
    if (llmModel.trim()) config.llm_model = llmModel.trim();
    if (provider === 'openai_compat' && baseUrl.trim()) {
      config.openai_compat_base_url = baseUrl.trim();
    }
    if (provider === 'openrouter' && baseUrl.trim()) {
      config.openrouter_base_url = baseUrl.trim();
    }
    return config;
  }, [embeddingModel, llmModel, baseUrl, provider]);

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!valid) return;
    setError(null);
    setSubmitting(true);
    try {
      const result = await saveByokProvider({
        provider,
        api_key: apiKey.trim(),
        model_config: modelConfig,
      });
      // Clear the typed key from memory on success.
      setApiKey('');
      onSaved(result);
    } catch (err) {
      if (err instanceof ApiError) {
        // The provider's validation error is surfaced verbatim (the server
        // scrubs the key from it before sending); a dimension mismatch carries
        // its own structured message.
        setError(err.message);
      } else {
        setError('Something went wrong saving your provider. Please try again.');
      }
    } finally {
      // Always re-enable the form. The parent reloads status in place (it bumps
      // a nonce — this same instance stays mounted), so without this the button
      // would be stuck spinning and every field disabled after a *successful*
      // save. The parent surfaces the "Saved" affordance; here we just return
      // to an idle, usable state.
      setSubmitting(false);
    }
  }

  return (
    <form onSubmit={handleSubmit} noValidate className="space-y-5">
      <SegmentedControl<ByokProvider>
        label="Provider"
        value={provider}
        onChange={(v) => {
          setProvider(v);
          setError(null);
        }}
        disabled={submitting}
        segments={[
          {
            value: 'openrouter',
            label: 'OpenRouter',
            description: 'One key, hundreds of models. The simplest option.',
          },
          {
            value: 'openai_compat',
            label: 'OpenAI-compatible',
            description: 'Any OpenAI-style endpoint — local models, gateways.',
          },
        ]}
      />

      {error && (
        <Banner tone="error" title="Couldn’t save your provider">
          {error}
        </Banner>
      )}

      {provider === 'openai_compat' && (
        <Field
          label="Base URL"
          type="url"
          inputMode="url"
          placeholder="https://your-endpoint.example/v1"
          value={baseUrl}
          onChange={(e) => setBaseUrl(e.target.value)}
          disabled={submitting}
          required
          help="The OpenAI-compatible API endpoint your key authenticates against."
        />
      )}

      <PasswordField
        label={hasExistingKey ? 'New API key' : 'API key'}
        placeholder={provider === 'openrouter' ? 'sk-or-…' : 'sk-…'}
        value={apiKey}
        onChange={(e) => {
          setApiKey(e.target.value);
          if (error) setError(null);
        }}
        disabled={submitting}
        required
        help={
          hasExistingKey
            ? 'Entering a new key replaces the stored one. The current key is never shown.'
            : provider === 'openrouter'
              ? 'Get a key at openrouter.ai/keys. Validated before it’s stored.'
              : 'Validated against your endpoint before it’s stored.'
        }
      />

      <div className="rounded-xl border border-border-light bg-bg-secondary/50 p-4">
        <p className="text-sm font-medium text-text-primary">Models (optional)</p>
        <p className="mt-0.5 text-xs text-text-muted">
          Leave blank to use the provider’s defaults. The embedding model must
          produce {PINNED_EMBEDDING_DIMENSION}-dimensional vectors — a different
          dimension is rejected.
        </p>
        <div className="mt-4 grid gap-4 sm:grid-cols-2">
          <Field
            label="Embedding model"
            placeholder={
              provider === 'openrouter' ? 'openai/text-embedding-3-small' : 'text-embedding-3-small'
            }
            value={embeddingModel}
            onChange={(e) => setEmbeddingModel(e.target.value)}
            disabled={submitting}
          />
          <Field
            label="LLM model"
            placeholder={provider === 'openrouter' ? 'openai/gpt-4o-mini' : 'your-llm'}
            value={llmModel}
            onChange={(e) => setLlmModel(e.target.value)}
            disabled={submitting}
            help="Used for tagging, wiki synthesis, and chat."
          />
        </div>
      </div>

      <div className="flex items-center gap-3">
        <Button type="submit" disabled={!valid} loading={submitting}>
          {submitting ? 'Validating…' : hasExistingKey ? 'Replace key' : 'Save & validate'}
        </Button>
        <p className="text-sm text-text-muted">
          We’ll verify the key with the provider before storing it.
        </p>
      </div>
    </form>
  );
}
