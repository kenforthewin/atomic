import { Fragment, type ReactNode } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { CitationLink } from '../../wiki/CitationLink';
import type { FindingCitation } from '../../../stores/featuredReport';

interface BriefingContentProps {
  content: string;
  citations: FindingCitation[];
  onCitationClick: (citation: FindingCitation, element: HTMLElement) => void;
}

/**
 * Renders briefing markdown with inline `[N]` citation markers replaced by
 * clickable CitationLink buttons. Intentionally simpler than WikiArticleContent:
 * no wiki-links, no search highlighting, no related tags.
 */
export function BriefingContent({ content, citations, onCitationClick }: BriefingContentProps) {
  const citationMap = new Map(citations.map(c => [c.citation_index, c]));

  const processTextWithCitations = (text: string): (string | JSX.Element)[] => {
    const parts = text.split(/(\[\d+\])/g);
    return parts.map((part, i) => {
      const match = part.match(/^\[(\d+)\]$/);
      if (!match) return part;
      const index = parseInt(match[1], 10);
      const citation = citationMap.get(index);
      if (!citation) return part;
      return (
        <CitationLink
          key={`c-${i}-${index}`}
          index={index}
          onClick={(e) => onCitationClick(citation, e.currentTarget)}
        />
      );
    });
  };

  const processChildren = (children: ReactNode): ReactNode => {
    if (typeof children === 'string') {
      return processTextWithCitations(children).map((part, i) =>
        typeof part === 'string' ? <Fragment key={`t-${i}`}>{part}</Fragment> : part
      );
    }
    if (Array.isArray(children)) {
      return children.map((child, i) => <Fragment key={i}>{processChildren(child)}</Fragment>);
    }
    return children;
  };

  const components = {
    p: ({ children }: { children?: ReactNode }) => <p>{processChildren(children)}</p>,
    li: ({ children }: { children?: ReactNode }) => <li>{processChildren(children)}</li>,
    strong: ({ children }: { children?: ReactNode }) => <strong>{processChildren(children)}</strong>,
    em: ({ children }: { children?: ReactNode }) => <em>{processChildren(children)}</em>,
  };

  return (
    <div className="prose prose-invert prose-sm md:prose-base max-w-none text-[var(--color-text-secondary)] [&_p]:leading-relaxed [&_p]:my-3 first:[&_p]:mt-0 last:[&_p]:mb-0">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {content}
      </ReactMarkdown>
    </div>
  );
}
