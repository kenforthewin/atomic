import { useEffect, useState } from 'react';
import { Loader2, Save, BookX } from 'lucide-react';
import { getTransport } from '../../lib/transport';
import { toast } from '../../stores/toasts';
import { TagSelector } from '../tags/TagSelector';
import { useTagsStore } from '../../stores/tags';
import type { Tag } from '../../stores/atoms';

interface GetResponse {
  tag_ids: string[];
}

export function WikiExclusionPanel() {
  const tagsFromStore = useTagsStore(s => s.tags);
  const tags: Tag[] = tagsFromStore as unknown as Tag[];
  const [selected, setSelected] = useState<Tag[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        const res = await getTransport().invoke<GetResponse>('get_wiki_excluded_tags', {});
        const ids = new Set(res?.tag_ids ?? []);
        // Resolve ids against the current tag cache. Unknown ids are dropped —
        // but we keep their id around so save still round-trips cleanly even
        // for tags that haven't loaded yet.
        const matched = tags.filter(t => ids.has(t.id));
        setSelected(matched);
      } catch (err) {
        console.error('load wiki excluded tags', err);
      } finally {
        setLoading(false);
      }
    })();
    // Re-resolve when tag cache hydrates.
  }, [tags]);

  const save = async () => {
    setSaving(true);
    try {
      const tag_ids = selected.map(t => t.id);
      await getTransport().invoke('set_wiki_excluded_tags', { tag_ids });
      toast.success(
        selected.length === 0
          ? 'Wiki exclusions cleared'
          : `${selected.length} tag${selected.length === 1 ? '' : 's'} excluded from wikis`,
      );
    } catch (err) {
      const detail = err instanceof Error ? err.message : String(err);
      toast.error('Save wiki exclusions failed', {
        detail,
        retry: () => { void save(); },
      });
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="rounded border border-white/5 p-4 space-y-3">
      <div className="flex items-start gap-2">
        <BookX className="w-4 h-4 text-yellow-400 mt-0.5 shrink-0" />
        <div className="text-xs text-gray-400 leading-relaxed">
          <p className="text-gray-200 text-sm font-medium mb-1">Exclude tags from wiki generation</p>
          <p>
            Atoms carrying any of these tags will not be used as sources when generating wiki articles.
            Deterministic filter at the retrieval layer — the LLM never sees these atoms, regardless of
            the wiki prompt. Useful for personal notes, drafts, or tags that shouldn't contribute to
            synthesized articles.
          </p>
        </div>
      </div>

      {loading ? (
        <div className="flex items-center gap-2 text-xs text-gray-500">
          <Loader2 className="w-3.5 h-3.5 animate-spin" /> Loading…
        </div>
      ) : (
        <>
          <TagSelector selectedTags={selected} onTagsChange={setSelected} />
          <div className="flex items-center justify-between pt-2 border-t border-white/5">
            <p className="text-xs text-gray-600">
              {selected.length === 0
                ? 'No exclusions — all tagged atoms are eligible for wiki synthesis.'
                : `${selected.length} tag${selected.length === 1 ? '' : 's'} excluded.`}
            </p>
            <button
              onClick={save}
              disabled={saving}
              className="flex items-center gap-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 disabled:bg-[#3a3a3a] rounded text-xs text-white transition-colors"
            >
              {saving ? <Loader2 className="w-3 h-3 animate-spin" /> : <Save className="w-3 h-3" />}
              Save exclusions
            </button>
          </div>
        </>
      )}
    </div>
  );
}
