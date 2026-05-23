import { useEffect, useState, useMemo } from 'react';
import { ChevronDown, ChevronRight } from 'lucide-react';
import { Modal } from '../ui/Modal';
import { ScheduleField } from './ScheduleField';
import { ScopeField } from './ScopeField';
import { CitationPolicyField } from './CitationPolicyField';
import { TagSelector } from '../tags/TagSelector';
import {
  useReportsStore,
  Report,
  CitationPolicy,
  ContextScopeMode,
  SourceScopeWindow,
  CreateReportInput,
  UpdateReportInput,
} from '../../stores/reports';
import { Tag } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { getBrowserTimeZone } from '../../lib/tz';

interface ReportEditorModalProps {
  isOpen: boolean;
  /// Edit existing report when present; create new when undefined.
  /// Switching between modes between mounts is fine — the form fully
  /// re-derives its state from `report` whenever `isOpen` toggles.
  report?: Report | null;
  onClose: () => void;
  onSaved?: (report: Report) => void;
}

/// Shared form state for both create and edit. We work in a flat shape
/// matching the Create/UpdateReportInput types so the save path is a
/// near-direct hand-off.
interface FormState {
  name: string;
  description: string;
  research_prompt: string;
  schedule: string;
  schedule_tz: string | null;
  enabled: boolean;
  source_tag_ids: string[];
  source_window: SourceScopeWindow | null;
  context_mode: ContextScopeMode;
  context_tag_ids: string[];
  context_window: SourceScopeWindow | null;
  citation_policy: CitationPolicy;
  output_atom_tags: string[];
}

const DEFAULT_FORM: FormState = {
  name: '',
  description: '',
  research_prompt: '',
  schedule: '0 0 9 * * *', // sensible default: 9am daily
  schedule_tz: null,
  enabled: true,
  source_tag_ids: [],
  source_window: { kind: 'since_last_run' },
  context_mode: 'same_as_source',
  context_tag_ids: [],
  context_window: null,
  citation_policy: 'source_only',
  output_atom_tags: [],
};

function reportToForm(r: Report): FormState {
  return {
    name: r.name,
    description: r.description ?? '',
    research_prompt: r.research_prompt,
    schedule: r.schedule,
    schedule_tz: r.schedule_tz,
    enabled: r.enabled,
    source_tag_ids: r.source_scope_tag_ids,
    source_window: r.source_scope_window,
    context_mode: r.context_scope_mode,
    context_tag_ids: r.context_scope_tag_ids,
    context_window: r.context_scope_window,
    citation_policy: r.citation_policy,
    output_atom_tags: r.output_atom_tags,
  };
}

function formToCreateInput(f: FormState): CreateReportInput {
  return {
    name: f.name.trim(),
    description: f.description.trim() || null,
    research_prompt: f.research_prompt,
    schedule: f.schedule,
    schedule_tz: f.schedule_tz,
    enabled: f.enabled,
    source_scope_tag_ids: f.source_tag_ids,
    source_scope_window: f.source_window,
    context_scope_mode: f.context_mode,
    context_scope_tag_ids: f.context_mode === 'tags' ? f.context_tag_ids : [],
    context_scope_window: f.context_mode === 'same_as_source' ? null : f.context_window,
    citation_policy: f.citation_policy,
    output_atom_tags: f.output_atom_tags,
  };
}

function formToUpdateInput(f: FormState, original: Report): UpdateReportInput {
  // Send only fields that actually changed. Keeps the merge surface
  // small and avoids accidentally stomping fields the user didn't touch.
  const out: UpdateReportInput = {};
  const trimmedDesc = f.description.trim() || null;
  if (f.name.trim() !== original.name) out.name = f.name.trim();
  if (trimmedDesc !== (original.description ?? null)) out.description = trimmedDesc;
  if (f.research_prompt !== original.research_prompt) out.research_prompt = f.research_prompt;
  if (f.schedule !== original.schedule) out.schedule = f.schedule;
  if (f.schedule_tz !== original.schedule_tz) out.schedule_tz = f.schedule_tz;
  if (f.enabled !== original.enabled) out.enabled = f.enabled;
  if (!sameStringArr(f.source_tag_ids, original.source_scope_tag_ids)) out.source_scope_tag_ids = f.source_tag_ids;
  if (!sameWindow(f.source_window, original.source_scope_window)) out.source_scope_window = f.source_window;
  if (f.context_mode !== original.context_scope_mode) out.context_scope_mode = f.context_mode;
  const ctxTags = f.context_mode === 'tags' ? f.context_tag_ids : [];
  if (!sameStringArr(ctxTags, original.context_scope_tag_ids)) out.context_scope_tag_ids = ctxTags;
  const ctxWindow = f.context_mode === 'same_as_source' ? null : f.context_window;
  if (!sameWindow(ctxWindow, original.context_scope_window)) out.context_scope_window = ctxWindow;
  if (f.citation_policy !== original.citation_policy) out.citation_policy = f.citation_policy;
  if (!sameStringArr(f.output_atom_tags, original.output_atom_tags)) out.output_atom_tags = f.output_atom_tags;
  return out;
}

function sameStringArr(a: string[], b: string[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function sameWindow(a: SourceScopeWindow | null, b: SourceScopeWindow | null): boolean {
  if (!a && !b) return true;
  if (!a || !b) return false;
  if (a.kind !== b.kind) return false;
  if (a.kind === 'iso_duration' && b.kind === 'iso_duration') return a.value === b.value;
  return true;
}

export function ReportEditorModal({ isOpen, report, onClose, onSaved }: ReportEditorModalProps) {
  const create = useReportsStore(s => s.create);
  const update = useReportsStore(s => s.update);
  const tags = useTagsStore(s => s.tags);

  const isEdit = Boolean(report);

  const [form, setForm] = useState<FormState>(DEFAULT_FORM);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [isSaving, setIsSaving] = useState(false);

  // Re-derive form state every time the modal opens. Closing-and-
  // reopening with no `report` is the "new" flow; closing-and-reopening
  // with one is "edit" — either way the form resets cleanly.
  useEffect(() => {
    if (!isOpen) return;
    if (report) {
      setForm(reportToForm(report));
    } else {
      setForm({ ...DEFAULT_FORM, schedule_tz: getBrowserTimeZone() });
    }
    // Auto-expand advanced when editing an existing report that has
    // non-default advanced fields — the user is here to find them.
    setShowAdvanced(Boolean(report && (
      report.source_scope_tag_ids.length > 0 ||
      report.context_scope_mode !== 'same_as_source' ||
      report.citation_policy !== 'source_only' ||
      report.output_atom_tags.length > 0
    )));
  }, [isOpen, report]);

  // Output-tags Tag[] derivation (TagSelector wants Tag objects).
  const outputTagObjs = useMemo<Tag[]>(() => {
    const flat: Tag[] = [];
    function walk(nodes: any[]) {
      for (const n of nodes) {
        flat.push({ id: n.id, name: n.name, parent_id: n.parent_id ?? null, created_at: n.created_at ?? '' });
        if (n.children) walk(n.children);
      }
    }
    walk(tags as any);
    const set = new Set(form.output_atom_tags);
    return flat.filter(t => set.has(t.id));
  }, [tags, form.output_atom_tags]);

  const nameInvalid = form.name.trim().length === 0;
  const promptInvalid = form.research_prompt.trim().length === 0;
  const canSave = !nameInvalid && !promptInvalid && !isSaving;

  const handleSave = async () => {
    if (!canSave) return;
    setIsSaving(true);
    try {
      if (report) {
        const patch = formToUpdateInput(form, report);
        // No-op shortcut: nothing changed. Close without a request.
        if (Object.keys(patch).length === 0) {
          onClose();
          return;
        }
        const merged = await update(report.id, patch);
        onSaved?.(merged);
      } else {
        const created = await create(formToCreateInput(form));
        onSaved?.(created);
      }
      onClose();
    } catch {
      // Store already toasted; keep the modal open so the user can
      // correct what went wrong.
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <Modal
      isOpen={isOpen}
      onClose={onClose}
      title={isEdit ? `Edit "${report?.name ?? ''}"` : 'New Report'}
      width="xl"
      confirmLabel={isEdit ? 'Save changes' : 'Create report'}
      onConfirm={handleSave}
      confirmDisabled={!canSave}
    >
      <div className="flex flex-col gap-5">
        {/* Name */}
        <div className="flex flex-col gap-1.5">
          <label className="text-xs font-medium uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
            Name
          </label>
          <input
            type="text"
            value={form.name}
            onChange={(e) => setForm(f => ({ ...f, name: e.target.value }))}
            placeholder="Daily Briefing, Weekly contradiction scan…"
            className={`
              px-3 py-2 rounded-md text-sm
              bg-[var(--color-bg-input)] border border-[var(--color-border)]
              text-[var(--color-text-primary)]
              focus:outline-none focus:ring-1 focus:ring-[var(--color-accent)]
              ${nameInvalid && form.name.length > 0 ? 'border-red-500/60' : ''}
            `}
            autoFocus={!isEdit}
            maxLength={120}
          />
        </div>

        {/* Prompt */}
        <div className="flex flex-col gap-1.5">
          <label className="text-xs font-medium uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
            Research prompt
          </label>
          <textarea
            value={form.research_prompt}
            onChange={(e) => setForm(f => ({ ...f, research_prompt: e.target.value }))}
            placeholder="What is this report supposed to do? E.g. 'Summarize today's AI articles, calling out contradictions with prior coverage.'"
            rows={5}
            className={`
              px-3 py-2 rounded-md text-sm font-mono leading-relaxed resize-y
              bg-[var(--color-bg-input)] border border-[var(--color-border)]
              text-[var(--color-text-primary)]
              focus:outline-none focus:ring-1 focus:ring-[var(--color-accent)]
              ${promptInvalid && form.research_prompt.length > 0 ? 'border-red-500/60' : ''}
            `}
          />
          <span className="text-[11px] text-[var(--color-text-tertiary)]">
            The agent reads this verbatim. Be specific about scope, tone, and what counts as a citation.
          </span>
        </div>

        {/* Schedule */}
        <ScheduleField
          cron={form.schedule}
          tz={form.schedule_tz}
          onChange={(cron, tz) => setForm(f => ({ ...f, schedule: cron, schedule_tz: tz }))}
        />

        {/* Enabled */}
        <label className="flex items-center gap-2 text-sm cursor-pointer">
          <input
            type="checkbox"
            checked={form.enabled}
            onChange={(e) => setForm(f => ({ ...f, enabled: e.target.checked }))}
            className="accent-[var(--color-accent)]"
          />
          <span className="text-[var(--color-text-primary)]">Enabled</span>
          <span className="text-[11px] text-[var(--color-text-tertiary)]">
            (paused reports keep their schedule but don't run)
          </span>
        </label>

        {/* Advanced expander */}
        <button
          type="button"
          onClick={() => setShowAdvanced(s => !s)}
          className="
            self-start flex items-center gap-1.5 text-xs font-medium uppercase tracking-[0.1em]
            text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)] transition-colors
          "
        >
          {showAdvanced ? <ChevronDown className="w-3.5 h-3.5" /> : <ChevronRight className="w-3.5 h-3.5" />}
          Advanced
        </button>

        {showAdvanced && (
          <div className="flex flex-col gap-5 pl-3 border-l border-[var(--color-border)]">
            {/* Source scope */}
            <ScopeField
              label="Source scope"
              tagIds={form.source_tag_ids}
              window={form.source_window}
              onChange={(ids, w) => setForm(f => ({ ...f, source_tag_ids: ids, source_window: w }))}
            />

            {/* Context scope: same-as-source / custom */}
            <div className="flex flex-col gap-2">
              <label className="text-xs font-medium uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
                Context scope
              </label>
              <div className="flex flex-wrap items-center gap-3 text-sm">
                <label className="flex items-center gap-1.5 cursor-pointer">
                  <input
                    type="radio"
                    checked={form.context_mode === 'same_as_source'}
                    onChange={() => setForm(f => ({ ...f, context_mode: 'same_as_source' }))}
                    className="accent-[var(--color-accent)]"
                  />
                  <span>Same as source</span>
                </label>
                <label className="flex items-center gap-1.5 cursor-pointer">
                  <input
                    type="radio"
                    checked={form.context_mode === 'all'}
                    onChange={() => setForm(f => ({ ...f, context_mode: 'all' }))}
                    className="accent-[var(--color-accent)]"
                  />
                  <span>All atoms</span>
                </label>
                <label className="flex items-center gap-1.5 cursor-pointer">
                  <input
                    type="radio"
                    checked={form.context_mode === 'tags'}
                    onChange={() => setForm(f => ({ ...f, context_mode: 'tags' }))}
                    className="accent-[var(--color-accent)]"
                  />
                  <span>Custom tags</span>
                </label>
                <label className="flex items-center gap-1.5 cursor-pointer">
                  <input
                    type="radio"
                    checked={form.context_mode === 'none'}
                    onChange={() => setForm(f => ({ ...f, context_mode: 'none' }))}
                    className="accent-[var(--color-accent)]"
                  />
                  <span>No context</span>
                </label>
              </div>
              {form.context_mode === 'tags' && (
                <ScopeField
                  label="Context tags"
                  tagIds={form.context_tag_ids}
                  window={form.context_window}
                  onChange={(ids, w) => setForm(f => ({ ...f, context_tag_ids: ids, context_window: w }))}
                  hideSinceLastRun
                />
              )}
            </div>

            {/* Citation policy */}
            <CitationPolicyField
              value={form.citation_policy}
              onChange={(next) => setForm(f => ({ ...f, citation_policy: next }))}
            />

            {/* Output tags */}
            <div className="flex flex-col gap-2">
              <label className="text-xs font-medium uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]">
                Output tags
                <span className="ml-2 normal-case text-[10px] tracking-normal text-[var(--color-text-tertiary)]">
                  applied to each finding atom
                </span>
              </label>
              <TagSelector
                selectedTags={outputTagObjs}
                onTagsChange={(next) => setForm(f => ({ ...f, output_atom_tags: next.map(t => t.id) }))}
              />
            </div>
          </div>
        )}
      </div>
    </Modal>
  );
}
