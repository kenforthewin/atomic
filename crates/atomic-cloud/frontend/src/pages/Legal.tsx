import { useEffect } from 'react';
import { PublicLayout } from '../layouts/PublicLayout';
import { ExternalTextLink } from '../components/ui/TextLink';

interface LegalProps {
  kind: 'terms' | 'privacy';
}

const META = {
  terms: { title: 'Terms of Service' },
  privacy: { title: 'Privacy Policy' },
} as const;

/**
 * `/terms` and `/privacy` on the app host redirect to the canonical legal
 * pages on the marketing site. One maintained copy of each document —
 * app-host links (footer, signup consent line, old emails) all land there
 * rather than on a second copy that can drift.
 */
export function Legal({ kind }: LegalProps) {
  const canonical = `https://atomicapp.ai/${kind}`;
  const { title } = META[kind];

  useEffect(() => {
    window.location.replace(canonical);
  }, [canonical]);

  // Fallback for the instant before the redirect (or with JS navigation
  // suppressed): a plain link to the same destination.
  return (
    <PublicLayout>
      <article className="mx-auto max-w-2xl px-6 py-16">
        <h1 className="font-display text-4xl tracking-tight text-balance">{title}</h1>
        <p className="mt-6 text-text-secondary leading-relaxed">
          The {title.toLowerCase()} lives at{' '}
          <ExternalTextLink href={canonical}>{canonical.replace('https://', '')}</ExternalTextLink>
          . Redirecting…
        </p>
      </article>
    </PublicLayout>
  );
}
