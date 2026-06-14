import { Link } from 'react-router-dom';
import { Search, BookOpen, MessagesSquare } from 'lucide-react';
import { PublicLayout } from '../layouts/PublicLayout';
import { NodeGraphBackdrop } from '../components/NodeGraphBackdrop';
import { Card, CardIcon } from '../components/ui/Card';

const HIGHLIGHTS = [
  {
    Icon: Search,
    title: 'Semantic search',
    body: 'Find a note by what you meant, not the words you typed.',
  },
  {
    Icon: BookOpen,
    title: 'Wiki synthesis',
    body: 'Pick a tag, get an article — with citations back to the source.',
  },
  {
    Icon: MessagesSquare,
    title: 'Agentic chat',
    body: 'Chat with your notes. It cites them; it never invents them.',
  },
];

/**
 * The account front door — a focused, on-brand landing. Not the full marketing
 * site (that lives at atomicapp.ai); this is the hosted-account entry point
 * with a single clear pair of CTAs.
 */
export function Landing() {
  return (
    <PublicLayout>
      <section className="relative overflow-hidden">
        <NodeGraphBackdrop />
        <div className="relative max-w-4xl mx-auto px-6 pt-16 pb-20 md:pt-24 md:pb-28 text-center">
          <div className="fade-in-up">
            <p className="text-xs font-medium tracking-wide uppercase text-text-muted mb-5">
              Atomic Cloud
            </p>
            <h1 className="font-display text-5xl md:text-6xl lg:text-7xl leading-[1.05] tracking-tight text-balance mb-6">
              Your knowledge graph,{' '}
              <span className="italic">hosted and run for you.</span>
            </h1>
            <p className="text-lg md:text-xl text-text-secondary leading-relaxed max-w-2xl mx-auto mb-10">
              The same AI-native Atomic — semantic search, wiki synthesis, and
              agentic chat — without the server to keep alive. Sign up, get a
              private subdomain, and start writing.
            </p>
            <div className="flex flex-col sm:flex-row items-center justify-center gap-4">
              <Link
                to="/signup"
                className="inline-flex items-center justify-center gap-3 px-7 py-3.5 text-base font-medium text-white bg-accent hover:bg-accent-dark rounded-xl transition-all hover:shadow-lg hover:shadow-accent/20 focus-visible:outline-2"
              >
                Get started
              </Link>
              <Link
                to="/login"
                className="inline-flex items-center justify-center gap-3 px-7 py-3.5 text-base font-medium text-text-primary bg-bg-white border border-border rounded-xl hover:border-accent/30 hover:bg-accent-subtle/50 transition-all focus-visible:outline-2"
              >
                Sign in
              </Link>
            </div>
            <p className="mt-5 text-xs text-text-muted">
              14-day trial of the paid tier. No card required.
            </p>
          </div>
        </div>
      </section>

      <section className="py-16 md:py-20 bg-bg-secondary">
        <div className="max-w-6xl mx-auto px-6">
          <div className="grid grid-cols-1 md:grid-cols-3 gap-6">
            {HIGHLIGHTS.map(({ Icon, title, body }) => (
              <Card key={title} interactive>
                <CardIcon className="mb-4">
                  <Icon className="w-5 h-5" strokeWidth={1.5} aria-hidden="true" />
                </CardIcon>
                <h2 className="font-medium text-lg mb-2">{title}</h2>
                <p className="text-sm text-text-secondary leading-relaxed">
                  {body}
                </p>
              </Card>
            ))}
          </div>
        </div>
      </section>
    </PublicLayout>
  );
}
