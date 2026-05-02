import { useEffect, useState, useMemo } from 'react';
import { Plus, Trash2, Save, Loader2, AlertTriangle } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import { toast } from '../../stores/toasts';
import { useTagsStore } from '../../stores/tags';

/**
 * Shape mirrors `crates/atomic-core/src/health/custom.rs`. The Rust layer is
 * the source of truth — any new rule kind lands there first and this enum
 * gets the matching arm.
 */
type CustomRule =
  | { kind: 'tag_requires'; any_of: string[]; required: string[] }
  | { kind: 'require_source'; tag_filter?: string | null }
  | { kind: 'content_regex'; pattern: string; invert?: boolean };

interface CustomCheck {
  id: string;
  label: string;
  description: string;
  enabled: boolean;
  /** 0 = informational (not scored). > 0 contributes to the overall score. */
  weight: number;
  rule: CustomRule;
}

interface GetResponse {
  checks: CustomCheck[];
}

function uuid(): string {
  // Good-enough client uuid — the server doesn't enforce format.
  return 'c_' + Math.random().toString(36).slice(2, 10) + Date.now().toString(36);
}

function blankCheck(): CustomCheck {
  return {
    id: uuid(),
    label: 'New check',
    description: '',
    enabled: true,
    weight: 0,
    rule: { kind: 'require_source', tag_filter: null },
  };
}

export function CustomChecksPanel() {
  const [checks, setChecks] = useState<CustomCheck[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);
  const tags = useTagsStore(s => s.tags);
  const tagsById = useMemo(() => {
    const m: Record<string, string> = {};
    for (const t of tags) m[t.id] = t.name;
    return m;
  }, [tags]);

  useEffect(() => {
    void (async () => {
      try {
        const res = await getTransport().invoke<GetResponse>('get_custom_health_checks', {});
        setChecks(res?.checks ?? []);
      } catch (err) {
        console.error('load custom checks', err);
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  const mutate = (id: string, patch: Partial<CustomCheck>) => {
    setChecks(curr => curr.map(c => (c.id === id ? { ...c, ...patch } : c)));
    setDirty(true);
  };

  const mutateRule = (id: string, rule: CustomRule) => mutate(id, { rule });

  const addCheck = () => {
    setChecks(curr => [...curr, blankCheck()]);
    setDirty(true);
  };

  const removeCheck = (id: string) => {
    setChecks(curr => curr.filter(c => c.id !== id));
    setDirty(true);
  };

  const save = async () => {
    setSaving(true);
    try {
      // Normalize weights: clamp to [0, 1]; reject NaN.
      const cleaned = checks.map(c => ({
        ...c,
        weight: Number.isFinite(c.weight) ? Math.max(0, Math.min(1, c.weight)) : 0,
        label: c.label.trim() || 'Unnamed check',
      }));
      await getTransport().invoke('set_custom_health_checks', { checks: cleaned });
      setChecks(cleaned);
      setDirty(false);
      toast.success(
        cleaned.length === 0
          ? 'All custom checks removed'
          : `${cleaned.length} custom check${cleaned.length === 1 ? '' : 's'} saved`,
      );
    } catch (err) {
      const detail = err instanceof Error ? err.message : String(err);
      toast.error('Save custom checks failed', { detail, retry: () => { void save(); } });
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return (
      <div className="flex items-center gap-2 text-xs text-gray-500 p-4">
        <Loader2 className="w-3.5 h-3.5 animate-spin" /> Loading custom checks…
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {checks.length === 0 && (
        <div className="rounded border border-white/5 p-4 text-xs text-gray-500">
          No custom checks yet. Click <strong>Add check</strong> to define a rule.
        </div>
      )}

      {checks.map(check => (
        <div key={check.id} className="rounded border border-white/5 p-3 space-y-3 bg-[#1e1e1e]">
          {/* Header: label + enabled + delete */}
          <div className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={check.enabled}
              onChange={e => mutate(check.id, { enabled: e.target.checked })}
              className="shrink-0"
              aria-label="Enabled"
            />
            <input
              type="text"
              value={check.label}
              onChange={e => mutate(check.id, { label: e.target.value })}
              className="flex-1 bg-[#252525] border border-white/5 rounded px-2 py-1 text-sm text-gray-200"
              placeholder="Check label"
            />
            <button
              onClick={() => removeCheck(check.id)}
              className="p-1.5 text-gray-500 hover:text-red-400 transition-colors"
              title="Delete check"
              aria-label="Delete"
            >
              <Trash2 className="w-4 h-4" />
            </button>
          </div>

          {/* Description */}
          <input
            type="text"
            value={check.description}
            onChange={e => mutate(check.id, { description: e.target.value })}
            className="w-full bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-400"
            placeholder="Description (optional)"
          />

          {/* Rule kind selector */}
          <div className="flex items-center gap-2">
            <label className="text-xs text-gray-500 w-16 shrink-0">Rule:</label>
            <select
              value={check.rule.kind}
              onChange={e => {
                const kind = e.target.value as CustomRule['kind'];
                let rule: CustomRule;
                switch (kind) {
                  case 'require_source':
                    rule = { kind, tag_filter: null };
                    break;
                  case 'tag_requires':
                    rule = { kind, any_of: [], required: [] };
                    break;
                  case 'content_regex':
                    rule = { kind, pattern: '', invert: false };
                    break;
                }
                mutateRule(check.id, rule);
              }}
              className="bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
            >
              <option value="require_source">Require source URL</option>
              <option value="tag_requires">Tag requires other tags</option>
              <option value="content_regex">Content matches regex</option>
            </select>
          </div>

          {/* Rule params */}
          <RuleEditor rule={check.rule} onChange={r => mutateRule(check.id, r)} tagsById={tagsById} tags={tags.map(t => ({ id: t.id, name: t.name }))} />

          {/* Weight */}
          <div className="flex items-center gap-2 pt-1">
            <label className="text-xs text-gray-500 w-16 shrink-0">Weight:</label>
            <input
              type="number"
              step="0.05"
              min="0"
              max="1"
              value={check.weight}
              onChange={e => mutate(check.id, { weight: parseFloat(e.target.value) || 0 })}
              className="w-20 bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
            />
            <span className="text-xs text-gray-600">
              {check.weight <= 0
                ? '(0 = informational — not scored)'
                : `contributes ${(check.weight * 100).toFixed(0)}% alongside built-ins`}
            </span>
          </div>
        </div>
      ))}

      {/* Actions */}
      <div className="flex items-center justify-between pt-2 border-t border-white/5">
        <button
          onClick={addCheck}
          className="flex items-center gap-1.5 px-3 py-1.5 bg-[#2d2d2d] hover:bg-[#3a3a3a] rounded text-xs text-gray-200 transition-colors"
        >
          <Plus className="w-3 h-3" /> Add check
        </button>
        <button
          onClick={save}
          disabled={saving || !dirty}
          className="flex items-center gap-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 disabled:bg-[#3a3a3a] disabled:text-gray-600 rounded text-xs text-white transition-colors"
        >
          {saving ? <Loader2 className="w-3 h-3 animate-spin" /> : <Save className="w-3 h-3" />}
          {dirty ? 'Save changes' : 'Saved'}
        </button>
      </div>
    </div>
  );
}

function RuleEditor({
  rule,
  onChange,
  tagsById,
  tags,
}: {
  rule: CustomRule;
  onChange: (r: CustomRule) => void;
  tagsById: Record<string, string>;
  tags: { id: string; name: string }[];
}) {
  if (rule.kind === 'require_source') {
    return (
      <div className="flex items-center gap-2">
        <label className="text-xs text-gray-500 w-16 shrink-0">Only tag:</label>
        <select
          value={rule.tag_filter ?? ''}
          onChange={e => onChange({ kind: 'require_source', tag_filter: e.target.value || null })}
          className="flex-1 bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
        >
          <option value="">— all atoms —</option>
          {tags.map(t => (
            <option key={t.id} value={t.id}>{t.name}</option>
          ))}
        </select>
      </div>
    );
  }

  if (rule.kind === 'tag_requires') {
    return (
      <div className="space-y-2">
        <TagMultiPicker
          label="If tagged:"
          selected={rule.any_of}
          onChange={any_of => onChange({ ...rule, any_of })}
          tagsById={tagsById}
          tags={tags}
        />
        <TagMultiPicker
          label="Must have:"
          selected={rule.required}
          onChange={required => onChange({ ...rule, required })}
          tagsById={tagsById}
          tags={tags}
        />
      </div>
    );
  }

  // content_regex
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2">
        <label className="text-xs text-gray-500 w-16 shrink-0">Pattern:</label>
        <input
          type="text"
          value={rule.pattern}
          onChange={e => onChange({ ...rule, pattern: e.target.value })}
          className="flex-1 bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200 font-mono"
          placeholder="\\bTODO\\b"
        />
      </div>
      <label className="flex items-center gap-2 text-xs text-gray-400">
        <input
          type="checkbox"
          checked={rule.invert ?? false}
          onChange={e => onChange({ ...rule, invert: e.target.checked })}
        />
        Flag atoms that do NOT match (invert)
      </label>
      {rule.pattern.length > 512 && (
        <div className="flex items-center gap-1 text-xs text-red-400">
          <AlertTriangle className="w-3 h-3" /> Pattern too long (max 512)
        </div>
      )}
    </div>
  );
}

function TagMultiPicker({
  label,
  selected,
  onChange,
  tagsById,
  tags,
}: {
  label: string;
  selected: string[];
  onChange: (v: string[]) => void;
  tagsById: Record<string, string>;
  tags: { id: string; name: string }[];
}) {
  const [draft, setDraft] = useState('');
  const add = (id: string) => {
    if (!id || selected.includes(id)) return;
    onChange([...selected, id]);
    setDraft('');
  };
  const remove = (id: string) => onChange(selected.filter(t => t !== id));
  const options = tags.filter(t => !selected.includes(t.id));

  return (
    <div className="flex items-start gap-2">
      <label className="text-xs text-gray-500 w-16 shrink-0 pt-1">{label}</label>
      <div className="flex-1 space-y-1">
        <div className="flex flex-wrap gap-1">
          {selected.map(id => (
            <span
              key={id}
              className="inline-flex items-center gap-1 px-2 py-0.5 bg-purple-600/20 border border-purple-600/40 rounded text-xs text-purple-200"
            >
              {tagsById[id] ?? id}
              <button
                onClick={() => remove(id)}
                className="text-purple-300/70 hover:text-red-300 text-xs leading-none"
                aria-label={`Remove ${tagsById[id] ?? id}`}
              >
                ×
              </button>
            </span>
          ))}
        </div>
        <select
          value={draft}
          onChange={e => add(e.target.value)}
          className="w-full bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
        >
          <option value="">+ add tag…</option>
          {options.map(t => (
            <option key={t.id} value={t.id}>{t.name}</option>
          ))}
        </select>
      </div>
    </div>
  );
}
