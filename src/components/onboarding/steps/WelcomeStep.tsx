import { useState, useEffect } from 'react';
import { BookOpen, Check, Copy, KeyRound } from 'lucide-react';
import { Button } from '../../ui/Button';
import { isDesktopApp, isCloudTenant, getTransport, switchTransport } from '../../../lib/transport';
import { verifyProviderConfigured } from '../../../lib/api';
import type { OnboardingState, OnboardingAction } from '../useOnboardingState';

interface WelcomeStepProps {
  state: OnboardingState;
  dispatch: React.Dispatch<OnboardingAction>;
  onNext: () => void;
  onComplete: () => void;
}

type SetupMode = 'checking' | 'claim' | 'manual';

interface SetupStatus {
  needs_setup: boolean;
  already_claimed: boolean;
  requires_setup_token: boolean;
  setup_token_configured: boolean;
}

export function WelcomeStep({ state, dispatch, onNext, onComplete }: WelcomeStepProps) {
  const isDesktop = isDesktopApp();
  // On a cloud tenant the app is already connected via the session cookie —
  // there's no server to connect to or token to paste, so it shares the
  // desktop "you're connected, here's what's next" welcome rather than the
  // self-hosted server-connection form.
  const isCloud = isCloudTenant();
  const [setupMode, setSetupMode] = useState<SetupMode>('checking');
  const [isClaiming, setIsClaiming] = useState(false);
  const [claimError, setClaimError] = useState<string | null>(null);
  const [claimedToken, setClaimedToken] = useState<string | null>(null);
  const [tokenCopied, setTokenCopied] = useState(false);
  const [setupStatus, setSetupStatus] = useState<SetupStatus | null>(null);
  const [setupToken, setSetupToken] = useState('');

  // On mount (web mode only), check if we're co-hosted with the server
  useEffect(() => {
    if (isDesktop || isCloud) return;
    if (getTransport().isConnected()) return;

    const baseUrl = window.location.origin;
    fetch(`${baseUrl}/api/setup/status`)
      .then(r => r.ok ? r.json() : Promise.reject(new Error(`${r.status}`)))
      .then((data: SetupStatus) => {
        setSetupStatus(data);
        if (data.needs_setup) {
          dispatch({ type: 'SET_SERVER_URL', value: baseUrl });
          setSetupMode('claim');
        } else {
          // Server exists at same origin but is already claimed — pre-fill URL
          dispatch({ type: 'SET_SERVER_URL', value: baseUrl });
          setSetupMode('manual');
        }
      })
      .catch(() => {
        // No co-hosted server — show manual form
        setSetupMode('manual');
      });
  }, [isDesktop, isCloud, dispatch]);

  const handleClaim = async () => {
    setIsClaiming(true);
    setClaimError(null);
    const baseUrl = state.serverUrl.trim().replace(/\/$/, '');
    try {
      const resp = await fetch(`${baseUrl}/api/setup/claim`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name: 'default',
          ...(setupToken.trim() ? { setup_token: setupToken.trim() } : {}),
        }),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      const data = await resp.json();
      // Connect with the new token, then show it to the user
      await switchTransport({ baseUrl, authToken: data.token });
      setClaimedToken(data.token);
    } catch (e) {
      setClaimError(String(e instanceof Error ? e.message : e));
    } finally {
      setIsClaiming(false);
    }
  };

  const handleTestServer = async () => {
    if (!state.serverUrl.trim() || !state.serverToken.trim()) return;
    dispatch({ type: 'SET_TESTING_SERVER', value: true });
    dispatch({ type: 'SET_SERVER_TEST', result: null });
    try {
      const resp = await fetch(`${state.serverUrl.trim().replace(/\/$/, '')}/health`);
      if (resp.ok) {
        dispatch({ type: 'SET_SERVER_TEST', result: 'success' });
      } else {
        dispatch({ type: 'SET_SERVER_TEST', result: 'error', error: `Server returned ${resp.status}` });
      }
    } catch (e) {
      dispatch({ type: 'SET_SERVER_TEST', result: 'error', error: String(e) });
    } finally {
      dispatch({ type: 'SET_TESTING_SERVER', value: false });
    }
  };

  const handleConnect = async () => {
    try {
      await switchTransport({
        baseUrl: state.serverUrl.trim().replace(/\/$/, ''),
        authToken: state.serverToken.trim(),
      });
      // If the server is already fully set up, skip the rest of the
      // onboarding wizard and drop the user straight into the app.
      try {
        const configured = await verifyProviderConfigured();
        if (configured) {
          onComplete();
          return;
        }
      } catch (e) {
        dispatch({
          type: 'SET_SERVER_TEST',
          result: 'error',
          error: String(e instanceof Error ? e.message : e),
        });
        return;
      }
      onNext();
    } catch (e) {
      dispatch({ type: 'SET_SERVER_TEST', result: 'error', error: String(e) });
    }
  };

  if (isDesktop || isCloud) {
    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-[var(--color-accent)]/10 flex items-center justify-center">
          <BookOpen className="w-8 h-8 text-[var(--color-accent)]" strokeWidth={2} />
        </div>

        <div>
          <h2 className="text-2xl font-bold text-[var(--color-text-primary)] mb-2">
            Welcome to Atomic
          </h2>
          <p className="text-[var(--color-text-secondary)] max-w-md">
            Your personal knowledge base that turns freeform notes into a semantically-connected, AI-augmented knowledge graph.
          </p>
        </div>

        <div className="bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg p-4 text-left max-w-md w-full">
          <h3 className="text-sm font-medium text-[var(--color-text-primary)] mb-2">What you'll set up:</h3>
          <ul className="space-y-1.5 text-sm text-[var(--color-text-secondary)]">
            {isCloud ? (
              // Cloud: AI is already provisioned (managed key), so onboarding is
              // about the rest — what gets auto-tagged, optional integrations,
              // and bringing notes in.
              <>
                <li className="flex items-center gap-2">
                  <span className="text-[var(--color-accent)]">1.</span> Tag categories the auto-tagger will use
                </li>
                <li className="flex items-center gap-2">
                  <span className="text-[var(--color-accent)]">2.</span> Optional integrations (MCP, mobile, browser extension)
                </li>
                <li className="flex items-center gap-2">
                  <span className="text-[var(--color-accent)]">3.</span> Import existing notes or start fresh
                </li>
              </>
            ) : (
              <>
                <li className="flex items-center gap-2">
                  <span className="text-[var(--color-accent)]">1.</span> AI provider for embeddings, tagging & chat
                </li>
                <li className="flex items-center gap-2">
                  <span className="text-[var(--color-accent)]">2.</span> Optional integrations (MCP, mobile, browser extension)
                </li>
                <li className="flex items-center gap-2">
                  <span className="text-[var(--color-accent)]">3.</span> Import existing notes or start fresh
                </li>
              </>
            )}
          </ul>
        </div>

        <p className="text-xs text-[var(--color-text-secondary)]">
          Required steps are marked. You can skip optional steps and configure them later in Settings.
        </p>
      </div>
    );
  }

  // Token reveal after claiming (must be checked before isConnected,
  // since claiming connects the transport)
  if (claimedToken) {
    const handleCopyToken = async () => {
      try {
        await navigator.clipboard.writeText(claimedToken);
      } catch {
        // Fallback for non-secure contexts / Tauri webview
        const textarea = document.createElement('textarea');
        textarea.value = claimedToken;
        textarea.style.position = 'fixed';
        textarea.style.opacity = '0';
        document.body.appendChild(textarea);
        textarea.select();
        document.execCommand('copy');
        document.body.removeChild(textarea);
      }
      setTokenCopied(true);
      setTimeout(() => setTokenCopied(false), 2000);
    };

    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-green-500/10 flex items-center justify-center">
          <Check className="w-8 h-8 text-green-500" strokeWidth={2} />
        </div>

        <div>
          <h2 className="text-2xl font-bold text-[var(--color-text-primary)] mb-2">
            Instance Claimed
          </h2>
          <p className="text-[var(--color-text-secondary)] max-w-md">
            Save this API token somewhere safe. You'll need it to connect from other devices or if you clear your browser data.
          </p>
        </div>

        <div className="w-full max-w-md space-y-2">
          <div className="flex items-center gap-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md p-3">
            <code className="flex-1 text-sm text-[var(--color-text-primary)] break-all text-left font-mono">
              {claimedToken}
            </code>
            <button
              onClick={handleCopyToken}
              className="shrink-0 p-1.5 rounded hover:bg-[var(--color-bg-hover)] transition-colors"
              title="Copy to clipboard"
            >
              {tokenCopied ? (
                <Check className="w-4 h-4 text-green-500" strokeWidth={2} />
              ) : (
                <Copy className="w-4 h-4 text-[var(--color-text-secondary)]" strokeWidth={2} />
              )}
            </button>
          </div>
          <p className="text-xs text-[var(--color-text-secondary)]">
            This token won't be shown again.
          </p>
        </div>

        <Button onClick={onNext}>
          Continue
        </Button>
      </div>
    );
  }

  // Web mode: already connected (e.g. returning user with saved config)
  const isConnected = getTransport().isConnected();

  if (isConnected) {
    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-green-500/10 flex items-center justify-center">
          <Check className="w-8 h-8 text-green-500" strokeWidth={2} />
        </div>
        <div>
          <h2 className="text-2xl font-bold text-[var(--color-text-primary)] mb-2">Connected</h2>
          <p className="text-[var(--color-text-secondary)]">You're connected to an Atomic server. Let's configure your AI provider.</p>
        </div>
      </div>
    );
  }

  // Checking for co-hosted server
  if (setupMode === 'checking') {
    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-4 px-8">
        <div className="w-8 h-8 border-2 border-[var(--color-accent)] border-t-transparent rounded-full animate-spin" />
        <p className="text-sm text-[var(--color-text-secondary)]">Detecting server...</p>
      </div>
    );
  }

  // Unclaimed instance — show claim UI
  if (setupMode === 'claim') {
    const requiresSetupToken = setupStatus?.requires_setup_token ?? false;

    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-[var(--color-accent)]/10 flex items-center justify-center">
          <BookOpen className="w-8 h-8 text-[var(--color-accent)]" strokeWidth={2} />
        </div>

        <div>
          <h2 className="text-2xl font-bold text-[var(--color-text-primary)] mb-2">
            Welcome to Atomic
          </h2>
          <p className="text-[var(--color-text-secondary)] max-w-md">
            Your personal knowledge base is ready to set up. Claim this instance to create your admin token and get started.
          </p>
        </div>

        {requiresSetupToken && (
          <div className="w-full max-w-md space-y-2 text-left">
            <label className="flex items-center gap-2 text-sm font-medium text-[var(--color-text-primary)]">
              <KeyRound className="w-4 h-4 text-[var(--color-accent)]" strokeWidth={2} />
              Setup Token
            </label>
            <input
              type="password"
              value={setupToken}
              onChange={(e) => setSetupToken(e.target.value)}
              placeholder="ATOMIC_SETUP_TOKEN"
              className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
            />
            {!setupStatus?.setup_token_configured && (
              <p className="text-xs text-amber-400">
                This server needs ATOMIC_SETUP_TOKEN before it can be claimed.
              </p>
            )}
          </div>
        )}

        <Button onClick={handleClaim} disabled={isClaiming || (requiresSetupToken && !setupToken.trim())}>
          {isClaiming ? 'Setting up...' : 'Get Started'}
        </Button>

        {claimError && (
          <div className="text-sm text-red-500">{claimError}</div>
        )}

        <button
          className="text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
          onClick={() => setSetupMode('manual')}
        >
          Connect to a different server instead
        </button>
      </div>
    );
  }

  // Manual connection form
  return (
    <div className="space-y-6 px-2">
      <div className="text-center mb-6">
        <h2 className="text-xl font-bold text-[var(--color-text-primary)] mb-1">Connect to Atomic Server</h2>
        <p className="text-sm text-[var(--color-text-secondary)]">
          Enter the URL and auth token of your running atomic-server
        </p>
      </div>

      <div className="space-y-4">
        <div className="space-y-1.5">
          <label className="block text-sm font-medium text-[var(--color-text-primary)]">Server URL</label>
          <input
            type="text"
            value={state.serverUrl}
            onChange={(e) => dispatch({ type: 'SET_SERVER_URL', value: e.target.value })}
            placeholder="http://localhost:8080"
            className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
          />
        </div>

        <div className="space-y-1.5">
          <label className="block text-sm font-medium text-[var(--color-text-primary)]">Auth Token</label>
          <input
            type="password"
            value={state.serverToken}
            onChange={(e) => dispatch({ type: 'SET_SERVER_TOKEN', value: e.target.value })}
            placeholder="API token"
            className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent transition-colors duration-150 text-sm"
          />
        </div>

        <div className="flex gap-2">
          <Button variant="secondary" onClick={handleTestServer} disabled={!state.serverUrl.trim() || !state.serverToken.trim() || state.isTestingServer}>
            {state.isTestingServer ? 'Testing...' : 'Test Connection'}
          </Button>
          <Button onClick={handleConnect} disabled={state.serverTestResult !== 'success'}>
            Connect
          </Button>
        </div>

        {state.serverTestResult === 'success' && (
          <div className="text-sm text-green-500">Server reachable</div>
        )}
        {state.serverTestResult === 'error' && (
          <div className="text-sm text-red-500">{state.serverTestError}</div>
        )}

        <div className="p-3 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-xs text-[var(--color-text-secondary)] space-y-1">
          <p>Don't have a token? Create one with:</p>
          <code className="block text-[var(--color-text-primary)]">atomic-server token create --name my-token</code>
        </div>
      </div>
    </div>
  );
}
