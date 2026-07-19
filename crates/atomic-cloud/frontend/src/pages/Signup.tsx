import { useMemo, useRef, useState } from 'react';
import type { FormEvent } from 'react';
import { PublicLayout } from '../layouts/PublicLayout';
import { AuthLayout } from '../layouts/AuthLayout';
import { CheckEmail } from '../components/CheckEmail';
import { Button } from '../components/ui/Button';
import { Field } from '../components/ui/Field';
import { Banner } from '../components/ui/Banner';
import { ExternalTextLink, TextLink } from '../components/ui/TextLink';
import { ApiError, requestSignupLink } from '../lib/api';
import {
  isEmailFormatOk,
  isSubdomainFormatOk,
  normalizeSubdomainInput,
} from '../lib/validate';
import { configuredBaseDomain } from '../lib/host';

/** The base shown in the live `<slug>.<base>` preview. */
function previewBaseDomain(): string {
  return configuredBaseDomain() ?? 'atomicapp.ai';
}

interface FieldErrors {
  email?: string;
  subdomain?: string;
}

export function Signup() {
  const [email, setEmail] = useState('');
  const [subdomain, setSubdomain] = useState('');
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>({});
  const [formError, setFormError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [sentTo, setSentTo] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  const base = useMemo(previewBaseDomain, []);
  const trimmedEmail = email.trim();

  const clientValid =
    isEmailFormatOk(trimmedEmail) && isSubdomainFormatOk(subdomain);

  function validateLocally(): FieldErrors {
    const errors: FieldErrors = {};
    if (!isEmailFormatOk(trimmedEmail)) {
      errors.email = "That email address doesn't look valid.";
    }
    if (!isSubdomainFormatOk(subdomain)) {
      errors.subdomain =
        'Subdomains are 3–32 characters of a–z, 0–9, and hyphens.';
    }
    return errors;
  }

  function mapServerError(err: ApiError): FieldErrors {
    switch (err.code) {
      case 'invalid_email':
        return { email: err.message };
      case 'invalid_subdomain':
        return { subdomain: err.message };
      case 'subdomain_taken':
      case 'subdomain_reserved':
        return { subdomain: err.message };
      default:
        return {};
    }
  }

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setFormError(null);

    const localErrors = validateLocally();
    if (Object.keys(localErrors).length > 0) {
      setFieldErrors(localErrors);
      return;
    }
    setFieldErrors({});

    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    setSubmitting(true);
    try {
      await requestSignupLink(
        { email: trimmedEmail, subdomain },
        controller.signal,
      );
      setSentTo(trimmedEmail);
    } catch (err) {
      if (err instanceof DOMException && err.name === 'AbortError') return;
      if (err instanceof ApiError) {
        const mapped = mapServerError(err);
        if (Object.keys(mapped).length > 0) {
          setFieldErrors(mapped);
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
    setFieldErrors({});
  }

  if (sentTo) {
    return (
      <PublicLayout>
        <AuthLayout
          eyebrow="Almost there"
          title={<>One link away.</>}
          footer={
            <>
              Wrong subdomain?{' '}
              <button
                type="button"
                onClick={reset}
                className="font-medium text-accent hover:text-accent-dark transition-colors"
              >
                Start over
              </button>
            </>
          }
        >
          <CheckEmail email={sentTo} onUseDifferentEmail={reset}>
            We sent a sign-up link to confirm your address and set up{' '}
            <span className="font-medium text-text-secondary">
              {subdomain}.{base}
            </span>
            . Open it to finish creating your workspace.
          </CheckEmail>
        </AuthLayout>
      </PublicLayout>
    );
  }

  return (
    <PublicLayout>
      <AuthLayout
        eyebrow="Create your workspace"
        title="Start your Atomic graph."
        subtitle="Pick a subdomain and we'll email you a link to finish — no password to remember."
        footer={
          <>
            Already have an account? <TextLink to="/login">Sign in</TextLink>
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
              if (fieldErrors.email) {
                setFieldErrors((prev) => ({ ...prev, email: undefined }));
              }
            }}
            error={fieldErrors.email}
            required
            autoFocus
          />

          <Field
            label="Subdomain"
            name="subdomain"
            autoComplete="off"
            autoCapitalize="none"
            spellCheck={false}
            placeholder="my-workspace"
            value={subdomain}
            onChange={(e) => {
              setSubdomain(normalizeSubdomainInput(e.target.value));
              if (fieldErrors.subdomain) {
                setFieldErrors((prev) => ({ ...prev, subdomain: undefined }));
              }
            }}
            error={fieldErrors.subdomain}
            help={
              <>
                Your workspace will live at{' '}
                <span className="font-mono text-text-secondary">
                  {subdomain || 'your-name'}.{base}
                </span>
                . 3–32 characters: a–z, 0–9, hyphens.
              </>
            }
            required
          />

          <Button type="submit" loading={submitting} disabled={!clientValid} fullWidth>
            {submitting ? 'Sending link…' : 'Send my sign-up link'}
          </Button>

          <p className="text-center text-xs text-text-muted">
            By signing up, you agree to our{' '}
            <ExternalTextLink
              href="https://atomicapp.ai/terms"
              target="_blank"
              rel="noopener noreferrer"
              className="text-xs"
            >
              Terms
            </ExternalTextLink>{' '}
            and{' '}
            <ExternalTextLink
              href="https://atomicapp.ai/privacy"
              target="_blank"
              rel="noopener noreferrer"
              className="text-xs"
            >
              Privacy Policy
            </ExternalTextLink>
            .
          </p>
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
