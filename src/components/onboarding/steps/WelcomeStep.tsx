import { useState, useEffect } from 'react';
import { Button } from '../../ui/Button';
import { isDesktopApp, isDesktopFreshInstall, getTransport, switchTransport, saveDesktopToken, getLocalServerConfig } from '../../../lib/transport';
import type { OnboardingState, OnboardingAction } from '../useOnboardingState';

interface WelcomeStepProps {
  state: OnboardingState;
  dispatch: React.Dispatch<OnboardingAction>;
  onNext: () => void;
}

type SetupMode = 'checking' | 'claim' | 'unlock' | 'manual';

export function WelcomeStep({ state, dispatch, onNext }: WelcomeStepProps) {
  const isDesktop = isDesktopApp();
  const [setupMode, setSetupMode] = useState<SetupMode>('checking');
  const [isClaiming, setIsClaiming] = useState(false);
  const [claimError, setClaimError] = useState<string | null>(null);
  const [isUnlocking, setIsUnlocking] = useState(false);
  const [unlockPassphrase, setUnlockPassphrase] = useState('');
  const [unlockError, setUnlockError] = useState<string | null>(null);
  const [claimedToken, setClaimedToken] = useState<string | null>(null);
  const [tokenCopied, setTokenCopied] = useState(false);
  const [passphrase, setPassphrase] = useState('');
  const [passphraseConfirm, setPassphraseConfirm] = useState('');
  const [enableEncryption, setEnableEncryption] = useState(false);

  // On mount (web mode only), check if we're co-hosted with the server
  useEffect(() => {
    if (isDesktop) return;
    if (getTransport().isConnected()) return;

    const baseUrl = window.location.origin;
    fetch(`${baseUrl}/api/setup/status`)
      .then(r => r.ok ? r.json() : Promise.reject(new Error(`${r.status}`)))
      .then((data: { needs_setup: boolean; needs_unlock: boolean }) => {
        dispatch({ type: 'SET_SERVER_URL', value: baseUrl });
        if (data.needs_unlock) {
          setSetupMode('unlock');
        } else if (data.needs_setup) {
          setSetupMode('claim');
        } else {
          // Server exists at same origin but is already claimed — pre-fill URL
          setSetupMode('manual');
        }
      })
      .catch(() => {
        // No co-hosted server — show manual form
        setSetupMode('manual');
      });
  }, [isDesktop, dispatch]);

  const handleClaim = async () => {
    if (enableEncryption && passphrase !== passphraseConfirm) {
      setClaimError('Passphrases do not match');
      return;
    }
    if (enableEncryption && passphrase.length < 8) {
      setClaimError('Passphrase must be at least 8 characters');
      return;
    }
    setIsClaiming(true);
    setClaimError(null);
    const baseUrl = state.serverUrl.trim().replace(/\/$/, '');
    try {
      const claimBody: Record<string, string> = { name: 'default' };
      if (enableEncryption && passphrase) {
        claimBody.passphrase = passphrase;
      }
      const resp = await fetch(`${baseUrl}/api/setup/claim`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(claimBody),
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

  const handleUnlock = async () => {
    setIsUnlocking(true);
    setUnlockError(null);
    const baseUrl = state.serverUrl.trim().replace(/\/$/, '');
    try {
      const resp = await fetch(`${baseUrl}/api/setup/unlock`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ passphrase: unlockPassphrase }),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || err.hint || `HTTP ${resp.status}`);
      }
      // Unlocked — now check if we need to show manual login or already have a token
      setSetupMode('manual');
    } catch (e) {
      setUnlockError(String(e instanceof Error ? e.message : e));
    } finally {
      setIsUnlocking(false);
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
      onNext();
    } catch (e) {
      dispatch({ type: 'SET_SERVER_TEST', result: 'error', error: String(e) });
    }
  };

  const handleDesktopClaim = async () => {
    if (enableEncryption && passphrase !== passphraseConfirm) {
      setClaimError('Passphrases do not match');
      return;
    }
    if (enableEncryption && passphrase.length < 8) {
      setClaimError('Passphrase must be at least 8 characters');
      return;
    }
    setIsClaiming(true);
    setClaimError(null);
    const baseUrl = getLocalServerConfig()?.baseUrl || `http://127.0.0.1:44380`;
    try {
      const claimBody: Record<string, string> = { name: 'default' };
      if (enableEncryption && passphrase) {
        claimBody.passphrase = passphrase;
      }
      const resp = await fetch(`${baseUrl}/api/setup/claim`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(claimBody),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || `HTTP ${resp.status}`);
      }
      const data = await resp.json();
      // Save token to disk so it persists across restarts (desktop-managed, never shown to user)
      await saveDesktopToken(data.token);
      await switchTransport({ baseUrl, authToken: data.token });
      onNext();
    } catch (e) {
      setClaimError(String(e instanceof Error ? e.message : e));
    } finally {
      setIsClaiming(false);
    }
  };

  if (isDesktop && isDesktopFreshInstall()) {
    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-[var(--color-accent)]/10 flex items-center justify-center">
          <svg className="w-8 h-8 text-[var(--color-accent)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M12 6.042A8.967 8.967 0 006 3.75c-1.052 0-2.062.18-3 .512v14.25A8.987 8.987 0 016 18c2.305 0 4.408.867 6 2.292m0-14.25a8.966 8.966 0 016-2.292c1.052 0 2.062.18 3 .512v14.25A8.987 8.987 0 0018 18a8.967 8.967 0 00-6 2.292m0-14.25v14.25" />
          </svg>
        </div>

        <div>
          <h2 className="text-2xl font-bold text-[var(--color-text-primary)] mb-2">
            Welcome to Atomic
          </h2>
          <p className="text-[var(--color-text-secondary)] max-w-md">
            Your personal knowledge base that turns freeform notes into a semantically-connected, AI-augmented knowledge graph.
          </p>
        </div>

        {/* Encryption option */}
        <div className="w-full max-w-md space-y-3">
          <label className="flex items-center gap-3 cursor-pointer">
            <input
              type="checkbox"
              checked={enableEncryption}
              onChange={(e) => setEnableEncryption(e.target.checked)}
              className="w-4 h-4 rounded border-[var(--color-border)] text-[var(--color-accent)] focus:ring-[var(--color-accent)]"
            />
            <div className="text-left">
              <span className="text-sm font-medium text-[var(--color-text-primary)]">Encrypt database at rest</span>
              <p className="text-xs text-[var(--color-text-secondary)]">Protects your data with a passphrase using SQLCipher (AES-256)</p>
            </div>
          </label>

          {enableEncryption && (
            <div className="space-y-2 pl-7">
              <input
                type="password"
                value={passphrase}
                onChange={(e) => setPassphrase(e.target.value)}
                placeholder="Passphrase (min 8 characters)"
                className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent text-sm"
              />
              <input
                type="password"
                value={passphraseConfirm}
                onChange={(e) => setPassphraseConfirm(e.target.value)}
                placeholder="Confirm passphrase"
                className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent text-sm"
              />
              <p className="text-xs text-[var(--color-text-secondary)]">
                You'll need this passphrase every time the app restarts. If you lose it, your data cannot be recovered.
              </p>
            </div>
          )}
        </div>

        <Button onClick={handleDesktopClaim} disabled={isClaiming || (enableEncryption && (passphrase.length < 8 || passphrase !== passphraseConfirm))}>
          {isClaiming ? 'Setting up...' : 'Get Started'}
        </Button>

        {claimError && (
          <div className="text-sm text-red-500">{claimError}</div>
        )}
      </div>
    );
  }

  if (isDesktop) {
    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-[var(--color-accent)]/10 flex items-center justify-center">
          <svg className="w-8 h-8 text-[var(--color-accent)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M12 6.042A8.967 8.967 0 006 3.75c-1.052 0-2.062.18-3 .512v14.25A8.987 8.987 0 016 18c2.305 0 4.408.867 6 2.292m0-14.25a8.966 8.966 0 016-2.292c1.052 0 2.062.18 3 .512v14.25A8.987 8.987 0 0018 18a8.967 8.967 0 00-6 2.292m0-14.25v14.25" />
          </svg>
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
            <li className="flex items-center gap-2">
              <span className="text-[var(--color-accent)]">1.</span> AI provider for embeddings, tagging & chat
            </li>
            <li className="flex items-center gap-2">
              <span className="text-[var(--color-accent)]">2.</span> Optional integrations (MCP, mobile, browser extension)
            </li>
            <li className="flex items-center gap-2">
              <span className="text-[var(--color-accent)]">3.</span> Import existing notes or start fresh
            </li>
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
      await navigator.clipboard.writeText(claimedToken);
      setTokenCopied(true);
      setTimeout(() => setTokenCopied(false), 2000);
    };

    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-green-500/10 flex items-center justify-center">
          <svg className="w-8 h-8 text-green-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
          </svg>
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
                <svg className="w-4 h-4 text-green-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
                </svg>
              ) : (
                <svg className="w-4 h-4 text-[var(--color-text-secondary)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                  <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M8 16H6a2 2 0 01-2-2V6a2 2 0 012-2h8a2 2 0 012 2v2m-6 12h8a2 2 0 002-2v-8a2 2 0 00-2-2h-8a2 2 0 00-2 2v8a2 2 0 002 2z" />
                </svg>
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
          <svg className="w-8 h-8 text-green-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
          </svg>
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
    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-[var(--color-accent)]/10 flex items-center justify-center">
          <svg className="w-8 h-8 text-[var(--color-accent)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M12 6.042A8.967 8.967 0 006 3.75c-1.052 0-2.062.18-3 .512v14.25A8.987 8.987 0 016 18c2.305 0 4.408.867 6 2.292m0-14.25a8.966 8.966 0 016-2.292c1.052 0 2.062.18 3 .512v14.25A8.987 8.987 0 0018 18a8.967 8.967 0 00-6 2.292m0-14.25v14.25" />
          </svg>
        </div>

        <div>
          <h2 className="text-2xl font-bold text-[var(--color-text-primary)] mb-2">
            Welcome to Atomic
          </h2>
          <p className="text-[var(--color-text-secondary)] max-w-md">
            Your personal knowledge base is ready to set up. Claim this instance to create your admin token and get started.
          </p>
        </div>

        {/* Encryption option */}
        <div className="w-full max-w-md space-y-3">
          <label className="flex items-center gap-3 cursor-pointer">
            <input
              type="checkbox"
              checked={enableEncryption}
              onChange={(e) => setEnableEncryption(e.target.checked)}
              className="w-4 h-4 rounded border-[var(--color-border)] text-[var(--color-accent)] focus:ring-[var(--color-accent)]"
            />
            <div className="text-left">
              <span className="text-sm font-medium text-[var(--color-text-primary)]">Encrypt database at rest</span>
              <p className="text-xs text-[var(--color-text-secondary)]">Protects your data with a passphrase using SQLCipher (AES-256)</p>
            </div>
          </label>

          {enableEncryption && (
            <div className="space-y-2 pl-7">
              <input
                type="password"
                value={passphrase}
                onChange={(e) => setPassphrase(e.target.value)}
                placeholder="Passphrase (min 8 characters)"
                className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent text-sm"
              />
              <input
                type="password"
                value={passphraseConfirm}
                onChange={(e) => setPassphraseConfirm(e.target.value)}
                placeholder="Confirm passphrase"
                className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent text-sm"
              />
              <p className="text-xs text-[var(--color-text-secondary)]">
                You'll need this passphrase every time the server restarts. If you lose it, your data cannot be recovered.
              </p>
            </div>
          )}
        </div>

        <Button onClick={handleClaim} disabled={isClaiming || (enableEncryption && (passphrase.length < 8 || passphrase !== passphraseConfirm))}>
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

  // Encrypted database — needs unlock
  if (setupMode === 'unlock') {
    return (
      <div className="flex flex-col items-center justify-center h-full text-center space-y-6 px-8">
        <div className="w-16 h-16 rounded-2xl bg-[var(--color-accent)]/10 flex items-center justify-center">
          <svg className="w-8 h-8 text-[var(--color-accent)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M16.5 10.5V6.75a4.5 4.5 0 10-9 0v3.75m-.75 11.25h10.5a2.25 2.25 0 002.25-2.25v-6.75a2.25 2.25 0 00-2.25-2.25H6.75a2.25 2.25 0 00-2.25 2.25v6.75a2.25 2.25 0 002.25 2.25z" />
          </svg>
        </div>

        <div>
          <h2 className="text-2xl font-bold text-[var(--color-text-primary)] mb-2">
            Database Locked
          </h2>
          <p className="text-[var(--color-text-secondary)] max-w-md">
            Your database is encrypted. Enter your passphrase to unlock it.
          </p>
        </div>

        <div className="w-full max-w-sm space-y-3">
          <input
            type="password"
            value={unlockPassphrase}
            onChange={(e) => setUnlockPassphrase(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter' && unlockPassphrase) handleUnlock(); }}
            placeholder="Passphrase"
            autoFocus
            className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent text-sm"
          />

          <Button onClick={handleUnlock} disabled={isUnlocking || !unlockPassphrase}>
            {isUnlocking ? 'Unlocking...' : 'Unlock'}
          </Button>

          {unlockError && (
            <div className="text-sm text-red-500">{unlockError}</div>
          )}
        </div>
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
