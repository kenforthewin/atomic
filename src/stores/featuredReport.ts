import { create } from 'zustand';
import { getTransport } from '../lib/transport';

// ----- Wire types (match crates/atomic-core/src/models.rs) -----

export interface AtomKindAware {
  id: string;
  content: string;
  title?: string;
  snippet?: string;
  source_url?: string | null;
  created_at: string;
  updated_at: string;
  kind: 'captured' | 'report';
}

export interface ReportFinding {
  finding_atom_id: string;
  report_id: string | null;
  run_id: string | null;
  report_name_snapshot: string;
  created_at: string;
}

export interface ReportFindingCitation {
  finding_atom_id: string;
  cited_atom_id: string;
  position: number;
  excerpt: string;
}

// ----- UI shapes (kept compatible with BriefingContent for a drop-in rewire) -----

/**
 * Compatibility shim: the existing citation popover / CitationLink chain
 * was written against the old `BriefingCitation { citation_index, atom_id,
 * excerpt }` shape. The phase-3 storage shape uses `position` and
 * `cited_atom_id`; both names mean the same thing. Holding the mapping in
 * one place keeps the widget rewire surgical.
 */
export interface FindingCitation {
  citation_index: number;
  atom_id: string;
  excerpt: string;
  source_url?: string | null;
}

export interface FindingWithCitations {
  finding: ReportFinding;
  /** Title/snippet/created_at come straight off the finding atom row. */
  atom: { id: string; content: string; created_at: string };
  citations: FindingCitation[];
}

interface FeaturedReportStore {
  /// Per-DB id of the dashboard's featured report. `null` until loaded;
  /// after load, `null` means no report is featured (empty-state UI).
  reportId: string | null;
  /// Findings history (no citations), newest first. activeIndex points in.
  history: ReportFinding[];
  activeIndex: number;
  /// Active finding fully resolved (atom + citations). Lazy-loaded on nav.
  active: FindingWithCitations | null;
  isLoading: boolean;
  isRunning: boolean;
  error: string | null;

  /// Load featured-report id + latest finding + history. Called on mount,
  /// on DB switch, and whenever an atom-created event lands a new finding.
  fetchLatest: () => Promise<void>;
  /// Step by `delta` (+1 = older, -1 = newer). No-op at edges.
  navigate: (delta: number) => Promise<void>;
  /// Dispatch a `POST /api/reports/:id/run`. The server returns 202 and the
  /// finding shows up via the atom-created event a few seconds later; we
  /// poll once after a short delay as a belt-and-suspenders fallback.
  runNow: () => Promise<void>;
  reset: () => void;
}

const HISTORY_LIMIT = 30;

async function loadFinding(
  finding: ReportFinding,
): Promise<FindingWithCitations> {
  const transport = getTransport();
  // The finding atom is just an atom — fetch it through the standard
  // endpoint. Citations come from the dedicated lookup added in phase 3.
  const [atom, citations] = await Promise.all([
    transport.invoke<{
      id: string;
      content: string;
      created_at: string;
    }>('get_atom', { id: finding.finding_atom_id }),
    transport.invoke<ReportFindingCitation[]>('list_finding_citations', {
      atom_id: finding.finding_atom_id,
    }),
  ]);
  return {
    finding,
    atom: { id: atom.id, content: atom.content, created_at: atom.created_at },
    citations: citations.map((c) => ({
      citation_index: c.position,
      atom_id: c.cited_atom_id,
      excerpt: c.excerpt,
    })),
  };
}

export const useFeaturedReportStore = create<FeaturedReportStore>(
  (set, get) => ({
    reportId: null,
    history: [],
    activeIndex: 0,
    active: null,
    isLoading: false,
    isRunning: false,
    error: null,

    fetchLatest: async () => {
      set({ isLoading: true, error: null });
      try {
        const transport = getTransport();
        const { report_id } = await transport.invoke<{
          report_id: string | null;
        }>('get_featured_report_id');
        if (!report_id) {
          set({
            reportId: null,
            history: [],
            activeIndex: 0,
            active: null,
            isLoading: false,
          });
          return;
        }
        const rows = await transport.invoke<
          [ReportFinding, { atom: { id: string; created_at: string } }][]
        >('list_findings_for_report', {
          report_id,
          limit: HISTORY_LIMIT,
        });
        // The storage helper returns `(ReportFinding, AtomWithTags)`
        // tuples; we only need the provenance row for the history view.
        const history = rows.map(([f]) => f);
        if (history.length === 0) {
          set({
            reportId: report_id,
            history: [],
            activeIndex: 0,
            active: null,
            isLoading: false,
          });
          return;
        }
        const active = await loadFinding(history[0]);
        set({
          reportId: report_id,
          history,
          activeIndex: 0,
          active,
          isLoading: false,
        });
      } catch (error) {
        const msg = String(error);
        if (msg.includes('404') || msg.toLowerCase().includes('not found')) {
          set({
            reportId: null,
            history: [],
            activeIndex: 0,
            active: null,
            isLoading: false,
          });
        } else {
          set({ error: msg, isLoading: false });
        }
      }
    },

    navigate: async (delta) => {
      const { history, activeIndex } = get();
      const next = activeIndex + delta;
      if (next < 0 || next >= history.length) return;
      set({ activeIndex: next, isLoading: true, error: null });
      try {
        const active = await loadFinding(history[next]);
        set({ active, isLoading: false });
      } catch (error) {
        set({ error: String(error), isLoading: false });
      }
    },

    runNow: async () => {
      const { reportId } = get();
      if (!reportId) return;
      set({ isRunning: true, error: null });
      try {
        // 202 with a dispatch acknowledgement. The actual finding atom
        // shows up via the standard atom-created event after the agent
        // loop completes; we fall back to a delayed re-fetch in case
        // the event subscription missed it.
        await getTransport().invoke('run_report_now', { report_id: reportId });
        setTimeout(() => {
          void get().fetchLatest();
        }, 4000);
        set({ isRunning: false });
      } catch (error) {
        set({ error: String(error), isRunning: false });
      }
    },

    reset: () =>
      set({
        reportId: null,
        history: [],
        activeIndex: 0,
        active: null,
        isLoading: false,
        isRunning: false,
        error: null,
      }),
  }),
);
