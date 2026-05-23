import { create } from 'zustand';
import { toast } from 'sonner';
import { getTransport } from '../lib/transport';

// =====================================================================
// Types — mirror crates/atomic-core/src/models.rs
// =====================================================================

/// Discriminator on every atom; reports return findings with kind=report.
export type AtomKind = 'captured' | 'report';

export type SourceScopeWindow =
  | { kind: 'since_last_run' }
  | { kind: 'iso_duration'; value: string };

export type ContextScopeMode = 'all' | 'same_as_source' | 'tags' | 'none';

export type ContextScopeWindow = SourceScopeWindow;

export type CitationPolicy = 'source_only' | 'context_citable';

/// One report definition. Matches `Report` in atomic-core. Cache fields
/// (`last_run_at`, `last_finding_atom_id`, `last_error`) are advisory —
/// authoritative state lives on `task_runs` + `report_findings`.
export interface Report {
  id: string;
  name: string;
  description: string | null;
  research_prompt: string;

  source_scope_tag_ids: string[];
  source_scope_window: SourceScopeWindow | null;
  source_include_kinds: AtomKind[];

  context_scope_mode: ContextScopeMode;
  context_scope_tag_ids: string[];
  context_scope_window: ContextScopeWindow | null;
  context_include_kinds: AtomKind[];

  citation_policy: CitationPolicy;

  max_source_atoms: number | null;
  max_source_tokens: number | null;
  max_tool_iterations: number | null;

  schedule: string;
  schedule_tz: string | null;

  enabled: boolean;
  output_atom_tags: string[];

  last_run_at: string | null;
  last_finding_atom_id: string | null;
  last_error: string | null;

  created_at: string;
  updated_at: string;
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

/// What `list_findings_for_report` returns: each finding row joined with
/// the standard AtomWithTags snippet. `serde(flatten)` on AtomWithTags
/// means atom fields live at the top of `atom` (no nested `.atom`).
export interface ReportFindingWithAtom {
  finding: ReportFinding;
  atom: {
    id: string;
    content: string;
    source_url: string | null;
    created_at: string;
    updated_at: string;
    kind: AtomKind;
    [k: string]: unknown;
  };
}

// =====================================================================
// Write request shapes — mirror Create/UpdateReportRequest
// =====================================================================

/// `POST /api/reports` body. Mirrors `CreateReportRequest`. Backend
/// fills sensible defaults for any field omitted; we keep them
/// optional here for the editor's progressive-disclosure UX.
export interface CreateReportInput {
  name: string;
  description?: string | null;
  research_prompt: string;

  source_scope_tag_ids?: string[];
  source_scope_window?: SourceScopeWindow | null;
  source_include_kinds?: AtomKind[];

  context_scope_mode?: ContextScopeMode;
  context_scope_tag_ids?: string[];
  context_scope_window?: ContextScopeWindow | null;
  context_include_kinds?: AtomKind[];

  citation_policy?: CitationPolicy;

  max_source_atoms?: number | null;
  max_source_tokens?: number | null;
  max_tool_iterations?: number | null;

  schedule: string;
  schedule_tz?: string | null;

  enabled?: boolean;
  output_atom_tags?: string[];
}

/// `PUT /api/reports/:id` body. Every field optional; only present
/// fields are written. Mirrors `UpdateReportRequest`. Note the nested
/// `Option<Option<T>>` pattern from Rust collapses to plain optional
/// here — we send `null` when the user is explicitly clearing the
/// field, omit the key entirely when they aren't touching it.
export interface UpdateReportInput {
  name?: string;
  description?: string | null;
  research_prompt?: string;

  source_scope_tag_ids?: string[];
  source_scope_window?: SourceScopeWindow | null;
  source_include_kinds?: AtomKind[];

  context_scope_mode?: ContextScopeMode;
  context_scope_tag_ids?: string[];
  context_scope_window?: ContextScopeWindow | null;
  context_include_kinds?: AtomKind[];

  citation_policy?: CitationPolicy;

  max_source_atoms?: number | null;
  max_source_tokens?: number | null;
  max_tool_iterations?: number | null;

  schedule?: string;
  schedule_tz?: string | null;

  enabled?: boolean;
  output_atom_tags?: string[];
}

// =====================================================================
// Store
// =====================================================================

interface ReportsStore {
  reports: Report[];
  byId: Record<string, Report>;

  /// Cached last finding per report so the list view's tertiary line
  /// (the italic excerpt) doesn't issue N requests on every render.
  /// `null` after a fetch attempt means "no findings yet" — distinguishes
  /// from `undefined` ("never fetched").
  lastFindingByReport: Record<string, ReportFindingWithAtom | null>;

  isLoadingList: boolean;
  loadError: string | null;

  /// Has the atom-created subscription already been set up? Guards
  /// against double-subscription if `fetchAll` is called twice.
  hasSubscription: boolean;

  fetchAll: () => Promise<void>;
  fetchLastFinding: (reportId: string) => Promise<void>;

  /// Create a new report. Returns the created `Report` so the caller
  /// (typically the editor modal) can navigate to it on success.
  /// Throws on failure with a useful message; the store toasts and the
  /// caller can keep its modal open.
  create: (input: CreateReportInput) => Promise<Report>;

  /// Patch an existing report. Returns the merged row from the server.
  /// On failure, throws and leaves the in-memory row untouched.
  update: (id: string, input: UpdateReportInput) => Promise<Report>;

  /// Convenience for the row-level toggle. Optimistic: flips the flag
  /// locally first, reverts on failure. Wired through `update_report`
  /// rather than a dedicated endpoint to keep the transport surface
  /// narrow.
  setEnabled: (id: string, enabled: boolean) => Promise<void>;

  /// Delete a report. Optimistic: removes from the list first, restores
  /// on failure. Findings outlive their producer by design — only the
  /// schedule + definition go away. (The backend already clears the
  /// dashboard's `featured_report_id` if it pointed at this report.)
  delete: (id: string) => Promise<void>;

  reset: () => void;
}

export const useReportsStore = create<ReportsStore>((set, get) => {
  // Module-scope handle for the atom-created unsubscribe so `reset()` can
  // tear it down. Captured in closure rather than store state because it
  // isn't render-relevant.
  let atomCreatedUnsub: (() => void) | null = null;

  return {
    reports: [],
    byId: {},
    lastFindingByReport: {},
    isLoadingList: false,
    loadError: null,
    hasSubscription: false,

    fetchAll: async () => {
      set({ isLoadingList: true, loadError: null });
      try {
        const reports = await getTransport().invoke<Report[]>('list_reports');
        const byId: Record<string, Report> = {};
        for (const r of reports) byId[r.id] = r;
        set({ reports, byId, isLoadingList: false });

        // Lazily prime last-finding for every report. Issue requests in
        // parallel; failures degrade to "no excerpt available" without
        // surfacing per-report toasts.
        await Promise.all(reports.map(r => get().fetchLastFinding(r.id)));

        // Wire the live-refresh subscription once. The dashboard
        // BriefingWidget uses the same shape: AtomWithTags flattens, so
        // `kind` lives at the payload top level. When a report finding
        // lands we refresh just that report's last-finding cache (and
        // could re-fetch the row itself if we needed updated `last_run_at`;
        // for 4a the cache update on next list refresh is sufficient).
        if (!get().hasSubscription) {
          atomCreatedUnsub = getTransport().subscribe('atom-created', (payload) => {
            const p = payload as { kind?: string; id?: string } | undefined;
            if (p?.kind !== 'report') return;
            // We don't know which report produced it from the payload
            // alone, so re-prime last-finding for every report we know
            // about. Cheap (one request each, no joins beyond atom tags).
            const ids = Object.keys(get().byId);
            ids.forEach(id => get().fetchLastFinding(id));
          });
          set({ hasSubscription: true });
        }
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        set({ isLoadingList: false, loadError: msg });
        toast.error('Failed to load reports', { description: msg });
      }
    },

    fetchLastFinding: async (reportId: string) => {
      try {
        const results = await getTransport().invoke<ReportFindingWithAtom[]>(
          'list_findings_for_report',
          { report_id: reportId, limit: 1 }
        );
        const first = results[0] ?? null;
        set(state => ({
          lastFindingByReport: { ...state.lastFindingByReport, [reportId]: first },
        }));
      } catch (e) {
        // Per-report failure: leave the cache untouched, log, and let the
        // row render without an excerpt. We don't toast — N possible
        // failures would flood the user.
        console.error('[reports] fetchLastFinding failed', reportId, e);
      }
    },

    create: async (input: CreateReportInput) => {
      try {
        const created = await getTransport().invoke<Report>('create_report', input as unknown as Record<string, unknown>);
        // Prepend on success; the list view shows most-recently-created
        // first by default. The next fetchAll will re-canonicalize
        // sort, but in-the-moment ordering should feel snappy.
        set(state => ({
          reports: [created, ...state.reports.filter(r => r.id !== created.id)],
          byId: { ...state.byId, [created.id]: created },
        }));
        toast.success('Report created', { description: created.name });
        return created;
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        toast.error('Failed to create report', { description: msg });
        throw e;
      }
    },

    update: async (id: string, input: UpdateReportInput) => {
      try {
        const merged = await getTransport().invoke<Report>('update_report', {
          report_id: id,
          ...input,
        });
        set(state => ({
          reports: state.reports.map(r => (r.id === id ? merged : r)),
          byId: { ...state.byId, [id]: merged },
        }));
        return merged;
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        toast.error('Failed to update report', { description: msg });
        throw e;
      }
    },

    setEnabled: async (id: string, enabled: boolean) => {
      const prev = get().byId[id];
      if (!prev) return;
      // Optimistic local flip so the row's badge updates instantly.
      const optimistic: Report = { ...prev, enabled };
      set(state => ({
        reports: state.reports.map(r => (r.id === id ? optimistic : r)),
        byId: { ...state.byId, [id]: optimistic },
      }));
      try {
        const merged = await getTransport().invoke<Report>('update_report', {
          report_id: id,
          enabled,
        });
        set(state => ({
          reports: state.reports.map(r => (r.id === id ? merged : r)),
          byId: { ...state.byId, [id]: merged },
        }));
      } catch (e) {
        // Revert. The next fetchAll would heal anyway, but a snappy
        // revert avoids a stuck-toggle perception while the user reads
        // the toast.
        set(state => ({
          reports: state.reports.map(r => (r.id === id ? prev : r)),
          byId: { ...state.byId, [id]: prev },
        }));
        const msg = e instanceof Error ? e.message : String(e);
        toast.error(enabled ? 'Failed to enable report' : 'Failed to pause report', {
          description: msg,
        });
      }
    },

    delete: async (id: string) => {
      const prev = get().reports;
      const prevById = get().byId;
      const target = prevById[id];
      // Optimistic removal.
      set(state => ({
        reports: state.reports.filter(r => r.id !== id),
        byId: Object.fromEntries(Object.entries(state.byId).filter(([k]) => k !== id)),
      }));
      try {
        await getTransport().invoke('delete_report', { report_id: id });
        toast.success('Report deleted', {
          description: target ? `${target.name} — findings remain in your atoms` : undefined,
        });
      } catch (e) {
        // Restore on failure.
        set({ reports: prev, byId: prevById });
        const msg = e instanceof Error ? e.message : String(e);
        toast.error('Failed to delete report', { description: msg });
        throw e;
      }
    },

    reset: () => {
      atomCreatedUnsub?.();
      atomCreatedUnsub = null;
      set({
        reports: [],
        byId: {},
        lastFindingByReport: {},
        isLoadingList: false,
        loadError: null,
        hasSubscription: false,
      });
    },
  };
});
