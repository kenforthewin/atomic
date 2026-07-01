import { PublicLayout } from '../layouts/PublicLayout';

interface LegalProps {
  kind: 'terms' | 'privacy';
}

const META = {
  terms: {
    title: 'Terms of Service',
    intro: 'These terms govern your use of Atomic’s hosted service.',
  },
  privacy: {
    title: 'Privacy Policy',
    intro:
      'This policy describes what data Atomic collects, how it is used, and how it is protected.',
  },
} as const;

/**
 * Public legal pages, served at `/terms` and `/privacy` on the app host and
 * linked from the footer and the signup consent line. The page structure ships
 * now; the authoritative legal copy is finalized before public launch (tracked
 * separately as a non-code task).
 */
export function Legal({ kind }: LegalProps) {
  const { title, intro } = META[kind];
  return (
    <PublicLayout>
      <article className="mx-auto max-w-2xl px-6 py-16">
        <h1 className="font-display text-4xl tracking-tight text-balance">{title}</h1>
        <p className="mt-2 text-sm text-text-muted">Last updated: —</p>
        <p className="mt-6 text-text-secondary leading-relaxed">{intro}</p>
        <p className="mt-8 rounded-lg border border-border-light bg-bg-secondary p-4 text-sm text-text-muted">
          The full {title.toLowerCase()} is being finalized and will be posted here
          before public launch.
        </p>
      </article>
    </PublicLayout>
  );
}
