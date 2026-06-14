import { Link } from 'react-router-dom';
import { PublicLayout } from '../layouts/PublicLayout';
import { NodeGraphBackdrop } from '../components/NodeGraphBackdrop';

export function NotFound() {
  return (
    <PublicLayout>
      <section className="relative overflow-hidden">
        <NodeGraphBackdrop />
        <div className="relative max-w-xl mx-auto px-6 py-24 md:py-32 text-center">
          <p className="text-xs font-medium tracking-wide uppercase text-text-muted mb-3">
            404
          </p>
          <h1 className="font-display text-4xl md:text-5xl tracking-tight mb-4">
            This page <span className="italic">drifted off.</span>
          </h1>
          <p className="text-text-secondary leading-relaxed mb-8">
            The link you followed doesn't lead anywhere here.
          </p>
          <Link
            to="/"
            className="inline-flex items-center px-7 py-3.5 text-base font-medium text-white bg-accent hover:bg-accent-dark rounded-xl transition-all hover:shadow-lg hover:shadow-accent/20 focus-visible:outline-2"
          >
            Back home
          </Link>
        </div>
      </section>
    </PublicLayout>
  );
}
