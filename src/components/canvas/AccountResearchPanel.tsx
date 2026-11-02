import { useState } from 'react';
import { ChevronDown, ExternalLink, RefreshCw } from 'lucide-react';
import type { CanvasAtomPosition } from '../../lib/api';
import { accountNetworkCategory } from './accountNetworkCategories';
import { nodesWithoutRecordedRun, type AccountResearchCheck } from './accountNetworkEnrichment';

type ResultFilter = 'all' | 'data' | 'empty' | 'unrecorded' | 'entry';

const FILTER_TONES = {
  data: { active: 'border-emerald-400/60 bg-emerald-400/15', idle: 'border-emerald-400/20 bg-emerald-400/5 hover:bg-emerald-400/10', count: 'text-emerald-300', dot: 'bg-emerald-400' },
  empty: { active: 'border-amber-400/60 bg-amber-400/15', idle: 'border-amber-400/20 bg-amber-400/5 hover:bg-amber-400/10', count: 'text-amber-300', dot: 'bg-amber-400' },
  unrecorded: { active: 'border-white/35 bg-white/10', idle: 'border-white/10 bg-white/[0.02] hover:bg-white/5', count: 'text-white/70', dot: 'bg-white/35' },
  entry: { active: 'border-violet-400/60 bg-violet-400/15', idle: 'border-violet-400/20 bg-violet-400/5 hover:bg-violet-400/10', count: 'text-violet-300', dot: 'bg-violet-400' },
} as const;

interface AccountResearchPanelProps {
  atoms: CanvasAtomPosition[];
  checks: AccountResearchCheck[];
  entryPoints: Array<{ source: string; target: string; sourceTitle: string; targetTitle: string; evidenceUrl: string }>;
  linkState: 'loading' | 'error' | 'ready';
  loading: boolean;
  error: boolean;
  onRetry: () => void;
  onRetryLinks: () => void;
  onRefresh: () => void;
  onFocus: (atomId: string) => void;
  animatePaths: boolean;
  motionAvailable: boolean;
  onAnimatePathsChange: (value: boolean) => void;
}

export function AccountResearchPanel({
  atoms, checks, entryPoints, linkState, loading, error, onRetry, onRetryLinks, onRefresh, onFocus, animatePaths, motionAvailable, onAnimatePathsChange,
}: AccountResearchPanelProps) {
  const [detailsOpen, setDetailsOpen] = useState(() => !window.matchMedia('(max-width: 639px)').matches);
  const [filter, setFilter] = useState<ResultFilter>('all');
  const dataCount = checks.filter(check => check.result === 'data').length;
  const emptyCount = checks.filter(check => check.result === 'empty').length;
  const entryCount = entryPoints.length;
  const unrecorded = nodesWithoutRecordedRun(atoms, checks);
  const visibleChecks = checks.filter(check => filter === 'all' || check.result === filter);
  const visibleEntries = filter === 'all' || filter === 'entry' ? entryPoints : [];
  const visibleUnrecorded = filter === 'all' || filter === 'unrecorded' ? unrecorded : [];

  const chooseFilter = (next: ResultFilter) => {
    setFilter(current => current === next ? 'all' : next);
    setDetailsOpen(true);
  };
  const focusCheck = (atomId: string) => {
    if (window.matchMedia('(max-width: 639px)').matches) setDetailsOpen(false);
    onFocus(atomId);
  };

  return (
    <section aria-label="Recorded enrichment" className="px-3 py-3">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[11px] text-white/55">Run status</span>
        <button type="button" onClick={onRefresh} aria-label="Refresh account research" title="Refresh account research"
          className="rounded p-1 text-white/55 hover:bg-white/5 hover:text-white focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">
          <RefreshCw className="h-3.5 w-3.5" />
        </button>
      </div>
      <div className="mt-2 grid grid-cols-2 gap-1.5" aria-label="Filter research checks">
        {([
          ['data', 'Found data', dataCount],
          ['empty', 'No findings', emptyCount],
          ['unrecorded', 'No run recorded', unrecorded.length],
          ['entry', 'Verified paths', entryCount],
        ] as const).map(([key, label, count]) => (
          <button key={key} type="button" onClick={() => chooseFilter(key)} aria-pressed={filter === key}
            className={`min-h-12 rounded-md border px-2.5 py-1.5 text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-white/80 ${
              filter === key ? FILTER_TONES[key].active : FILTER_TONES[key].idle
            }`}>
            <span className="flex items-center justify-between gap-2">
              <span className={`text-[16px] font-semibold tabular-nums ${FILTER_TONES[key].count}`}>{loading || error || key === 'entry' && linkState !== 'ready' ? '–' : count}</span>
              <span aria-hidden="true" className={`h-2 w-2 rounded-full ${FILTER_TONES[key].dot}`} />
            </span>
            <span className="block text-[11px] text-white/75">{label}</span>
          </button>
        ))}
      </div>
      <button type="button" aria-expanded={detailsOpen} onClick={() => setDetailsOpen(value => !value)}
        className="mt-2 flex w-full items-center justify-between rounded py-1 text-left text-[11px] text-white/65 hover:text-white focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">
        {detailsOpen ? 'Hide checks' : 'Show checks'}
        <ChevronDown className={`h-3.5 w-3.5 transition-transform ${detailsOpen ? 'rotate-180' : ''}`} />
      </button>
      {detailsOpen && <div className="mt-2">
          <div className="mb-1.5 flex items-center justify-between gap-2">
            <span className="text-[11px] text-white/55">{filter === 'all' ? 'All research status' : filter === 'data' ? 'Runs with data' : filter === 'empty' ? 'Runs with no findings' : filter === 'unrecorded' ? 'No run recorded' : 'Verified entry points'}</span>
            {filter !== 'all' && <button type="button" onClick={() => setFilter('all')}
              className="text-[11px] text-white/75 underline decoration-white/30 underline-offset-2 hover:text-white focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">Show all</button>}
          </div>
          {loading ? <p role="status" className="py-3 text-xs text-white/60">Reading account notes…</p>
            : error ? <div role="alert" className="flex items-center justify-between gap-2 py-2 text-xs text-white/75">
              <span>Could not read research checks</span>
              <button type="button" onClick={onRetry} className="rounded border border-white/25 px-2 py-1 text-white hover:bg-white/10 focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">Retry</button>
            </div>
              : filter === 'entry' && linkState === 'error' ? <div role="alert" className="flex items-center justify-between gap-2 py-2 text-xs text-white/75">
                <span>Could not verify entry paths</span>
                <button type="button" onClick={onRetryLinks} className="rounded border border-white/25 px-2 py-1 text-white hover:bg-white/10 focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">Retry</button>
              </div>
                : filter === 'entry' && linkState === 'loading' ? <p role="status" className="py-3 text-xs text-white/60">Checking note links…</p>
                  : visibleEntries.length || visibleChecks.length || visibleUnrecorded.length ? <ul className="max-h-[304px] overflow-y-auto border-t border-white/10 max-sm:max-h-[224px]">
                {visibleEntries.map(entry => (
                  <li key={`${entry.source}:${entry.target}`} className="flex items-start gap-1 border-b border-l-2 border-b-white/10 border-l-violet-400/70 bg-violet-400/[0.04] py-2 pl-2 pr-1">
                    <button type="button" onClick={() => focusCheck(entry.target)}
                      className="min-w-0 flex-1 rounded text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">
                      <span className="block truncate text-xs font-medium text-white/90">{entry.sourceTitle} → {entry.targetTitle}</span>
                      <span className="mt-1 inline-flex rounded border border-violet-400/30 bg-violet-400/10 px-1.5 py-0.5 text-[10px] font-medium text-violet-200">Verified path</span>
                    </button>
                    <a href={entry.evidenceUrl} target="_blank" rel="noopener noreferrer" aria-label={`Open evidence for ${entry.sourceTitle} to ${entry.targetTitle}`} title="Open entry point evidence"
                      className="mt-0.5 rounded p-1 text-white/55 hover:text-white focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80"><ExternalLink className="h-3.5 w-3.5" /></a>
                  </li>
                ))}
                {visibleChecks.map(check => (
                  <li key={check.id} className={`flex items-start gap-1 border-b border-l-2 border-b-white/10 py-2 pl-2 pr-1 last:border-b-0 ${check.result === 'data' ? 'border-l-emerald-400/70 bg-emerald-400/[0.04]' : 'border-l-amber-400/70 bg-amber-400/[0.04]'}`}>
                    <button type="button" onClick={() => focusCheck(check.atomId)}
                      className="min-w-0 flex-1 rounded text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">
                      <span className="block truncate text-xs font-medium text-white/90">{check.title}</span>
                      <span className="mt-0.5 block text-[11px] text-white/60">{check.source} · {check.checkedAt}</span>
                      <span className="mt-1 flex flex-wrap items-center gap-1.5">
                        <span className={`rounded border px-1.5 py-0.5 text-[10px] font-medium ${check.result === 'data' ? 'border-emerald-400/30 bg-emerald-400/10 text-emerald-200' : 'border-amber-400/30 bg-amber-400/10 text-amber-200'}`}>
                          {check.result === 'data' ? 'Ran · found data' : 'Ran · no findings'}
                        </span>
                        {check.path === 'verified' && <span className="rounded border border-violet-400/30 bg-violet-400/10 px-1.5 py-0.5 text-[10px] font-medium text-violet-200">Verified path</span>}
                      </span>
                      <span className="mt-1 block text-[11px] text-white/70">
                        {check.resultLabel}{check.kind === 'person' && check.path !== 'verified' ? ` · ${check.path === 'none' ? 'No named intro in this check' : 'Introduction unverified'}` : ''}
                      </span>
                    </button>
                    {check.sourceUrl && <a href={check.sourceUrl} target="_blank" rel="noopener noreferrer" aria-label={`Open ${check.title} research source`} title="Open research source"
                      className="mt-0.5 rounded p-1 text-white/55 hover:text-white focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80"><ExternalLink className="h-3.5 w-3.5" /></a>}
                  </li>
                ))}
                {visibleUnrecorded.map(atom => (
                  <li key={`unrecorded:${atom.atom_id}`} className="flex items-start border-b border-l-2 border-b-white/10 border-l-white/25 bg-white/[0.02] py-2 pl-2 pr-1 last:border-b-0">
                    <button type="button" onClick={() => focusCheck(atom.atom_id)}
                      className="min-w-0 flex-1 rounded text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80">
                      <span className="block truncate text-xs font-medium text-white/75">{atom.title}</span>
                      <span className="mt-0.5 block text-[11px] text-white/45">{accountNetworkCategory(atom.source_url)?.label ?? 'Account node'}</span>
                      <span className="mt-1 inline-flex rounded border border-white/15 px-1.5 py-0.5 text-[10px] font-medium text-white/55">No run recorded</span>
                    </button>
                  </li>
                ))}
              </ul>
                : <p className="py-3 text-xs text-white/60">{filter === 'entry' ? 'No verified entry points recorded' : filter === 'all' ? 'No account nodes to check' : 'No checks in this group'}</p>}
          {entryCount > 0 && <button type="button" aria-pressed={animatePaths} disabled={!motionAvailable}
            onClick={() => onAnimatePathsChange(!animatePaths)}
            className="mt-2 text-[11px] text-white/75 underline decoration-white/30 underline-offset-2 hover:text-white focus-visible:outline focus-visible:outline-2 focus-visible:outline-white/80 disabled:cursor-not-allowed disabled:text-white/45">
            {!motionAvailable ? 'Motion off in system settings' : animatePaths ? 'Pause entry path motion' : 'Animate entry paths'}
          </button>}
          <p className="mt-2 text-[11px] leading-4 text-white/50">No run recorded means this atom has no check record, not that no external check happened. A verified path needs a specific, identified introducer.</p>
      </div>}
    </section>
  );
}
