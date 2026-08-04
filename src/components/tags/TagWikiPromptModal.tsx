import { useEffect, useState } from 'react';
import { ChevronDown, ChevronRight, Loader2 } from 'lucide-react';
import { Modal } from '../ui/Modal';
import { useTagsStore, Tag } from '../../stores/tags';
import { useSettingsStore } from '../../stores/settings';

interface TagWikiPromptModalProps {
  isOpen: boolean;
  /// The tag whose prompts are being edited; null while the modal is closed.
  tag: Pick<Tag, 'id' | 'name'> | null;
  onClose: () => void;
}

const TEXTAREA_CLASS = `
  px-3 py-2 rounded-md text-sm font-mono leading-relaxed resize-y
  bg-[var(--color-bg-main)] border border-[var(--color-border)]
  text-[var(--color-text-primary)] placeholder:text-[var(--color-text-secondary)]/40
  focus:outline-none focus:ring-1 focus:ring-[var(--color-accent)]
`;

const LABEL_CLASS = 'text-xs font-medium uppercase tracking-[0.1em] text-[var(--color-text-tertiary)]';

/// How far one tag's prompts got in loading. The tag id is part of the state
/// rather than a sibling flag on purpose: the modal stays mounted across
/// open/close, so the previous tag's load outcome is still in hand for the
/// render that happens before the load effect runs. Every read below matches
/// `tagId` against the tag in the title, which is what makes it impossible to
/// show — or save — one tag's prompts under another tag's name.
type Load =
  | { tagId: string; status: 'loading' }
  | { tagId: string; status: 'ready' }
  | { tagId: string; status: 'failed'; message: string };

/// The transport throws the raw error body, which is `''` for an error
/// response with no body — that renders as nothing, leaving the user in front
/// of a modal that refuses to save and won't say why. Give every message a floor.
function errorMessage(e: unknown): string {
  return String(e) || 'Request failed';
}

/// What an empty field falls back to. Settings → Prompts holds the global
/// prompts; when one of those is empty too, Atomic's built-in prompt runs.
/// Until settings have loaded we don't know which of the two it is, so the
/// copy names both rather than claiming a fallback that may not be the real one.
function fallbackLabel(globalPrompt: string | undefined, settingsLoaded: boolean): string {
  if (!settingsLoaded) return "the global prompt from Settings → Prompts, or Atomic's built-in one";
  return globalPrompt?.trim()
    ? 'the global prompt from Settings → Prompts'
    : "Atomic's built-in prompt";
}

export function TagWikiPromptModal({ isOpen, tag, onClose }: TagWikiPromptModalProps) {
  const fetchTagWikiPrompts = useTagsStore(s => s.fetchTagWikiPrompts);
  const saveTagWikiPrompts = useTagsStore(s => s.saveTagWikiPrompts);
  const settings = useSettingsStore(s => s.settings);
  const fetchSettings = useSettingsStore(s => s.fetchSettings);

  const [load, setLoad] = useState<Load | null>(null);
  const [generationPrompt, setGenerationPrompt] = useState('');
  const [updatePrompt, setUpdatePrompt] = useState('');
  const [showUpdatePrompt, setShowUpdatePrompt] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  const tagId = tag?.id ?? null;
  /// Only the open tag's load reaches the UI; any other one is left over.
  const currentLoad = load && load.tagId === tagId ? load : null;
  /// The single gate. True only when the fields hold *this* tag's stored
  /// prompts, so both the form and Save are off until then — a save is a full
  /// replace, and replacing with fields we never filled would wipe prompts the
  /// user never saw.
  const isLoaded = currentLoad?.status === 'ready';

  // Prompts aren't carried on the tag tree, so they are read fresh every time
  // the modal opens. Everything the previous tag left behind is cleared before
  // the fetch, and only the success branch claims the fields for this tag.
  useEffect(() => {
    // Closing drops the load outright — the component stays mounted, and
    // nothing one tag left behind may reach the next open.
    if (!isOpen || !tagId) {
      setLoad(null);
      return;
    }
    let cancelled = false;
    setLoad({ tagId, status: 'loading' });
    setGenerationPrompt('');
    setUpdatePrompt('');
    setShowUpdatePrompt(false);
    setSaveError(null);
    fetchTagWikiPrompts(tagId)
      .then((prompts) => {
        if (cancelled) return;
        setGenerationPrompt(prompts.generation_prompt ?? '');
        setUpdatePrompt(prompts.update_prompt ?? '');
        // Unfold the secondary field when this tag already overrides it,
        // so an existing value is never hidden behind a disclosure.
        setShowUpdatePrompt(Boolean(prompts.update_prompt?.trim()));
        setLoad({ tagId, status: 'ready' });
      })
      .catch((e) => {
        if (!cancelled) setLoad({ tagId, status: 'failed', message: errorMessage(e) });
      });
    return () => {
      cancelled = true;
    };
  }, [isOpen, tagId, fetchTagWikiPrompts]);

  // The fallback copy names the user's global prompts, which live in the
  // settings store — empty until someone fetches them, and Settings may never
  // have been opened this session.
  useEffect(() => {
    if (isOpen) void fetchSettings();
  }, [isOpen, fetchSettings]);

  // The store starts empty and only a successful fetch fills it, so a
  // non-empty map means the global prompts are known.
  const settingsLoaded = Object.keys(settings).length > 0;

  const handleSave = async () => {
    if (!tagId || !isLoaded || isSaving) return;
    setIsSaving(true);
    setSaveError(null);
    try {
      // Blank fields go over as-is; the server reads them as "clear this
      // override".
      await saveTagWikiPrompts(tagId, {
        generation_prompt: generationPrompt,
        update_prompt: updatePrompt,
      });
      onClose();
    } catch (e) {
      // Keep the modal open so the user doesn't lose what they typed.
      setSaveError(errorMessage(e));
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <Modal
      isOpen={isOpen}
      onClose={onClose}
      title={tag ? `Wiki Prompt for "${tag.name}"` : 'Wiki Prompt'}
      width="lg"
      confirmLabel={isSaving ? 'Saving…' : 'Save prompts'}
      onConfirm={handleSave}
      confirmDisabled={!isLoaded || isSaving}
    >
      {isLoaded ? (
        <div className="flex flex-col gap-5">
          {/* Generation prompt — the reason anyone opens this modal. */}
          <div className="flex flex-col gap-1.5">
            <label className={LABEL_CLASS}>Generation prompt</label>
            <p className="text-xs text-[var(--color-text-secondary)]">
              Replaces the prompt used to write this tag's article. Leave empty to use{' '}
              {fallbackLabel(settings.wiki_generation_prompt, settingsLoaded)}.
            </p>
            <textarea
              value={generationPrompt}
              onChange={(e) => setGenerationPrompt(e.target.value)}
              placeholder="e.g. Collect every unchecked task from these notes into one checklist, grouped by project. Skip anything already done."
              rows={10}
              className={TEXTAREA_CLASS}
              autoFocus
            />
          </div>

          {/* Update prompt — secondary, folded away until it's in play. */}
          <div className="flex flex-col gap-2">
            <button
              type="button"
              onClick={() => setShowUpdatePrompt(s => !s)}
              className={`self-start flex items-center gap-1.5 ${LABEL_CLASS} hover:text-[var(--color-text-primary)] transition-colors`}
            >
              {showUpdatePrompt ? <ChevronDown className="w-3.5 h-3.5" /> : <ChevronRight className="w-3.5 h-3.5" />}
              Update prompt
            </button>
            {showUpdatePrompt && (
              <div className="flex flex-col gap-1.5 pl-3 border-l border-[var(--color-border)]">
                <p className="text-xs text-[var(--color-text-secondary)]">
                  Added to the prompt when new atoms are folded into the existing article. Leave empty and
                  the generation prompt above steers updates too; if that's empty as well, it falls back to{' '}
                  {fallbackLabel(settings.wiki_update_prompt, settingsLoaded)}.
                </p>
                <textarea
                  value={updatePrompt}
                  onChange={(e) => setUpdatePrompt(e.target.value)}
                  placeholder="e.g. Keep the checklist grouped by project, and drop tasks that are now done."
                  rows={4}
                  className={TEXTAREA_CLASS}
                />
              </div>
            )}
          </div>

          {saveError && <p className="text-xs text-red-400">{saveError}</p>}

          <p className="text-[11px] text-[var(--color-text-tertiary)]">
            Saving doesn't rewrite anything. These prompts apply the next time this article is generated or
            updated.
          </p>
        </div>
      ) : currentLoad?.status === 'failed' ? (
        <p className="py-10 text-center text-sm text-red-400">{currentLoad.message}</p>
      ) : (
        <div className="flex items-center justify-center gap-2 py-10 text-sm text-[var(--color-text-secondary)]">
          <Loader2 className="w-4 h-4 animate-spin" strokeWidth={2} />
          Loading prompts…
        </div>
      )}
    </Modal>
  );
}
