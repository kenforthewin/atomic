import { memo } from 'react';
import { DisplayAtom } from '../../stores/atoms';
import { TagChip } from '../tags/TagChip';
import { formatRelativeDate, formatShortRelativeDate } from '../../lib/date';

/** Get display source — prefer pre-parsed `source` field, fall back to extracting from URL */
function getDisplaySource(atom: DisplayAtom): string | null {
  if (atom.source) return atom.source;
  if (atom.source_url) {
    try {
      const hostname = new URL(atom.source_url).hostname;
      return hostname.replace(/^www\./, '');
    } catch {
      return atom.source_url;
    }
  }
  return null;
}

/** Display date matches the default sort order (updated_at desc) */
function getDisplayDate(atom: DisplayAtom): string {
  return atom.updated_at;
}

interface AtomCardProps {
  atom: DisplayAtom;
  onAtomClick: (atomId: string) => void;
  viewMode: 'grid' | 'list';
  matchingChunkContent?: string;  // For search results
  onRetryEmbedding?: (atomId: string) => void;  // For retry action
  onRetryTagging?: (atomId: string) => void;  // For tagging retry action
}

function ProcessingStatusIndicator({
  embeddingStatus,
  taggingStatus,
  onRetry,
  onRetryTagging,
}: {
  embeddingStatus: DisplayAtom['embedding_status'];
  taggingStatus: DisplayAtom['tagging_status'];
  onRetry?: () => void;
  onRetryTagging?: () => void;
}) {
  // Show failed state if embedding failed
  if (embeddingStatus === 'failed') {
    return (
      <button
        onClick={(e) => {
          e.stopPropagation();
          onRetry?.();
        }}
        className="absolute top-2 right-2 text-red-500 hover:text-red-400 transition-colors"
        title="Embedding failed - click to retry"
      >
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path
            strokeLinecap="round"
            strokeLinejoin="round"
            strokeWidth={2}
            d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z"
          />
        </svg>
      </button>
    );
  }

  // Determine if still processing (either embedding or tagging not complete)
  const isEmbedding = embeddingStatus === 'pending' || embeddingStatus === 'processing';
  const isTagging = taggingStatus === 'pending' || taggingStatus === 'processing';
  const taggingFailed = taggingStatus === 'failed';

  // Both complete (or tagging skipped) - no indicator needed
  if (embeddingStatus === 'complete' && (taggingStatus === 'complete' || taggingStatus === 'skipped')) {
    return null;
  }

  // Show amber indicator for pending/processing states
  if (isEmbedding || isTagging) {
    let title = 'Processing...';
    if (isEmbedding) {
      title = embeddingStatus === 'pending' ? 'Embedding pending' : 'Embedding in progress';
    } else if (isTagging) {
      title = taggingStatus === 'pending' ? 'Tag extraction pending' : 'Tag extraction in progress';
    }

    return (
      <div
        className="absolute top-2 right-2 w-2.5 h-2.5 bg-amber-500 rounded-full animate-pulse"
        title={title}
      />
    );
  }

  // Tagging failed (but embedding succeeded) - show retry button
  if (taggingFailed) {
    return (
      <button
        onClick={(e) => {
          e.stopPropagation();
          onRetryTagging?.();
        }}
        className="absolute top-2 right-2 text-orange-500 hover:text-orange-400 transition-colors"
        title="Tag extraction failed - click to retry"
      >
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path
            strokeLinecap="round"
            strokeLinejoin="round"
            strokeWidth={2}
            d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15"
          />
        </svg>
      </button>
    );
  }

  return null;
}

export const AtomCard = memo(function AtomCard({
  atom,
  onAtomClick,
  viewMode,
  matchingChunkContent,
  onRetryEmbedding,
  onRetryTagging,
}: AtomCardProps) {
  const handleClick = () => onAtomClick(atom.id);
  const handleRetry = onRetryEmbedding ? () => onRetryEmbedding(atom.id) : undefined;
  const handleRetryTagging = onRetryTagging ? () => onRetryTagging(atom.id) : undefined;

  const { title, snippet } = atom;

  const displaySource = getDisplaySource(atom);
  const hasSource = !!displaySource;
  const maxVisibleTags = viewMode === 'grid' ? (hasSource ? 1 : 2) : 3;
  const visibleTags = atom.tags.slice(0, maxVisibleTags);
  const remainingTags = atom.tags.length - maxVisibleTags;

  if (viewMode === 'list') {
    return (
      <div
        onClick={handleClick}
        className="relative flex items-center gap-3 px-3 py-2.5 min-h-[80px] bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg cursor-pointer hover:border-[var(--color-border-hover)] hover:bg-[var(--color-bg-hover)] transition-all duration-150"
      >
        <ProcessingStatusIndicator
          embeddingStatus={atom.embedding_status}
          taggingStatus={atom.tagging_status}
          onRetry={handleRetry}
          onRetryTagging={handleRetryTagging}
        />
        <div className="flex-1 min-w-0">
          <div className="flex items-baseline gap-2">
            <span
              className={`text-sm font-medium shrink-0 ${
                matchingChunkContent ? 'text-[var(--color-accent-light)]' : 'text-[var(--color-text-primary)]'
              }`}
            >
              {title || 'Untitled'}
            </span>
            {snippet && (
              <span className="text-xs text-[var(--color-text-tertiary)] truncate">
                {snippet}
              </span>
            )}
          </div>
          {atom.tags.length > 0 && (
            <div className="flex items-center gap-1.5 mt-1">
              {visibleTags.map((tag) => (
                <TagChip key={tag.id} name={tag.name} size="sm" />
              ))}
              {remainingTags > 0 && (
                <span className="text-xs text-[var(--color-text-tertiary)]">+{remainingTags}</span>
              )}
            </div>
          )}
        </div>
        <div className="flex items-center gap-2 shrink-0">
          {displaySource && (
            <span className="text-xs text-[var(--color-text-tertiary)] bg-[var(--color-bg-panel)] px-1.5 py-0.5 rounded truncate max-w-[120px]" title={atom.source_url ?? displaySource}>
              {displaySource}
            </span>
          )}
          <span className="text-xs text-[var(--color-text-tertiary)] whitespace-nowrap">
            {formatRelativeDate(getDisplayDate(atom))}
          </span>
        </div>
      </div>
    );
  }

  return (
    <div
      onClick={handleClick}
      className="relative flex flex-col p-4 bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg cursor-pointer hover:border-[var(--color-border-hover)] hover:bg-[var(--color-bg-hover)] transition-all duration-150 h-full min-h-[80px] min-w-0 overflow-hidden break-words"
    >
      <ProcessingStatusIndicator
        embeddingStatus={atom.embedding_status}
        taggingStatus={atom.tagging_status}
        onRetry={handleRetry}
        onRetryTagging={handleRetryTagging}
      />
      <div className="flex-1 min-h-0 overflow-hidden">
        <div className="flex items-baseline justify-between gap-2">
          <p
            className={`text-sm font-medium line-clamp-1 min-w-0 ${
              matchingChunkContent ? 'text-[var(--color-accent-light)]' : 'text-[var(--color-text-primary)]'
            }`}
          >
            {title || 'Untitled'}
          </p>
          <span className="text-xs text-[var(--color-text-tertiary)] shrink-0" title={formatRelativeDate(getDisplayDate(atom))}>
            {formatShortRelativeDate(getDisplayDate(atom))}
          </span>
        </div>
        {snippet && (
          <p className="text-sm text-[var(--color-text-secondary)] mt-1 leading-relaxed line-clamp-4">
            {snippet}
          </p>
        )}
      </div>
      <div className="mt-3 pt-3 border-t border-[var(--color-border)]">
        <div className="flex items-center gap-1.5">
          {visibleTags.map((tag) => (
            <TagChip key={tag.id} name={tag.name} size="sm" />
          ))}
          {remainingTags > 0 && (
            <span className="text-xs text-[var(--color-text-tertiary)] shrink-0">+{remainingTags}</span>
          )}
          {displaySource && (
            <span className="ml-auto text-xs text-[var(--color-text-tertiary)] bg-[var(--color-bg-panel)] px-1.5 py-0.5 rounded truncate max-w-[120px] shrink-0" title={atom.source_url ?? displaySource}>
              {displaySource}
            </span>
          )}
        </div>
      </div>
    </div>
  );
}, (prev, next) => {
  return prev.atom.id === next.atom.id
    && prev.atom.updated_at === next.atom.updated_at
    && prev.atom.embedding_status === next.atom.embedding_status
    && prev.atom.tagging_status === next.atom.tagging_status
    && prev.atom.tags.length === next.atom.tags.length
    && prev.atom.source === next.atom.source
    && prev.viewMode === next.viewMode
    && prev.matchingChunkContent === next.matchingChunkContent;
});
