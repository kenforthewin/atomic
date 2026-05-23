import { useEffect, useRef, useState } from 'react';
import { Plus } from 'lucide-react';
import { useReportsStore, Report, CreateReportInput } from '../../stores/reports';
import { useUIStore } from '../../stores/ui';
import { ReportsList } from './ReportsList';
import { ReportEditorModal } from './ReportEditorModal';
import { ReportTemplateGallery } from './ReportTemplateGallery';
import { Modal } from '../ui/Modal';
import { ReportTemplate } from '../../lib/reportTemplates';

/// Top-level reports view, mounted by MainView when viewMode === 'reports'.
/// Mirrors WikiFullView's shape: initialize the store once on mount,
/// tear down on unmount. The list itself owns rendering + virtualization.
///
/// 4b adds the create/edit/delete/enable-disable plumbing. Row click
/// currently opens the edit modal as a stand-in for the detail view —
/// that re-routes to ReportDetailView in 4c.
export function ReportsFullView() {
  const reports = useReportsStore(s => s.reports);
  const lastFindingByReport = useReportsStore(s => s.lastFindingByReport);
  const isLoadingList = useReportsStore(s => s.isLoadingList);
  const fetchAll = useReportsStore(s => s.fetchAll);
  const setEnabled = useReportsStore(s => s.setEnabled);
  const deleteReport = useReportsStore(s => s.delete);
  const ensureSubscription = useReportsStore(s => s.ensureSubscription);
  const openReportDetail = useUIStore(s => s.openReportDetail);

  const initializedRef = useRef(false);
  useEffect(() => {
    if (initializedRef.current) return;
    initializedRef.current = true;
    fetchAll();
    // Idempotent — also wired from ReportDetailView so deep-links
    // observe completion events even without visiting the list first.
    ensureSubscription();
  }, [fetchAll, ensureSubscription]);

  // Note: no `reset()` on unmount. The data in useReportsStore is
  // shared with ReportDetailView, the TabStrip's report-tab labels,
  // and the AtomCreated subscription's completion-matching logic.
  // Wiping it on view unmount empties byId before those consumers
  // can read it, causing blank detail pages and "Report N" fallback
  // labels. The wiki pattern that inspired this isn't a fit because
  // wikis aren't shared across views the way reports are.

  const [editorOpen, setEditorOpen] = useState(false);
  const [editingReport, setEditingReport] = useState<Report | null>(null);
  const [editorInitialBody, setEditorInitialBody] = useState<CreateReportInput | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<Report | null>(null);
  const [galleryOpen, setGalleryOpen] = useState(false);

  // "New report" no longer opens a blank editor directly — it opens
  // the template gallery, which then opens the editor with the picked
  // template's body (or `null` for "Start blank"). Both paths share
  // one editor surface.
  const openNewReport = () => {
    setGalleryOpen(true);
  };

  const handleTemplatePick = (template: ReportTemplate | null) => {
    setGalleryOpen(false);
    setEditingReport(null);
    setEditorInitialBody(template?.body ?? null);
    setEditorOpen(true);
  };

  const openEdit = (reportId: string) => {
    const r = reports.find(x => x.id === reportId);
    if (!r) return;
    setEditingReport(r);
    setEditorInitialBody(null);
    setEditorOpen(true);
  };

  const handleDelete = (reportId: string) => {
    const r = reports.find(x => x.id === reportId);
    if (!r) return;
    setConfirmDelete(r);
  };

  const confirmDeleteNow = async () => {
    if (!confirmDelete) return;
    const target = confirmDelete;
    setConfirmDelete(null);
    try {
      await deleteReport(target.id);
    } catch {
      // Store toasts on failure; nothing else to do here.
    }
  };

  return (
    <div className="h-full overflow-hidden flex flex-col">
      {/* Header row — title + New Report. Read-only mode (no
          `setEnabled`/`delete` available, which can't happen in 4b but
          is the future-proof check) renders no header at all. */}
      <div className="flex items-center justify-between px-5 py-3 border-b border-[var(--color-border)] flex-shrink-0">
        <div className="flex items-center gap-3">
          <h2 className="text-sm font-medium uppercase tracking-[0.12em] text-[var(--color-text-secondary)]">
            Reports
          </h2>
          {reports.length > 0 && (
            <span className="text-xs text-[var(--color-text-tertiary)] tabular-nums">
              {reports.length}
            </span>
          )}
        </div>
        <button
          onClick={openNewReport}
          className="
            inline-flex items-center gap-1.5 px-3 py-1.5 rounded-md text-sm font-medium
            bg-[var(--color-accent)] text-white hover:bg-[var(--color-accent-hover)]
            transition-colors
          "
        >
          <Plus className="w-4 h-4" strokeWidth={2.5} />
          New report
        </button>
      </div>

      <ReportsList
        reports={reports}
        lastFindingByReport={lastFindingByReport}
        isLoading={isLoadingList}
        onRowClick={openReportDetail}
        onEdit={openEdit}
        onToggleEnabled={setEnabled}
        onDelete={handleDelete}
        onPickTemplate={handleTemplatePick}
      />

      <ReportEditorModal
        isOpen={editorOpen}
        report={editingReport}
        initialBody={editorInitialBody}
        onClose={() => setEditorOpen(false)}
      />

      <ReportTemplateGallery
        mode="modal"
        isOpen={galleryOpen}
        onClose={() => setGalleryOpen(false)}
        onPick={handleTemplatePick}
      />

      <Modal
        isOpen={confirmDelete !== null}
        onClose={() => setConfirmDelete(null)}
        title={`Delete "${confirmDelete?.name ?? ''}"?`}
        confirmLabel="Delete report"
        confirmVariant="danger"
        onConfirm={confirmDeleteNow}
      >
        <div className="text-sm text-[var(--color-text-secondary)] leading-relaxed">
          The schedule and report definition will be deleted. Past findings
          remain in your atoms — they're first-class notes, not owned by the
          report that produced them.
          {confirmDelete?.last_finding_atom_id && (
            <span className="block mt-2 text-[var(--color-text-tertiary)] text-xs">
              The dashboard's featured report pointer is cleared if it points here.
            </span>
          )}
        </div>
      </Modal>
    </div>
  );
}
