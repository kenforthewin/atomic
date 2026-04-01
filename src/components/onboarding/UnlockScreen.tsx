import { useState } from 'react';
import { Button } from '../ui/Button';

interface UnlockScreenProps {
  baseUrl: string;
  onUnlocked: () => Promise<void>;
}

export function UnlockScreen({ baseUrl, onUnlocked }: UnlockScreenProps) {
  const [passphrase, setPassphrase] = useState('');
  const [isUnlocking, setIsUnlocking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleUnlock = async () => {
    setIsUnlocking(true);
    setError(null);
    try {
      const resp = await fetch(`${baseUrl.replace(/\/$/, '')}/api/setup/unlock`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ passphrase }),
      });
      if (!resp.ok) {
        const err = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        throw new Error(err.error || err.hint || `HTTP ${resp.status}`);
      }
      // Stay in "unlocking" state while the parent reconnects the transport —
      // the component will be unmounted once Layout transitions to the app.
      await onUnlocked();
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e));
      setIsUnlocking(false);
    }
  };

  return (
    <div className="flex flex-col items-center justify-center w-full h-full text-center space-y-6 px-8">
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
          value={passphrase}
          onChange={(e) => setPassphrase(e.target.value)}
          onKeyDown={(e) => { if (e.key === 'Enter' && passphrase) handleUnlock(); }}
          placeholder="Passphrase"
          autoFocus
          className="w-full px-3 py-2 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-md text-[var(--color-text-primary)] placeholder-[var(--color-text-secondary)] focus:outline-none focus:ring-2 focus:ring-[var(--color-accent)] focus:border-transparent text-sm"
        />

        <Button onClick={handleUnlock} disabled={isUnlocking || !passphrase}>
          {isUnlocking ? 'Unlocking...' : 'Unlock'}
        </Button>

        {error && (
          <div className="text-sm text-red-500">{error}</div>
        )}
      </div>
    </div>
  );
}
