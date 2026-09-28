import { useState, useEffect } from 'react';
import { ChevronDown } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import { SimilarAtomResult } from '../../stores/atoms';
import { MiniGraphPreview } from '../canvas/MiniGraphPreview';

// Benchmarking helper
const PERF_DEBUG = import.meta.env.DEV;
const perfLog = (label: string, startTime?: number) => {
  if (!PERF_DEBUG) return;
  if (startTime !== undefined) {
    console.log(`[RelatedAtoms] ${label}: ${(performance.now() - startTime).toFixed(2)}ms`);
  } else {
    console.log(`[RelatedAtoms] ${label}`);
  }
};

interface RelatedAtomsProps {
  atomId: string;
  onAtomClick: (atomId: string, opts?: { newTab?: boolean }) => void;
  onViewGraph?: (opts?: { newTab?: boolean }) => void;
}

export function RelatedAtoms({ atomId, onAtomClick, onViewGraph }: RelatedAtomsProps) {
  const [relatedAtoms, setRelatedAtoms] = useState<SimilarAtomResult[]>([]);
  const [isLoading, setIsLoading] = useState(false);
  const [isCollapsed, setIsCollapsed] = useState(true);
  const [hasLoaded, setHasLoaded] = useState(false);

  useEffect(() => {
    // Only fetch when expanded and not yet loaded
    if (!isCollapsed && !hasLoaded) {
      const fetchRelated = async () => {
        const fetchStart = performance.now();
        perfLog('Fetch similar atoms START');
        setIsLoading(true);
        try {
          const results = await getTransport().invoke<SimilarAtomResult[]>('find_similar_atoms', {
            atomId,
            limit: 5,
            threshold: 0.7,
          });
          perfLog(`Fetch similar atoms COMPLETE (found ${results.length})`, fetchStart);
          setRelatedAtoms(results);
          setHasLoaded(true);
        } catch (error) {
          console.error('Failed to fetch related atoms:', error);
          perfLog('Fetch similar atoms FAILED', fetchStart);
          setRelatedAtoms([]);
        } finally {
          setIsLoading(false);
        }
      };

      fetchRelated();
    }
  }, [atomId, isCollapsed, hasLoaded]);

  return (
    <div className="border-t border-[var(--color-border)] px-6 py-4">
      <button
        onClick={() => setIsCollapsed(!isCollapsed)}
        className="flex items-center justify-between w-full text-sm font-medium text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
      >
        <div className="flex items-center gap-2">
          <span>Related & Neighborhood</span>
          {hasLoaded && relatedAtoms.length > 0 && (
            <span className="text-xs text-[var(--color-text-tertiary)] bg-[var(--color-bg-card)] px-2 py-0.5 rounded">
              {relatedAtoms.length}
            </span>
          )}
        </div>
        <ChevronDown
          className={`w-4 h-4 transition-transform ${isCollapsed ? '' : 'rotate-180'}`}
          strokeWidth={2}
        />
      </button>

      {!isCollapsed && (
        <div className="mt-3 space-y-4">
          {/* Related Atoms List */}
          {isLoading ? (
            <div className="text-sm text-[var(--color-text-tertiary)]">Loading...</div>
          ) : relatedAtoms.length > 0 ? (
            <div className="space-y-2">
              <div className="text-xs text-[var(--color-text-tertiary)] uppercase tracking-wide">Similar atoms</div>
              {relatedAtoms.map((result) => (
                <button
                  key={result.id}
                  onClick={(e) => onAtomClick(result.id, { newTab: e.metaKey || e.ctrlKey })}
                  onAuxClick={(e) => {
                    if (e.button === 1) {
                      e.preventDefault();
                      onAtomClick(result.id, { newTab: true });
                    }
                  }}
                  className="w-full text-left p-3 bg-[var(--color-bg-panel)] rounded-md hover:bg-[var(--color-bg-card)] transition-colors"
                >
                  <p className="text-sm text-[var(--color-text-primary)] line-clamp-2">
                    {result.content.length > 100
                      ? result.content.slice(0, 100) + '...'
                      : result.content}
                  </p>
                  <div className="flex items-center gap-2 mt-2">
                    <span className="text-xs text-[var(--color-accent)]">
                      {Math.round(result.similarity_score * 100)}% similar
                    </span>
                  </div>
                </button>
              ))}
            </div>
          ) : hasLoaded ? (
            <div className="text-sm text-[var(--color-text-tertiary)]">No similar atoms found</div>
          ) : null}

          {/* Mini Graph Preview */}
          <div>
            <div className="text-xs text-[var(--color-text-tertiary)] uppercase tracking-wide mb-2">Neighborhood graph</div>
            <MiniGraphPreview atomId={atomId} onExpand={onViewGraph} />
          </div>
        </div>
      )}
    </div>
  );
}
