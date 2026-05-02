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
  | { kind: 'content_regex'; pattern: string; invert?: boolean }
  | { kind: 'require_tag'; any_of: string[]; tag_filter?: string | null }
  | { kind: 'content_length'; min_words: number; max_words: number; tag_filter?: string | null }
  | { kind: 'citation_count'; min_citations: number; tag_filter?: string | null }
  | { kind: 'source_domain_matches'; domains: string[]; mode: 'allowlist' | 'blocklist'; tag_filter?: string | null }
  | { kind: 'stale_atom'; tag: string; max_age_days: number }
  | { kind: 'forbidden_tag_combo'; all_of: string[] }
  | { kind: 'missing_heading'; min_length_chars: number; tag_filter?: string | null }
  | { kind: 'tag_cardinality'; min: number; max: number; tag_filter?: string | null };

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
                  case 'require_tag':
                    rule = { kind, any_of: [], tag_filter: null };
                    break;
                  case 'content_length':
                    rule = { kind, min_words: 0, max_words: 0, tag_filter: null };
                    break;
                  case 'citation_count':
                    rule = { kind, min_citations: 1, tag_filter: null };
                    break;
                  case 'source_domain_matches':
                    rule = { kind, domains: [], mode: 'allowlist', tag_filter: null };
                    break;
                  case 'stale_atom':
                    rule = { kind, tag: '', max_age_days: 14 };
                    break;
                  case 'forbidden_tag_combo':
                    rule = { kind, all_of: [] };
                    break;
                  case 'missing_heading':
                    rule = { kind, min_length_chars: 120, tag_filter: null };
                    break;
                  case 'tag_cardinality':
                    rule = { kind, min: 1, max: 5, tag_filter: null };
                    break;
                }
                mutateRule(check.id, rule);
              }}
              className="bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
            >
              <optgroup label="Tags">
                <option value="tag_requires">Tag requires other tags</option>
                <option value="require_tag">Atoms must carry at least one tag</option>
                <option value="forbidden_tag_combo">Forbidden tag combination</option>
                <option value="tag_cardinality">Tag count bounds</option>
              </optgroup>
              <optgroup label="Sources">
                <option value="require_source">Require source URL</option>
                <option value="source_domain_matches">Source domain allowlist / blocklist</option>
              </optgroup>
              <optgroup label="Content">
                <option value="content_regex">Content matches regex</option>
                <option value="content_length">Content word-count bounds</option>
                <option value="citation_count">Minimum inline citations</option>
                <option value="missing_heading">Missing markdown heading</option>
              </optgroup>
              <optgroup label="Workflow">
                <option value="stale_atom">Stale atom (age + tag)</option>
              </optgroup>
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

  if (rule.kind === 'content_regex') {
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

  if (rule.kind === 'require_tag') {
    return (
      <div className="space-y-2">
        <TagMultiPicker
          label="Require:"
          selected={rule.any_of}
          onChange={any_of => onChange({ ...rule, any_of })}
          tagsById={tagsById}
          tags={tags}
        />
        <TagFilterRow
          value={rule.tag_filter ?? null}
          onChange={v => onChange({ ...rule, tag_filter: v })}
          tags={tags}
          label="Scope:"
        />
      </div>
    );
  }

  if (rule.kind === 'content_length') {
    return (
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-500 w-16 shrink-0">Words:</label>
          <NumberField value={rule.min_words} onChange={v => onChange({ ...rule, min_words: v })} placeholder="min" />
          <span className="text-xs text-gray-600">to</span>
          <NumberField value={rule.max_words} onChange={v => onChange({ ...rule, max_words: v })} placeholder="max" />
          <span className="text-xs text-gray-600">(0 = unbounded)</span>
        </div>
        <TagFilterRow
          value={rule.tag_filter ?? null}
          onChange={v => onChange({ ...rule, tag_filter: v })}
          tags={tags}
          label="Scope:"
        />
      </div>
    );
  }

  if (rule.kind === 'citation_count') {
    return (
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-500 w-16 shrink-0">Min:</label>
          <NumberField value={rule.min_citations} onChange={v => onChange({ ...rule, min_citations: v })} placeholder="1" />
          <span className="text-xs text-gray-600">inline links or wikilinks</span>
        </div>
        <TagFilterRow
          value={rule.tag_filter ?? null}
          onChange={v => onChange({ ...rule, tag_filter: v })}
          tags={tags}
          label="Scope:"
        />
      </div>
    );
  }

  if (rule.kind === 'source_domain_matches') {
    return (
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-500 w-16 shrink-0">Mode:</label>
          <select
            value={rule.mode}
            onChange={e => onChange({ ...rule, mode: e.target.value as 'allowlist' | 'blocklist' })}
            className="bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
          >
            <option value="allowlist">Allowlist (flag off-list)</option>
            <option value="blocklist">Blocklist (flag on-list)</option>
          </select>
        </div>
        <DomainListInput
          domains={rule.domains}
          onChange={domains => onChange({ ...rule, domains })}
        />
        <TagFilterRow
          value={rule.tag_filter ?? null}
          onChange={v => onChange({ ...rule, tag_filter: v })}
          tags={tags}
          label="Scope:"
        />
      </div>
    );
  }

  if (rule.kind === 'stale_atom') {
    return (
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-500 w-16 shrink-0">Tag:</label>
          <select
            value={rule.tag}
            onChange={e => onChange({ ...rule, tag: e.target.value })}
            className="flex-1 bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
          >
            <option value="">— select a tag —</option>
            {tags.map(t => <option key={t.id} value={t.id}>{t.name}</option>)}
          </select>
        </div>
        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-500 w-16 shrink-0">Max age:</label>
          <NumberField value={rule.max_age_days} onChange={v => onChange({ ...rule, max_age_days: v })} placeholder="14" />
          <span className="text-xs text-gray-600">days</span>
        </div>
      </div>
    );
  }

  if (rule.kind === 'forbidden_tag_combo') {
    return (
      <TagMultiPicker
        label="Combo:"
        selected={rule.all_of}
        onChange={all_of => onChange({ ...rule, all_of })}
        tagsById={tagsById}
        tags={tags}
      />
    );
  }

  if (rule.kind === 'missing_heading') {
    return (
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-500 w-16 shrink-0">Min size:</label>
          <NumberField value={rule.min_length_chars} onChange={v => onChange({ ...rule, min_length_chars: v })} placeholder="120" />
          <span className="text-xs text-gray-600">chars (shorter atoms skipped)</span>
        </div>
        <TagFilterRow
          value={rule.tag_filter ?? null}
          onChange={v => onChange({ ...rule, tag_filter: v })}
          tags={tags}
          label="Scope:"
        />
      </div>
    );
  }

  if (rule.kind === 'tag_cardinality') {
    return (
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <label className="text-xs text-gray-500 w-16 shrink-0">Tags:</label>
          <NumberField value={rule.min} onChange={v => onChange({ ...rule, min: v })} placeholder="min" />
          <span className="text-xs text-gray-600">to</span>
          <NumberField value={rule.max} onChange={v => onChange({ ...rule, max: v })} placeholder="max" />
          <span className="text-xs text-gray-600">(0 = unbounded)</span>
        </div>
        <TagFilterRow
          value={rule.tag_filter ?? null}
          onChange={v => onChange({ ...rule, tag_filter: v })}
          tags={tags}
          label="Scope:"
        />
      </div>
    );
  }

  return null;
}

function NumberField({
  value,
  onChange,
  placeholder,
}: {
  value: number;
  onChange: (v: number) => void;
  placeholder?: string;
}) {
  return (
    <input
      type="number"
      min={0}
      value={value}
      onChange={e => onChange(parseInt(e.target.value || '0', 10) || 0)}
      placeholder={placeholder}
      className="w-20 bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
    />
  );
}

function TagFilterRow({
  value,
  onChange,
  tags,
  label,
}: {
  value: string | null;
  onChange: (v: string | null) => void;
  tags: { id: string; name: string }[];
  label: string;
}) {
  return (
    <div className="flex items-center gap-2">
      <label className="text-xs text-gray-500 w-16 shrink-0">{label}</label>
      <select
        value={value ?? ''}
        onChange={e => onChange(e.target.value || null)}
        className="flex-1 bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200"
      >
        <option value="">— all atoms —</option>
        {tags.map(t => (<option key={t.id} value={t.id}>{t.name}</option>))}
      </select>
    </div>
  );
}

function DomainListInput({
  domains,
  onChange,
}: {
  domains: string[];
  onChange: (d: string[]) => void;
}) {
  const [draft, setDraft] = useState('');
  const add = () => {
    const v = draft.trim().toLowerCase();
    if (!v || domains.includes(v)) return;
    onChange([...domains, v]);
    setDraft('');
  };
  return (
    <div className="flex items-start gap-2">
      <label className="text-xs text-gray-500 w-16 shrink-0 pt-1">Domains:</label>
      <div className="flex-1 space-y-1">
        <div className="flex flex-wrap gap-1">
          {domains.map(d => (
            <span key={d} className="inline-flex items-center gap-1 px-2 py-0.5 bg-purple-600/20 border border-purple-600/40 rounded text-xs text-purple-200 font-mono">
              {d}
              <button
                onClick={() => onChange(domains.filter(x => x !== d))}
                className="text-purple-300/70 hover:text-red-300 text-xs leading-none"
                aria-label={`Remove ${d}`}
              >×</button>
            </span>
          ))}
        </div>
        <div className="flex gap-1">
          <input
            type="text"
            value={draft}
            onChange={e => setDraft(e.target.value)}
            onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); add(); } }}
            placeholder="arxiv.org"
            className="flex-1 bg-[#252525] border border-white/5 rounded px-2 py-1 text-xs text-gray-200 font-mono"
          />
          <button
            onClick={add}
            className="px-2 py-1 bg-[#2d2d2d] hover:bg-[#3a3a3a] rounded text-xs text-gray-200"
          >Add</button>
        </div>
      </div>
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
