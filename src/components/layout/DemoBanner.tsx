import { demoSignupUrl } from '../../lib/transport';

/**
 * The slim strip shown to anonymous visitors on the cloud's public demo
 * instance (rendered only when `isDemoInstance()`): names what they're
 * looking at and carries the one CTA. The server-side whitelist is the
 * real enforcement; this is orientation.
 */
export function DemoBanner() {
  return (
    <div className="flex-shrink-0 flex items-center justify-center gap-3 px-4 py-1.5 bg-[var(--color-accent)] text-white text-sm">
      <span>
        Live demo — a real Atomic knowledge base, read-only. Search it, explore
        the canvas, read the wikis.
      </span>
      <a
        href={demoSignupUrl()}
        className="flex-shrink-0 px-2.5 py-0.5 rounded-md bg-white/15 hover:bg-white/25 font-medium whitespace-nowrap"
      >
        Get your own →
      </a>
    </div>
  );
}
