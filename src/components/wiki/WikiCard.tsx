import { memo } from 'react';
import { Sparkles, BookOpen, Link2 } from 'lucide-react';
import { WikiArticleSummary, SuggestedArticle } from '../../stores/wiki';
import { formatRelativeDate, formatShortRelativeDate } from '../../lib/date';

interface WikiArticleCardProps {
  type: 'article';
  article: WikiArticleSummary;
  onClick: (opts?: { newTab?: boolean }) => void;
}

interface WikiSuggestionCardProps {
  type: 'suggestion';
  suggestion: SuggestedArticle;
  onClick: (opts?: { newTab?: boolean }) => void;
}

type WikiCardProps = WikiArticleCardProps | WikiSuggestionCardProps;

export const WikiCard = memo(function WikiCard(props: WikiCardProps) {
  if (props.type === 'suggestion') {
    const { suggestion, onClick } = props;
    return (
      <div
        onClick={(e) => onClick({ newTab: e.metaKey || e.ctrlKey })}
        onAuxClick={(e) => {
          if (e.button === 1) {
            e.preventDefault();
            onClick({ newTab: true });
          }
        }}
        className="relative flex flex-col p-4 border border-dashed border-[var(--color-border)] rounded-lg cursor-pointer hover:border-[var(--color-accent)]/50 hover:bg-[var(--color-bg-card)]/50 transition-all duration-150 h-full min-w-0 overflow-hidden group"
      >
        <div className="flex-1 min-h-0">
          <div className="flex items-start justify-between gap-2">
            <p className="text-sm font-medium text-[var(--color-text-primary)] line-clamp-1">
              {suggestion.tag_name}
            </p>
            <Sparkles className="w-4 h-4 text-[var(--color-accent)] shrink-0 opacity-60 group-hover:opacity-100 transition-opacity" strokeWidth={2} />
          </div>
          <p className="text-xs text-[var(--color-text-tertiary)] mt-2 leading-relaxed">
            Generate a wiki article from {suggestion.atom_count} atom{suggestion.atom_count !== 1 ? 's' : ''}
            {suggestion.mention_count > 0 && (
              <> and {suggestion.mention_count} mention{suggestion.mention_count !== 1 ? 's' : ''}</>
            )}
          </p>
        </div>
        <div className="mt-3 pt-3 border-t border-dashed border-[var(--color-border)]">
          <span className="text-xs font-medium text-[var(--color-accent)] group-hover:text-[var(--color-accent-light)] transition-colors">
            Generate article
          </span>
        </div>
      </div>
    );
  }

  const { article, onClick } = props;

  return (
    <div
      onClick={(e) => onClick({ newTab: e.metaKey || e.ctrlKey })}
      onAuxClick={(e) => {
        if (e.button === 1) {
          e.preventDefault();
          onClick({ newTab: true });
        }
      }}
      className="relative flex flex-col p-4 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg cursor-pointer hover:border-[var(--color-border-hover)] hover:bg-[var(--color-bg-hover)] transition-all duration-150 h-full min-w-0 overflow-hidden break-words"
    >
      <div className="flex-1 min-h-0">
        <div className="flex items-baseline justify-between gap-2">
          <p className="text-sm font-medium text-[var(--color-text-primary)] line-clamp-1 min-w-0">
            {article.tag_name}
          </p>
          <span className="text-xs text-[var(--color-text-tertiary)] shrink-0" title={formatRelativeDate(article.updated_at)}>
            {formatShortRelativeDate(article.updated_at)}
          </span>
        </div>
      </div>
      <div className="mt-3 pt-3 border-t border-[var(--color-border)]">
        <div className="flex items-center gap-3">
          <span className="flex items-center gap-1 text-xs text-[var(--color-text-tertiary)]">
            <BookOpen className="w-3.5 h-3.5" strokeWidth={2} />
            {article.atom_count} source{article.atom_count !== 1 ? 's' : ''}
          </span>
          {article.inbound_links > 0 && (
            <span className="flex items-center gap-1 text-xs text-[var(--color-accent-light)]">
              <Link2 className="w-3.5 h-3.5" strokeWidth={2} />
              {article.inbound_links} link{article.inbound_links !== 1 ? 's' : ''}
            </span>
          )}
        </div>
      </div>
    </div>
  );
}, (prev, next) => {
  if (prev.type !== next.type) return false;
  if (prev.type === 'article' && next.type === 'article') {
    return prev.article.id === next.article.id
      && prev.article.updated_at === next.article.updated_at
      && prev.article.atom_count === next.article.atom_count
      && prev.article.inbound_links === next.article.inbound_links
      && prev.article.new_atoms_available === next.article.new_atoms_available;
  }
  if (prev.type === 'suggestion' && next.type === 'suggestion') {
    return prev.suggestion.tag_id === next.suggestion.tag_id
      && prev.suggestion.atom_count === next.suggestion.atom_count;
  }
  return false;
});
