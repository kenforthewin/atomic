import { Link } from 'react-router-dom';
import { Logo } from './ui/Logo';

/**
 * The fixed, blurred top nav for the public app-host pages. Mirrors the
 * marketing site's nav: `h-16`, backdrop blur, light bottom border, the
 * wordmark on the left and a sign-in + get-started pairing on the right.
 */
export function SiteNav() {
  return (
    <nav className="fixed top-0 left-0 right-0 z-50 backdrop-blur-md bg-bg-primary/80 border-b border-border-light">
      <div className="max-w-6xl mx-auto px-6 h-16 flex items-center justify-between">
        <Link
          to="/"
          className="flex items-center rounded-md focus-visible:outline-2"
          aria-label="Atomic Cloud home"
        >
          <Logo className="h-6" />
          <span className="ml-2.5 hidden sm:inline text-sm font-medium text-text-muted">
            Cloud
          </span>
        </Link>

        <div className="flex items-center gap-2 sm:gap-4">
          <Link
            to="/login"
            className="px-3 py-2 text-sm font-medium text-text-secondary hover:text-text-primary transition-colors rounded-lg focus-visible:outline-2"
          >
            Sign in
          </Link>
          <Link
            to="/signup"
            className="inline-flex items-center px-4 py-2 text-sm font-medium text-white bg-accent hover:bg-accent-dark rounded-lg transition-colors focus-visible:outline-2"
          >
            Get started
          </Link>
        </div>
      </div>
    </nav>
  );
}
