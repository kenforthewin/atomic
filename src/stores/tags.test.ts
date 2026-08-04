import { describe, it, expect, beforeEach, vi } from 'vitest';
import { useTagsStore, type TagWikiPrompts } from './tags';

const invoke = vi.fn();
vi.mock('../lib/transport', () => ({
  getTransport: () => ({ invoke, subscribe: () => () => {} }),
}));

/// The per-tag wiki prompt actions. They are deliberately cache-free: the
/// prompts never ride the tag tree, so a stale copy in the store would be a
/// liability rather than a speed-up.

const TAG = 'tag-1';

function prompts(generation: string | null, update: string | null): TagWikiPrompts {
  return { generation_prompt: generation, update_prompt: update };
}

describe('tag wiki prompts', () => {
  beforeEach(() => {
    invoke.mockReset();
    useTagsStore.getState().reset();
  });

  it('reads a tag\'s prompts on demand', async () => {
    invoke.mockResolvedValue(prompts('write a checklist', null));

    const result = await useTagsStore.getState().fetchTagWikiPrompts(TAG);

    expect(invoke).toHaveBeenCalledWith('get_tag_wiki_prompts', { id: TAG });
    expect(result).toEqual(prompts('write a checklist', null));
    // Nothing about the tree changed, so nothing was refetched.
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it('saves both prompts and returns what the server stored', async () => {
    // Blank fields clear the override; the server answers with the
    // normalized record, which is what callers should render.
    invoke.mockResolvedValue(prompts('write a checklist', null));

    const result = await useTagsStore
      .getState()
      .saveTagWikiPrompts(TAG, prompts('write a checklist', '   '));

    expect(invoke).toHaveBeenCalledWith('set_tag_wiki_prompts', {
      id: TAG,
      generation_prompt: 'write a checklist',
      update_prompt: '   ',
    });
    expect(result).toEqual(prompts('write a checklist', null));
  });

  it('writes the tag the caller named, not one riding on the payload', async () => {
    // The prompts object is caller data; a stray `id` on it must not be able
    // to redirect the write onto a different tag.
    invoke.mockResolvedValue(prompts('write a checklist', null));

    await useTagsStore
      .getState()
      .saveTagWikiPrompts(TAG, { ...prompts('write a checklist', null), id: 'other-tag' } as TagWikiPrompts);

    expect(invoke).toHaveBeenCalledWith('set_tag_wiki_prompts', {
      id: TAG,
      generation_prompt: 'write a checklist',
      update_prompt: null,
    });
  });

  it('surfaces failures to the caller and to the store', async () => {
    invoke.mockRejectedValue(new Error('tag not found'));

    await expect(useTagsStore.getState().fetchTagWikiPrompts(TAG)).rejects.toThrow('tag not found');
    expect(useTagsStore.getState().error).toContain('tag not found');

    await expect(
      useTagsStore.getState().saveTagWikiPrompts(TAG, prompts('x', null)),
    ).rejects.toThrow('tag not found');
    expect(useTagsStore.getState().error).toContain('tag not found');
  });

  it('rejects with whatever the transport threw, including a bare string', async () => {
    // The HTTP transport throws the raw error body rather than an Error, and
    // an error response with no body throws `''`. UI that renders the message
    // has to supply its own fallback text — see TagWikiPromptModal.
    invoke.mockRejectedValue('');

    await expect(useTagsStore.getState().fetchTagWikiPrompts(TAG)).rejects.toBe('');
    expect(useTagsStore.getState().error).toBe('');
  });
});
