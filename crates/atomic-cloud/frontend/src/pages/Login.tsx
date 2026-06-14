import { useRef, useState } from 'react';
import type { FormEvent } from 'react';
import { PublicLayout } from '../layouts/PublicLayout';
import { AuthLayout } from '../layouts/AuthLayout';
import { CheckEmail } from '../components/CheckEmail';
import { Button } from '../components/ui/Button';
import { Field } from '../components/ui/Field';
import { Banner } from '../components/ui/Banner';
import { TextLink } from '../components/ui/TextLink';
import { ApiError, requestLoginLink } from '../lib/api';
import { isEmailFormatOk } from '../lib/validate';

/**
 * Sign-in via magic link. The server is deliberately indistinguishable about
 * whether an account exists, so the success copy here is account-neutral —
 * "if there's an account, a link is on its way" — and never confirms or denies
 * the address. The only inline error is a malformed email or a rate limit.
 */
export function Login() {
  const [email, setEmail] = useState('');
  const [emailError, setEmailError] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [sentTo, setSentTo] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  const trimmedEmail = email.trim();
  const clientValid = isEmailFormatOk(trimmedEmail);

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setFormError(null);

    if (!isEmailFormatOk(trimmedEmail)) {
      setEmailError("That email address doesn't look valid.");
      return;
    }
    setEmailError(null);

    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    setSubmitting(true);
    try {
      await requestLoginLink({ email: trimmedEmail }, controller.signal);
      setSentTo(trimmedEmail);
    } catch (err) {
      if (err instanceof DOMException && err.name === 'AbortError') return;
      if (err instanceof ApiError) {
        if (err.code === 'invalid_email') {
          setEmailError(err.message);
        } else if (err.status === 429) {
          const wait = err.retryAfterSeconds;
          setFormError(
            wait
              ? `Too many attempts. Please wait ${formatWait(wait)} and try again.`
              : 'Too many attempts. Please wait a few minutes and try again.',
          );
        } else {
          setFormError(err.message);
        }
      } else {
        setFormError('Something went wrong. Please try again.');
      }
    } finally {
      setSubmitting(false);
    }
  }

  function reset() {
    setSentTo(null);
    setFormError(null);
    setEmailError(null);
  }

  if (sentTo) {
    return (
      <PublicLayout>
        <AuthLayout eyebrow="Sign in" title={<>Check your inbox.</>}>
          <CheckEmail email={sentTo} onUseDifferentEmail={reset}>
            If an Atomic account uses that address, a sign-in link is on its
            way. Open it to continue to your workspace.
          </CheckEmail>
        </AuthLayout>
      </PublicLayout>
    );
  }

  return (
    <PublicLayout>
      <AuthLayout
        eyebrow="Sign in"
        title={
          <>
            Welcome <span className="italic">back.</span>
          </>
        }
        subtitle="Enter your email and we'll send you a link to sign in — no password needed."
        footer={
          <>
            New to Atomic Cloud? <TextLink to="/signup">Create a workspace</TextLink>
          </>
        }
      >
        <form onSubmit={handleSubmit} noValidate className="flex flex-col gap-5">
          {formError && (
            <Banner tone="error" title="Couldn't send your link">
              {formError}
            </Banner>
          )}

          <Field
            label="Email"
            type="email"
            name="email"
            autoComplete="email"
            inputMode="email"
            placeholder="you@example.com"
            value={email}
            onChange={(e) => {
              setEmail(e.target.value);
              if (emailError) setEmailError(null);
            }}
            error={emailError}
            required
            autoFocus
          />

          <Button type="submit" loading={submitting} disabled={!clientValid} fullWidth>
            {submitting ? 'Sending link…' : 'Send my sign-in link'}
          </Button>
        </form>
      </AuthLayout>
    </PublicLayout>
  );
}

function formatWait(seconds: number): string {
  if (seconds < 60) return `${seconds} second${seconds === 1 ? '' : 's'}`;
  const minutes = Math.ceil(seconds / 60);
  return `${minutes} minute${minutes === 1 ? '' : 's'}`;
}
