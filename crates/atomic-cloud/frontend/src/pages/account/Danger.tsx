import { useState } from 'react';
import { TriangleAlert } from 'lucide-react';
import { useAccount } from '../../lib/accountContext';
import { Card } from '../../components/ui/Card';
import { Field } from '../../components/ui/Field';
import { Button } from '../../components/ui/Button';
import { Banner } from '../../components/ui/Banner';
import { deleteAccount } from '../../lib/api';
import { deletionErrorMessage } from './deletionError';
import { appHostLoginUrl } from '../../lib/host';

/**
 * Account settings — deletion. Hard-deletes the account after a typed
 * confirmation that must match the subdomain (the same value the server
 * requires). The action is permanent: it destroys the tenant database and all
 * knowledge bases. On success the credentials are revoked, so we navigate
 * straight to the app host.
 */
export function Danger() {
  const { overview } = useAccount();
  const { subdomain } = overview;
  const [confirm, setConfirm] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const matches = confirm.trim() === subdomain;

  async function handleDelete() {
    if (!matches) return;
    setError(null);
    setSubmitting(true);
    try {
      await deleteAccount(subdomain);
      // The account and its session are gone — leave the (now-dead) tenant
      // subdomain for the app host's login, flagging the deletion so the login
      // page confirms it.
      window.location.assign(appHostLoginUrl('deleted=1'));
    } catch (err) {
      setError(deletionErrorMessage(err));
      setSubmitting(false);
    }
  }

  return (
    <div className="space-y-8">
      <header>
        <p className="text-xs font-medium uppercase tracking-wide text-text-muted">Account</p>
        <h1 className="mt-1 font-display text-3xl tracking-tight md:text-4xl">
          Account <span className="italic">settings.</span>
        </h1>
        <p className="mt-2 text-text-secondary">
          {subdomain} · {overview.email}
        </p>
      </header>

      <Card className="border-red-200/70">
        <div className="flex items-start gap-3">
          <span className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-red-50 text-red-600">
            <TriangleAlert className="h-5 w-5" strokeWidth={1.75} aria-hidden="true" />
          </span>
          <div className="min-w-0 flex-1">
            <h2 className="font-medium text-lg">Delete this account</h2>
            <p className="mt-1 text-sm text-text-secondary leading-relaxed">
              Permanently delete your workspace and every knowledge base, atom,
              wiki, and conversation in it. This cannot be undone. Your provider
              keys are revoked and the subdomain is released.
            </p>

            <div className="mt-5 max-w-sm space-y-4">
              {error && (
                <Banner tone="error" title="Couldn’t delete the account">
                  {error}
                </Banner>
              )}
              <Field
                label={`Type “${subdomain}” to confirm`}
                value={confirm}
                onChange={(e) => {
                  setConfirm(e.target.value);
                  if (error) setError(null);
                }}
                placeholder={subdomain}
                autoComplete="off"
                spellCheck={false}
                disabled={submitting}
              />
              <Button
                variant="primary"
                onClick={handleDelete}
                disabled={!matches}
                loading={submitting}
                className="bg-red-600 hover:bg-red-700 hover:shadow-red-600/20"
              >
                {submitting ? 'Deleting…' : 'Delete account permanently'}
              </Button>
            </div>
          </div>
        </div>
      </Card>
    </div>
  );
}
