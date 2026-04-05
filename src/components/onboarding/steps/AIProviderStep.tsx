import { useEffect, useCallback, useRef, useState } from 'react';
import { Button } from '../../ui/Button';
import { CustomSelect } from '../../ui/CustomSelect';
import { SearchableSelect } from '../../ui/SearchableSelect';
import { ConnectionStatus } from '../../ui/ConnectionStatus';
import { useSettingsStore } from '../../../stores/settings';
import {
  getAvailableLlmModels,
  getOpenRouterEmbeddingModels,
  testOllamaConnection,
  testOpenAICompatConnection,
  getOllamaModels,
  type AvailableModel,
  type OpenRouterEmbeddingModel,
} from '../../../lib/api';
import { isDesktopApp } from '../../../lib/transport';
import { generatePKCE, openOAuthPopup, exchangeCodeForKey } from '../../../lib/openrouter-oauth';
import type { OnboardingState, OnboardingAction } from '../useOnboardingState';

function normalizeUrl(url: string): string {
  const trimmed = url.trim();
  if (!trimmed) return trimmed;
  if (!/^https?:\/\//i.test(trimmed)) return `http://${trimmed}`;
  return trimmed;
}

interface AIProviderStepProps {
  state: OnboardingState;
  dispatch: React.Dispatch<OnboardingAction>;
}

export function AIProviderStep({ state, dispatch }: AIProviderStepProps) {
  const testOpenRouterConnection = useSettingsStore(s => s.testOpenRouterConnection);
  const isDesktop = isDesktopApp();
  const [oauthLoading, setOauthLoading] = useState(false);
  const [oauthError, setOauthError] = useState<string | null>(null);
  const codeVerifierRef = useRef<string | null>(null);
  const codeChallengeMethodRef = useRef<'S256' | 'plain'>('S256');
  const [openrouterEmbeddingModels, setOpenrouterEmbeddingModels] = useState<OpenRouterEmbeddingModel[]>([]);

  // Listen for OAuth callback message from popup
  useEffect(() => {
    if (state.provider !== 'openrouter' || isDesktop) return;

    const handler = async (event: MessageEvent) => {
      if (event.origin !== window.location.origin) return;
      if (event.data?.type !== 'openrouter-oauth-callback') return;

      const code = event.data.code;
      const verifier = codeVerifierRef.current;
      if (!code || !verifier) {
        setOauthError('OAuth flow failed: missing code or verifier');
        setOauthLoading(false);
        return;
      }

      try {
        const key = await exchangeCodeForKey(code, verifier, codeChallengeMethodRef.current);
        dispatch({ type: 'SET_API_KEY', value: key });
        // Auto-test the new key
        dispatch({ type: 'SET_TESTING', value: true });
        try {
          await testOpenRouterConnection(key);
          dispatch({ type: 'SET_TEST_RESULT', result: 'success' });
        } catch (e) {
          dispatch({ type: 'SET_TEST_RESULT', result: 'error', error: String(e) });
        }
        dispatch({ type: 'SET_TESTING', value: false });
      } catch (e) {
        setOauthError(e instanceof Error ? e.message : String(e));
      } finally {
        setOauthLoading(false);
        codeVerifierRef.current = null;
      }
    };

    window.addEventListener('message', handler);
    return () => window.removeEventListener('message', handler);
  }, [state.provider, isDesktop, dispatch, testOpenRouterConnection]);

  const handleOAuthSignIn = async () => {
    setOauthError(null);
    setOauthLoading(true);
    try {
      const { codeVerifier, codeChallenge, codeChallengeMethod } = await generatePKCE();
      codeVerifierRef.current = codeVerifier;
      codeChallengeMethodRef.current = codeChallengeMethod;
      const popup = openOAuthPopup(codeChallenge, codeChallengeMethod);
      if (!popup) {
        setOauthError('Popup blocked. Please allow popups for this site.');
        setOauthLoading(false);
        codeVerifierRef.current = null;
      }
    } catch (e) {
      setOauthError(e instanceof Error ? e.message : String(e));
      setOauthLoading(false);
      codeVerifierRef.current = null;
    }
  };

  // Load OpenRouter models when provider is openrouter and we have a key
  useEffect(() => {
    if (state.provider === 'openrouter' && state.testResult === 'success' && state.availableModels.length === 0) {
      dispatch({ type: 'SET_LOADING_MODELS', value: true });
      getAvailableLlmModels()
        .then(models => dispatch({ type: 'SET_AVAILABLE_MODELS', models }))
        .catch(err => console.error('Failed to load models:', err))
        .finally(() => dispatch({ type: 'SET_LOADING_MODELS', value: false }));
    }
  }, [state.provider, state.testResult, state.availableModels.length, dispatch]);

  // Load curated OpenRouter embedding model registry
  useEffect(() => {
    if (state.provider === 'openrouter' && state.testResult === 'success' && openrouterEmbeddingModels.length === 0) {
      getOpenRouterEmbeddingModels()
        .then(models => setOpenrouterEmbeddingModels(models))
        .catch(err => console.error('Failed to load embedding models:', err));
    }
  }, [state.provider, state.testResult, openrouterEmbeddingModels.length]);

  // Check Ollama connection when provider is ollama
  const checkOllamaConnection = useCallback(async (host: string) => {
    dispatch({ type: 'SET_OLLAMA_STATUS', status: 'checking' });
    try {
      const connected = await testOllamaConnection(host);
      if (connected) {
        dispatch({ type: 'SET_OLLAMA_STATUS', status: 'connected' });
        dispatch({ type: 'SET_LOADING_OLLAMA_MODELS', value: true });
        const models = await getOllamaModels(host);
        dispatch({ type: 'SET_OLLAMA_MODELS', models });
        dispatch({ type: 'SET_LOADING_OLLAMA_MODELS', value: false });
      } else {
        dispatch({ type: 'SET_OLLAMA_STATUS', status: 'disconnected', error: 'Could not connect to Ollama' });
      }
    } catch (e) {
      dispatch({ type: 'SET_OLLAMA_STATUS', status: 'disconnected', error: String(e) });
      dispatch({ type: 'SET_LOADING_OLLAMA_MODELS', value: false });
    }
  }, [dispatch]);

  useEffect(() => {
    if (state.provider !== 'ollama') return;
    const timer = setTimeout(() => {
      checkOllamaConnection(normalizeUrl(state.ollamaHost));
    }, 800);
    return () => clearTimeout(timer);
  }, [state.provider, state.ollamaHost, checkOllamaConnection]);

  // Check OpenAI Compatible connection
  const checkOpenaiCompatConnection = useCallback(async (baseUrl: string, apiKey?: string) => {
    if (!baseUrl.trim()) return;
    dispatch({ type: 'SET_OPENAI_COMPAT_STATUS', status: 'checking' });
    try {
      await testOpenAICompatConnection(baseUrl, apiKey || undefined);
      dispatch({ type: 'SET_OPENAI_COMPAT_STATUS', status: 'connected' });
    } catch (e) {
      dispatch({ type: 'SET_OPENAI_COMPAT_STATUS', status: 'error', error: String(e) });
    }
  }, [dispatch]);

  const handleTestConnection = async () => {
    if (!state.apiKey.trim()) return;
    dispatch({ type: 'SET_TESTING', value: true });
    dispatch({ type: 'SET_TEST_RESULT', result: null });
    try {
      await testOpenRouterConnection(state.apiKey);
      dispatch({ type: 'SET_TEST_RESULT', result: 'success' });
    } catch (e) {
      dispatch({ type: 'SET_TEST_RESULT', result: 'error', error: String(e) });
    } finally {
      dispatch({ type: 'SET_TESTING', value: false });
    }
  };

  // Ollama model lists
  const ollamaEmbeddingModels: AvailableModel[] = state.ollamaModels
    .filter(m => m.is_embedding)
    .map(m => ({ id: m.id, name: m.name }));

  const ollamaLlmModels: AvailableModel[] = state.ollamaModels
    .filter(m => !m.is_embedding)
    .map(m => ({ id: m.id, name: m.name }));

  return (
    <div className="space-y-5 px-2">
      <div className="text-center mb-4">
        <h2 className="text-xl font-bold text-[var(--color-text-primary)] mb-1">AI Provider</h2>
        <p className="text-sm text-[var(--color-text-secondary)]">
          Choose your AI provider
        </p>
      </div>

      {/* Provider selector */}
      <div className="space-y-2">
        <label className="block text-sm font-medium text-[var(--color-text-primary)]">Provider</label>
        <CustomSelect
          value={state.provider}
          onChange={(v) => dispatch({ type: 'SET_PROVIDER', value: v as 'openrouter' | 'ollama' | 'openai_compat' })}
          options={[
            { value: 'openrouter', label: 'OpenRouter' },
            { value: 'ollama', label: 'Ollama' },
            { value: 'openai_compat', label: 'OpenAI Compatible' },
          ]}
        />
      </div>

      {state.provider === 'openrouter' && (
        <>
          {/* OAuth sign-in (web only) */}
          {!isDesktop && (
            <div className="space-y-2">
              <Button
                onClick={handleOAuthSignIn}
                disabled={oauthLoading || state.testResult === 'success'}
                className="w-full"
              >
                {oauthLoading ? 'Waiting for OpenRouter...' : 'Sign in with OpenRouter'}
              </Button>
              {oauthError && (
                <p className="text-sm text-red-500">{oauthError}</p>
              )}
              <div className="relative my-3">
                <div className="absolute inset-0 flex items-center">
                  <div className="w-full border-t border-[var(--color-border)]" />
                </div>
                <div className="relative flex justify-center text-xs">
                  <span className="px-2 bg-[var(--color-bg-panel)] text-[var(--color-text-secondary)]">or enter key manually</span>
                </div>
              </div>
            </div>
          )}

          {/* API Key */}
          <div className="space-y-2">
            <label className="block text-sm font-medium text-[var(--color-text-primary)]">API Key</label>
            <div className="flex gap-2">
              <input
                type="password"
                value={state.apiKey}
                onChange={(e) => dispatch({ type: 'SET_API_KEY', value: e.target.value })}
                placeholder="sk-or-..."
                className="flex-1 px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
              />
              <Button variant="secondary" onClick={handleTestConnection} disabled={!state.apiKey.trim() || state.isTesting}>
                {state.isTesting ? 'Testing...' : 'Test'}
              </Button>
            </div>
            {state.testResult === 'success' && (
              <p className="text-sm text-green-500">Connected successfully</p>
            )}
            {state.testResult === 'error' && (
              <p className="text-sm text-red-500">{state.testError}</p>
            )}
            <p className="text-xs text-[var(--color-text-secondary)]">
              Get an API key from <a href="https://openrouter.ai/keys" target="_blank" rel="noopener noreferrer" className="text-[var(--color-accent)] hover:underline">openrouter.ai/keys</a>
            </p>
          </div>

          {/* Model configuration - only show after successful test */}
          {state.testResult === 'success' && (
            <div className="space-y-3 p-4 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg">
              <h3 className="text-sm font-medium text-[var(--color-text-primary)]">Model Configuration</h3>

              <div className="space-y-2">
                <label className="block text-xs text-[var(--color-text-secondary)]">Embedding Model</label>
                <SearchableSelect
                  value={state.embeddingModel}
                  onChange={(v) => dispatch({ type: 'SET_EMBEDDING_MODEL', value: v })}
                  options={openrouterEmbeddingModels.map(m => ({
                    id: m.id,
                    name: `${m.name} (${m.dimension})`,
                  }))}
                  placeholder="Select embedding model..."
                />
              </div>

              <div className="space-y-2">
                <label className="block text-xs text-[var(--color-text-secondary)]">Tagging Model</label>
                <SearchableSelect
                  value={state.taggingModel}
                  onChange={(v) => dispatch({ type: 'SET_TAGGING_MODEL', value: v })}
                  options={state.availableModels}
                  isLoading={state.isLoadingModels}
                  placeholder="Select tagging model..."
                />
              </div>

              <div className="space-y-2">
                <label className="block text-xs text-[var(--color-text-secondary)]">Wiki Model</label>
                <SearchableSelect
                  value={state.wikiModel}
                  onChange={(v) => dispatch({ type: 'SET_WIKI_MODEL', value: v })}
                  options={state.availableModels}
                  isLoading={state.isLoadingModels}
                  placeholder="Select wiki model..."
                />
              </div>

              <div className="space-y-2">
                <label className="block text-xs text-[var(--color-text-secondary)]">Chat Model</label>
                <SearchableSelect
                  value={state.chatModel}
                  onChange={(v) => dispatch({ type: 'SET_CHAT_MODEL', value: v })}
                  options={state.availableModels}
                  isLoading={state.isLoadingModels}
                  placeholder="Select chat model..."
                />
              </div>
            </div>
          )}
        </>
      )}

      {state.provider === 'ollama' && (
        <>
          {/* Ollama configuration */}
          <div className="space-y-2">
            <label className="block text-sm font-medium text-[var(--color-text-primary)]">Ollama Server URL</label>
            <input
              type="text"
              value={state.ollamaHost}
              onChange={(e) => dispatch({ type: 'SET_OLLAMA_HOST', value: e.target.value })}
              onBlur={(e) => dispatch({ type: 'SET_OLLAMA_HOST', value: normalizeUrl(e.target.value) })}
              placeholder="http://127.0.0.1:11434"
              className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
            />
            <ConnectionStatus status={state.ollamaStatus} error={state.ollamaError} />
          </div>

          {state.ollamaStatus === 'connected' && (
            <div className="space-y-3 p-4 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg">
              <h3 className="text-sm font-medium text-[var(--color-text-primary)]">Model Configuration</h3>

              <div className="space-y-2">
                <label className="block text-xs text-[var(--color-text-secondary)]">Embedding Model</label>
                <SearchableSelect
                  value={state.embeddingModel}
                  onChange={(v) => dispatch({ type: 'SET_EMBEDDING_MODEL', value: v })}
                  options={ollamaEmbeddingModels}
                  isLoading={state.isLoadingOllamaModels}
                  placeholder="Select embedding model..."
                />
              </div>

              <div className="space-y-2">
                <label className="block text-xs text-[var(--color-text-secondary)]">LLM Model (tagging, wiki, chat)</label>
                <SearchableSelect
                  value={state.taggingModel}
                  onChange={(v) => {
                    dispatch({ type: 'SET_TAGGING_MODEL', value: v });
                    dispatch({ type: 'SET_WIKI_MODEL', value: v });
                    dispatch({ type: 'SET_CHAT_MODEL', value: v });
                  }}
                  options={ollamaLlmModels}
                  isLoading={state.isLoadingOllamaModels}
                  placeholder="Select LLM model..."
                />
              </div>

              <div className="space-y-2">
                <label className="block text-xs text-[var(--color-text-secondary)]">Context Length</label>
                <CustomSelect
                  value={state.ollamaContextLength}
                  onChange={(v) => dispatch({ type: 'SET_OLLAMA_CONTEXT_LENGTH', value: v })}
                  options={[
                    { value: '2048', label: '2K' },
                    { value: '4096', label: '4K' },
                    { value: '8192', label: '8K' },
                    { value: '16384', label: '16K' },
                    { value: '32768', label: '32K' },
                    { value: '65536', label: '64K' },
                    { value: '131072', label: '128K' },
                    { value: '262144', label: '256K' },
                    { value: '1000000', label: '1M' },
                  ]}
                />
              </div>

              <div className="space-y-2">
                <label className="block text-xs text-[var(--color-text-secondary)]">Request Timeout (seconds)</label>
                <CustomSelect
                  value={state.ollamaTimeoutSecs}
                  onChange={(v) => dispatch({ type: 'SET_OLLAMA_TIMEOUT_SECS', value: v })}
                  options={[
                    { value: '30', label: '30 seconds' },
                    { value: '60', label: '60 seconds' },
                    { value: '120', label: '2 minutes' },
                    { value: '180', label: '3 minutes' },
                    { value: '300', label: '5 minutes' },
                    { value: '600', label: '10 minutes' },
                  ]}
                />
                <p className="text-[10px] text-[var(--color-text-tertiary)]">
                  Maximum time to wait for Ollama to respond. Increase for slow models or large contexts.
                </p>
              </div>
            </div>
          )}
        </>
      )}

      {state.provider === 'openai_compat' && (
        <>
          {/* Base URL */}
          <div className="space-y-2">
            <label className="block text-sm font-medium text-[var(--color-text-primary)]">Base URL</label>
            <p className="text-xs text-[var(--color-text-secondary)]">
              OpenAI-compatible API endpoint (e.g. http://localhost:8080/v1)
            </p>
            <div className="flex gap-2">
              <input
                type="text"
                value={state.openaiCompatBaseUrl}
                onChange={(e) => dispatch({ type: 'SET_OPENAI_COMPAT_BASE_URL', value: e.target.value })}
                placeholder="http://localhost:8080/v1"
                className="flex-1 px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
              />
              <Button
                variant="secondary"
                onClick={() => checkOpenaiCompatConnection(state.openaiCompatBaseUrl, state.openaiCompatApiKey || undefined)}
                disabled={!state.openaiCompatBaseUrl.trim() || state.openaiCompatStatus === 'checking'}
              >
                {state.openaiCompatStatus === 'checking' ? 'Testing...' : 'Test'}
              </Button>
            </div>
            {state.openaiCompatStatus === 'connected' && (
              <p className="text-sm text-green-500">Connected successfully</p>
            )}
            {state.openaiCompatStatus === 'error' && (
              <p className="text-sm text-red-500">{state.openaiCompatError}</p>
            )}
          </div>

          {/* API Key */}
          <div className="space-y-2">
            <label className="block text-sm font-medium text-[var(--color-text-primary)]">API Key (optional)</label>
            <input
              type="password"
              value={state.openaiCompatApiKey}
              onChange={(e) => dispatch({ type: 'SET_OPENAI_COMPAT_API_KEY', value: e.target.value })}
              placeholder="sk-..."
              className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
            />
          </div>

          {/* Model Configuration */}
          <div className="space-y-3 p-4 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg">
            <h3 className="text-sm font-medium text-[var(--color-text-primary)]">Model Configuration</h3>
            <p className="text-xs text-[var(--color-text-secondary)]">Enter the exact model names your server expects.</p>

            <div className="space-y-2">
              <label className="block text-xs text-[var(--color-text-secondary)]">Embedding Model</label>
              <input
                type="text"
                value={state.openaiCompatEmbeddingModel}
                onChange={(e) => dispatch({ type: 'SET_OPENAI_COMPAT_EMBEDDING_MODEL', value: e.target.value })}
                placeholder="text-embedding-3-small"
                className="w-full px-3 py-2 bg-[var(--color-bg-primary)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
              />
            </div>

            <div className="space-y-2">
              <label className="block text-xs text-[var(--color-text-secondary)]">Embedding Dimension</label>
              <input
                type="number"
                value={state.openaiCompatEmbeddingDimension}
                onChange={(e) => dispatch({ type: 'SET_OPENAI_COMPAT_EMBEDDING_DIMENSION', value: e.target.value })}
                placeholder="1536"
                className="w-full px-3 py-2 bg-[var(--color-bg-primary)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
              />
            </div>

            <div className="space-y-2">
              <label className="block text-xs text-[var(--color-text-secondary)]">LLM Model (tagging, wiki, chat)</label>
              <input
                type="text"
                value={state.openaiCompatLlmModel}
                onChange={(e) => dispatch({ type: 'SET_OPENAI_COMPAT_LLM_MODEL', value: e.target.value })}
                placeholder="meta-llama/Llama-3.1-8B-Instruct"
                className="w-full px-3 py-2 bg-[var(--color-bg-primary)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
              />
            </div>

            <div className="space-y-2">
              <label className="block text-xs text-[var(--color-text-secondary)]">Context Length</label>
              <CustomSelect
                value={state.openaiCompatContextLength}
                onChange={(v) => dispatch({ type: 'SET_OPENAI_COMPAT_CONTEXT_LENGTH', value: v })}
                options={[
                  { value: '2048', label: '2K' },
                  { value: '4096', label: '4K' },
                  { value: '8192', label: '8K' },
                  { value: '16384', label: '16K' },
                  { value: '32768', label: '32K' },
                  { value: '65536', label: '64K' },
                  { value: '131072', label: '128K' },
                  { value: '262144', label: '256K' },
                  { value: '1000000', label: '1M' },
                ]}
              />
            </div>

            <div className="space-y-2">
              <label className="block text-xs text-[var(--color-text-secondary)]">Request Timeout</label>
              <p className="text-xs text-[var(--color-text-secondary)]">Maximum time to wait for the server to respond</p>
              <CustomSelect
                value={state.openaiCompatTimeoutSecs}
                onChange={(v) => dispatch({ type: 'SET_OPENAI_COMPAT_TIMEOUT_SECS', value: v })}
                options={[
                  { value: '30', label: '30 seconds' },
                  { value: '60', label: '60 seconds' },
                  { value: '120', label: '2 minutes' },
                  { value: '180', label: '3 minutes' },
                  { value: '300', label: '5 minutes' },
                  { value: '600', label: '10 minutes' },
                ]}
              />
            </div>
          </div>
        </>
      )}

      {/* Auto-tagging toggle */}
      <div className="flex items-center justify-between p-3 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg">
        <div>
          <p className="text-sm font-medium text-[var(--color-text-primary)]">Automatic Tag Extraction</p>
          <p className="text-xs text-[var(--color-text-secondary)]">Use AI to automatically extract and assign tags to new atoms</p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={state.autoTaggingEnabled}
          onClick={() => dispatch({ type: 'SET_AUTO_TAGGING', value: !state.autoTaggingEnabled })}
          className={`relative inline-flex h-6 w-11 shrink-0 cursor-pointer rounded-full border-2 border-transparent transition-colors duration-200 ease-in-out ${
            state.autoTaggingEnabled ? 'bg-[var(--color-accent)]' : 'bg-[var(--color-bg-hover)]'
          }`}
        >
          <span
            className={`pointer-events-none inline-block h-5 w-5 transform rounded-full bg-white shadow ring-0 transition duration-200 ease-in-out ${
              state.autoTaggingEnabled ? 'translate-x-5' : 'translate-x-0'
            }`}
          />
        </button>
      </div>
    </div>
  );
}
