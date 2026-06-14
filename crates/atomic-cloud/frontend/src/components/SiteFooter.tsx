import { Logo } from './ui/Logo';

const PRODUCT_URL = 'https://atomicapp.ai';
const GITHUB_URL = 'https://github.com/kenforthewin/atomic';

/**
 * The public footer, echoing the marketing site: the wordmark, a one-line
 * value statement, a small set of resource links pointing back at the product
 * site / repo, and a copyright line.
 */
export function SiteFooter() {
  return (
    <footer className="border-t border-border-light bg-bg-secondary">
      <div className="max-w-6xl mx-auto px-6 py-12">
        <div className="grid grid-cols-1 md:grid-cols-3 gap-8">
          <div>
            <div className="mb-4">
              <Logo className="h-5" />
            </div>
            <p className="text-sm text-text-muted leading-relaxed max-w-xs">
              Your ideas, semantically connected — now hosted and run for you.
            </p>
          </div>
          <div>
            <h4 className="text-sm font-medium mb-3">Product</h4>
            <ul className="space-y-2 text-sm text-text-muted">
              <li>
                <a
                  href={PRODUCT_URL}
                  className="hover:text-text-primary transition-colors"
                >
                  Atomic
                </a>
              </li>
              <li>
                <a
                  href={`${PRODUCT_URL}/getting-started/self-hosting/`}
                  className="hover:text-text-primary transition-colors"
                >
                  Self-Host
                </a>
              </li>
            </ul>
          </div>
          <div>
            <h4 className="text-sm font-medium mb-3">Resources</h4>
            <ul className="space-y-2 text-sm text-text-muted">
              <li>
                <a
                  href={`${PRODUCT_URL}/getting-started/`}
                  className="hover:text-text-primary transition-colors"
                >
                  Documentation
                </a>
              </li>
              <li>
                <a
                  href={GITHUB_URL}
                  className="hover:text-text-primary transition-colors"
                >
                  GitHub
                </a>
              </li>
            </ul>
          </div>
        </div>
        <div className="mt-10 pt-6 border-t border-border-light text-sm text-text-muted">
          &copy; {new Date().getFullYear()} Atomic
        </div>
      </div>
    </footer>
  );
}
