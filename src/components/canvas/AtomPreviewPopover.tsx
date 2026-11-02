import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, memo } from 'react';
import { ArrowRight, ChevronDown, ChevronUp, ExternalLink, Loader2, X } from 'lucide-react';
import { createPortal } from 'react-dom';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { useKeyboard } from '../../hooks/useKeyboard';
import { getTransport } from '../../lib/transport';
import { chunkMarkdown } from '../../lib/markdown';
import type { AtomWithTags } from '../../stores/atoms';
import { TagChip } from '../tags/TagChip';
import { formatDate } from '../../lib/date';
import { accountNetworkCategory } from './accountNetworkCategories';
import { parseCanvasImage, parseCompanyProfile, parseExecutiveProfile, type CompanyProfile, type ExecutiveProfile } from './accountNetworkProfile';

interface AtomPreviewPopoverProps {
  atomId: string;
  anchorRect: { top: number; left: number; bottom: number; width: number };
  onClose: () => void;
  onViewAtom: (atomId: string, opts?: { newTab?: boolean }) => void;
}

const POPOVER_WIDTH = 480;
const POPOVER_MAX_HEIGHT = 360;
const CHUNK_SIZE = 4000;
const INITIAL_CHUNKS = 1;
const CHUNKS_PER_BATCH = 2;

const remarkPluginsStable = [remarkGfm];

const MemoizedChunk = memo(function MemoizedChunk({ content }: { content: string }) {
  return (
    <ReactMarkdown
      remarkPlugins={remarkPluginsStable}
      components={{ a: ({ children, ...props }) => <a {...props} target="_blank" rel="noopener noreferrer">{children}</a> }}
    >
      {content}
    </ReactMarkdown>
  );
});

function SourceLink({ href, label = 'Source' }: { href: string; label?: string }) {
  return <a href={href} target="_blank" rel="noopener noreferrer" className="text-[11px] text-[var(--color-text-tertiary)] hover:text-[var(--color-accent)] underline underline-offset-2">{label}</a>;
}

function CompanyProfileCard({ profile, name }: { profile: CompanyProfile; name: string }) {
  const host = new URL(profile.website).hostname.replace(/^www\./, '');
  return (
    <section data-testid="company-profile" aria-label="Company profile" className="px-4 py-4 border-b border-[var(--color-border)]">
      <div className="flex items-center gap-4">
        {profile.logo && (
          <div className="w-[176px] h-14 shrink-0 rounded-md border border-[var(--color-border)] bg-[var(--color-bg-main)] flex items-center justify-center px-2">
            <img src={profile.logo.url} alt={`${name} logo`} className="max-w-full max-h-10 object-contain" />
          </div>
        )}
        <div className="min-w-0">
          <h4 className="text-xs font-semibold text-[var(--color-text-primary)]">Company profile</h4>
          <p className="text-[11px] leading-4 text-[var(--color-text-tertiary)] mt-1">Public company information{profile.checkedAt ? ` · checked ${profile.checkedAt}` : ''}</p>
          {profile.logo && <SourceLink href={profile.logo.source} label="Logo source" />}
        </div>
      </div>
      <dl className="grid grid-cols-2 gap-x-5 gap-y-3 mt-4 text-xs">
        <div className="min-w-0">
          <dt className="text-[11px] text-[var(--color-text-tertiary)]">Website</dt>
          <dd className="mt-0.5 truncate"><a href={profile.website} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-1 text-[var(--color-accent)] hover:underline">{host}<ExternalLink className="w-3 h-3 shrink-0" /></a></dd>
        </div>
        <div className="min-w-0">
          <dt className="text-[11px] text-[var(--color-text-tertiary)]">LinkedIn</dt>
          <dd className="mt-0.5">{profile.linkedIn ? <a href={profile.linkedIn} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-1 text-[var(--color-accent)] hover:underline">Company page<ExternalLink className="w-3 h-3" /></a> : <span className="text-[var(--color-text-tertiary)]">Not recorded</span>}</dd>
        </div>
        <div className="min-w-0">
          <dt className="text-[11px] text-[var(--color-text-tertiary)]">Annual originations</dt>
          <dd className="mt-0.5 text-[var(--color-text-primary)] leading-4">{profile.annualOriginations?.value ?? 'Not recorded'}</dd>
          {profile.annualOriginations && <SourceLink href={profile.annualOriginations.source} />}
        </div>
        <div className="min-w-0">
          <dt className="text-[11px] text-[var(--color-text-tertiary)]">Headcount</dt>
          <dd className="mt-0.5 text-[var(--color-text-primary)] leading-4">{profile.headcount?.value ?? 'Not recorded'}</dd>
          {profile.headcount && <SourceLink href={profile.headcount.source} />}
        </div>
        <div className="min-w-0">
          <dt className="text-[11px] text-[var(--color-text-tertiary)]">HQ</dt>
          <dd className="mt-0.5 text-[var(--color-text-primary)]">{profile.hq?.value ?? 'Not recorded'}</dd>
          {profile.hq && <SourceLink href={profile.hq.source} />}
        </div>
      </dl>
    </section>
  );
}

function ExecutiveProfileCard({ profile }: { profile: ExecutiveProfile }) {
  const linkedInDisplay = profile.linkedIn
    ? new URL(profile.linkedIn).hostname + new URL(profile.linkedIn).pathname.replace(/\/$/, '')
    : null;

  return (
    <section data-testid="executive-profile" aria-label="Executive profile" className="px-4 py-3 border-b border-[var(--color-border)]">
      <p className="text-sm text-[var(--color-text-primary)]">{profile.title}</p>
      <div className="mt-1"><SourceLink href={profile.officialBio} label="Official bio" /></div>

      <dl className="mt-2 divide-y divide-[var(--color-border)]">
        <div className="grid grid-cols-[104px_minmax(0,1fr)] gap-3 py-2 text-xs leading-5">
          <dt className="text-[var(--color-text-tertiary)]">LinkedIn URL</dt>
          <dd className="min-w-0 truncate">{profile.linkedIn ? <a href={profile.linkedIn} target="_blank" rel="noopener noreferrer" className="text-[var(--color-accent)] hover:underline" title={profile.linkedIn}>{linkedInDisplay}</a> : 'Not verified'}</dd>
        </div>
        <div className="grid grid-cols-[104px_minmax(0,1fr)] gap-3 py-2 text-xs leading-5">
          <dt className="text-[var(--color-text-tertiary)]">College</dt>
          <dd className="text-[var(--color-text-primary)]">
            <div><span className="text-[var(--color-text-tertiary)]">Undergrad</span> · {profile.college.undergraduate.join(' · ') || 'Not verified'}</div>
            <div><span className="text-[var(--color-text-tertiary)]">Graduate</span> · {profile.college.graduate.join(' · ') || 'Not verified'}</div>
            {profile.college.other.length > 0 && <div><span className="text-[var(--color-text-tertiary)]">Level unknown</span> · {profile.college.other.join(' · ')}</div>}
          </dd>
        </div>
        <div className="grid grid-cols-[104px_minmax(0,1fr)] gap-3 py-2 text-xs leading-5">
          <dt className="text-[var(--color-text-tertiary)]">Associations</dt>
          <dd className="text-[var(--color-text-primary)]">{profile.affiliations.join(' · ') || 'Not documented'}</dd>
        </div>
        {([
          ['LinkedIn About', profile.linkedInAbout],
          ['Email', profile.email],
          ['Phone', profile.phone],
        ] as const).map(([label, value]) => (
          <div key={label} className="grid grid-cols-[104px_minmax(0,1fr)] gap-3 py-2 text-xs leading-5">
            <dt className="text-[var(--color-text-tertiary)]">{label}</dt>
            <dd className="text-[var(--color-text-primary)]">{value}</dd>
          </div>
        ))}
      </dl>
      <div className="mt-2 px-3 py-2 rounded-md bg-[var(--color-bg-main)]">
        <p className="text-[11px] text-[var(--color-text-tertiary)]">Mutuals with Mark</p>
        <p className="mt-0.5 text-xs leading-5 text-[var(--color-text-primary)]">{profile.mutuals}</p>
        {profile.linkedInMutualsEvidence && <p className="text-[11px] leading-4 text-[var(--color-text-tertiary)]">{profile.linkedInMutualsEvidence}</p>}
        <p className="text-[11px] leading-4 text-[var(--color-text-tertiary)]">
          {profile.networkCheck ? <>{profile.linkedInMutualsEvidence ? 'Happenstance found no named mutual in connected sources' : 'Happenstance check'} · {profile.networkCheck.checkedAt} · <SourceLink href={profile.networkCheck.source} label="Source" /></> : 'Network check pending'}
          {' · School, work, and community overlap unverified'}
        </p>
      </div>
      {profile.priorWork.length > 0 && <p className="mt-3 text-xs leading-5 text-[var(--color-text-primary)]"><span className="text-[var(--color-text-tertiary)]">Prior work</span> · {profile.priorWork.join(' · ')}</p>}
    </section>
  );
}

function calculatePosition(
  anchorRect: { top: number; left: number; bottom: number; width: number },
  popoverHeight: number,
  popoverWidth: number
): { top: number; left: number } {
  const spaceBelow = window.innerHeight - anchorRect.bottom;
  const spaceAbove = anchorRect.top;

  let top: number;
  if (spaceBelow >= popoverHeight + 12 || spaceBelow >= spaceAbove) {
    top = anchorRect.bottom + 12;
  } else {
    top = anchorRect.top - popoverHeight - 12;
  }

  let left = anchorRect.left + anchorRect.width / 2 - popoverWidth / 2;
  left = Math.max(8, Math.min(left, window.innerWidth - popoverWidth - 8));

  top = Math.max(8, Math.min(top, window.innerHeight - popoverHeight - 8));

  return { top, left };
}

export function AtomPreviewPopover({ atomId, anchorRect, onClose, onViewAtom }: AtomPreviewPopoverProps) {
  const popoverRef = useRef<HTMLDivElement>(null);
  const [atom, setAtom] = useState<AtomWithTags | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [isCollapsed, setIsCollapsed] = useState(false);
  const [notesOpen, setNotesOpen] = useState(false);
  const category = accountNetworkCategory(atom?.source_url);
  const companyProfile = useMemo(() => category?.key === 'account' && atom ? parseCompanyProfile(atom.content) : null, [atom, category?.key]);
  const executiveProfile = useMemo(() => category?.key === 'executive' && atom ? parseExecutiveProfile(atom.content) : null, [atom, category?.key]);
  const nodeImage = useMemo(() => category && category.key !== 'account' && atom ? parseCanvasImage(atom.content) : undefined, [atom, category]);
  const hasStructuredProfile = !!(companyProfile || executiveProfile);

  const initialPosition = calculatePosition(anchorRect, POPOVER_MAX_HEIGHT, POPOVER_WIDTH);
  const [position, setPosition] = useState(initialPosition);
  // Once the user drags, stop re-anchoring to the node.
  const hasBeenDraggedRef = useRef(false);
  // When the atom switches (user clicked a different node), allow re-anchoring once.
  useEffect(() => { hasBeenDraggedRef.current = false; }, [atomId]);
  useEffect(() => { setNotesOpen(false); }, [atomId]);

  useKeyboard('Escape', onClose, true);

  // Click outside dismisses the popover, but a canvas pan (mousedown-drag-mouseup
  // outside) must NOT dismiss it. We treat it as a click only when the cursor
  // moved less than a few px between down and up.
  useEffect(() => {
    let downX = 0;
    let downY = 0;
    let downOutside = false;
    const handleDown = (e: MouseEvent) => {
      downOutside = !!popoverRef.current && !popoverRef.current.contains(e.target as Node);
      downX = e.clientX;
      downY = e.clientY;
    };
    const handleUp = (e: MouseEvent) => {
      if (!downOutside) return;
      downOutside = false;
      const dx = e.clientX - downX;
      const dy = e.clientY - downY;
      if (dx * dx + dy * dy > 25) return; // > 5px = drag, not a click
      if (popoverRef.current && !popoverRef.current.contains(e.target as Node)) {
        onClose();
      }
    };
    document.addEventListener('mousedown', handleDown);
    document.addEventListener('mouseup', handleUp);
    return () => {
      document.removeEventListener('mousedown', handleDown);
      document.removeEventListener('mouseup', handleUp);
    };
  }, [onClose]);

  // Fetch atom data
  useEffect(() => {
    setIsLoading(true);
    getTransport().invoke<AtomWithTags | null>('get_atom_by_id', { id: atomId })
      .then((result) => {
        setAtom(result);
        setIsLoading(false);
      })
      .catch((err) => {
        console.error('Failed to fetch atom for preview:', err);
        setIsLoading(false);
      });
  }, [atomId]);

  // Refine position after render — but not after the user has manually dragged.
  useLayoutEffect(() => {
    if (hasBeenDraggedRef.current) return;
    if (!popoverRef.current) return;
    const rect = popoverRef.current.getBoundingClientRect();
    setPosition(calculatePosition(anchorRect, rect.height, rect.width));
  }, [anchorRect, atom, isLoading, isCollapsed, notesOpen]);

  // Drag handle on the header — mousedown starts drag, window mousemove updates,
  // mouseup ends. Clamps to viewport so the popover can't be dragged off-screen.
  const handleDragStart = useCallback((e: React.MouseEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const startX = e.clientX;
    const startY = e.clientY;
    const startTop = position.top;
    const startLeft = position.left;

    const onMove = (ev: MouseEvent) => {
      const dx = ev.clientX - startX;
      const dy = ev.clientY - startY;
      const width = popoverRef.current?.offsetWidth ?? POPOVER_WIDTH;
      const height = popoverRef.current?.offsetHeight ?? POPOVER_MAX_HEIGHT;
      const nextLeft = Math.max(8, Math.min(startLeft + dx, window.innerWidth - width - 8));
      const nextTop = Math.max(8, Math.min(startTop + dy, window.innerHeight - height - 8));
      hasBeenDraggedRef.current = true;
      setPosition({ top: nextTop, left: nextLeft });
    };
    const onUp = () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
    };
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
  }, [position.top, position.left]);

  const handleViewAtom = (e: React.MouseEvent) => {
    onViewAtom(atomId, { newTab: e.metaKey || e.ctrlKey });
    onClose();
  };

  // Progressive rendering: chunk content, render first chunk immediately, load rest incrementally
  const chunks = useMemo(
    () => (atom ? chunkMarkdown(companyProfile?.researchContent ?? executiveProfile?.researchContent ?? atom.content, CHUNK_SIZE) : []),
    [atom, companyProfile, executiveProfile]
  );
  const [renderedChunkCount, setRenderedChunkCount] = useState(INITIAL_CHUNKS);
  const isFullyRendered = renderedChunkCount >= chunks.length;

  // Reset when atom changes
  useEffect(() => { setRenderedChunkCount(INITIAL_CHUNKS); }, [atom?.id]);

  // Progressively load remaining chunks
  useEffect(() => {
    if (isFullyRendered) return;
    if ('requestIdleCallback' in window) {
      const id = requestIdleCallback(() => {
        setRenderedChunkCount((prev) => Math.min(prev + CHUNKS_PER_BATCH, chunks.length));
      }, { timeout: 100 });
      return () => cancelIdleCallback(id);
    } else {
      const id = setTimeout(() => {
        setRenderedChunkCount((prev) => Math.min(prev + CHUNKS_PER_BATCH, chunks.length));
      }, 32);
      return () => clearTimeout(id);
    }
  }, [renderedChunkCount, chunks.length, isFullyRendered]);

  return createPortal(
    <div
      ref={popoverRef}
      data-modal="true"
      className="fixed z-[100] bg-[var(--color-bg-card)] border border-[var(--color-border)] rounded-lg shadow-xl"
      style={{ top: position.top, left: position.left, width: POPOVER_WIDTH, maxWidth: 'calc(100vw - 16px)', maxHeight: 'calc(100vh - 16px)', overflowY: 'auto' }}
    >
      {isLoading ? (
        <div className="flex items-center justify-center py-8">
          <div className="flex items-center gap-2 text-[var(--color-text-secondary)]">
            <Loader2 className="animate-spin h-4 w-4" strokeWidth={2} />
            <span className="text-sm">Loading...</span>
          </div>
        </div>
      ) : !atom ? (
        <div className="py-6 text-center text-sm text-[var(--color-text-secondary)]">
          Atom not found
        </div>
      ) : (
        <>
          {/* Header: drag handle + title + metadata + collapse/close controls */}
          <div
            onMouseDown={handleDragStart}
            className={`px-4 py-3 flex items-start gap-2 select-none cursor-grab active:cursor-grabbing ${isCollapsed ? '' : 'border-b border-[var(--color-border)]'}`}
          >
            {nodeImage && !isCollapsed && (
              <div className="w-10 h-10 rounded-md border border-[var(--color-border)] bg-[var(--color-bg-main)] overflow-hidden shrink-0 flex items-center justify-center">
                <img src={nodeImage} alt="" className={`w-full h-full ${category?.key === 'executive' ? 'object-cover' : 'object-contain p-1'}`} />
              </div>
            )}
            <div className="flex-1 min-w-0">
              <h3 className={`text-sm font-medium text-[var(--color-text-primary)] ${isCollapsed ? 'truncate' : 'line-clamp-2'}`}>
                {atom.title || 'Untitled'}
              </h3>
              {!isCollapsed && (
                <div className="flex items-center gap-2 mt-1">
                  {atom.source_url && !atom.source_url.startsWith('atomic://') && (
                    <span className="text-[11px] text-[var(--color-text-tertiary)] bg-[var(--color-bg-main)] px-2 py-1 rounded truncate max-w-[200px]">
                      {atom.source || (() => { try { return new URL(atom.source_url!).hostname.replace(/^www\./, ''); } catch { return atom.source_url; } })()}
                    </span>
                  )}
                  <span className="text-[11px] text-[var(--color-text-tertiary)]">
                    {formatDate(atom.updated_at)}
                  </span>
                </div>
              )}
            </div>
            <div className="flex items-center gap-1 shrink-0" onMouseDown={(e) => e.stopPropagation()}>
              <button
                onClick={() => setIsCollapsed((v) => !v)}
                className="p-1 rounded hover:bg-[var(--color-bg-main)] text-[var(--color-text-secondary)]"
                title={isCollapsed ? 'Expand' : 'Collapse'}
              >
                {isCollapsed
                  ? <ChevronDown className="w-4 h-4" strokeWidth={2} />
                  : <ChevronUp className="w-4 h-4" strokeWidth={2} />}
              </button>
              <button
                onClick={onClose}
                className="p-1 rounded hover:bg-[var(--color-bg-main)] text-[var(--color-text-secondary)]"
                title="Close"
              >
                <X className="w-4 h-4" strokeWidth={2} />
              </button>
            </div>
          </div>

          {!isCollapsed && (
            <>
              {companyProfile && <CompanyProfileCard profile={companyProfile} name={atom.title || 'Company'} />}
              {executiveProfile && <ExecutiveProfileCard profile={executiveProfile} />}
              {/* Content preview */}
              {hasStructuredProfile && (
                <button
                  type="button"
                  onClick={() => setNotesOpen((open) => !open)}
                  aria-expanded={notesOpen}
                  className="w-full px-4 py-2 flex items-center justify-between text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-[var(--color-accent)]"
                >
                  Research notes
                  {notesOpen ? <ChevronUp className="w-4 h-4" /> : <ChevronDown className="w-4 h-4" />}
                </button>
              )}
              {(!hasStructuredProfile || notesOpen) && (
                <div className="px-4 py-3 prose prose-invert prose-sm max-w-none [&_h1]:text-sm [&_h2]:text-sm [&_h3]:text-xs [&_h4]:text-xs [&_h1]:m-0 [&_h2]:m-0 [&_h3]:m-0 [&_h4]:m-0 [&_p]:text-xs [&_li]:text-xs [&_pre]:text-[11px] [&_code]:text-[11px] [&_blockquote]:text-xs max-h-[200px] overflow-y-auto">
                  {chunks.slice(0, renderedChunkCount).map((chunk, i) => (
                    <MemoizedChunk key={i} content={chunk} />
                  ))}
                </div>
              )}

              {/* Tags */}
              {atom.tags.length > 0 && (
                <div className="px-4 pt-1 pb-2 flex flex-wrap gap-1">
                  {atom.tags.slice(0, 5).map((tag) => (
                    <TagChip key={tag.id} name={tag.name} size="xs" />
                  ))}
                  {atom.tags.length > 5 && (
                    <span className="text-[11px] text-[var(--color-text-tertiary)] self-center">
                      +{atom.tags.length - 5} more
                    </span>
                  )}
                </div>
              )}

              {/* Footer */}
              <div className="px-4 py-2 border-t border-[var(--color-border)]">
                <button
                  onClick={handleViewAtom}
                  className="flex items-center gap-1 text-sm text-[var(--color-accent)] hover:text-[var(--color-accent-light)] transition-colors"
                >
                  Open in reader
                  <ArrowRight className="w-4 h-4" strokeWidth={2} />
                </button>
              </div>
            </>
          )}
        </>
      )}
    </div>,
    document.body
  );
}
