import { create } from 'zustand';
import { getTransport, isDemoInstance } from '../lib/transport';
import { saveAnswerAsAtom } from '../components/chat/saveAnswerAsAtom';
import { useUIStore } from './ui';
import { useCanvasStore } from './canvas';
import type { ChatSkill } from '../lib/chatSkills';

// ==================== Types ====================

export interface Tag {
  id: string;
  name: string;
  parent_id: string | null;
  created_at: string;
}

// The role a tag plays in a conversation's scope. An atom is in scope when it
// carries at least one `include` tag (skipped when nothing is included),
// every `require` tag, and no `exclude` tag — child tags count as their
// parent. `include` is what every tag meant before modes existed.
export type ScopeMode = 'include' | 'require' | 'exclude';

export interface ScopeTag extends Tag {
  // Optional because a server older than boolean scope answers without it.
  // Read it through `scopeModeOf` rather than defaulting at each use site.
  mode?: ScopeMode;
}

/// A scope tag's mode, defaulting to `include` — which is what every tag meant
/// before modes existed, and what a server that predates them still means.
export function scopeModeOf(tag: Pick<ScopeTag, 'mode'>): ScopeMode {
  return tag.mode ?? 'include';
}

export interface ScopeEntry {
  tag_id: string;
  mode: ScopeMode;
}

export interface Conversation {
  id: string;
  title: string | null;
  created_at: string;
  updated_at: string;
  is_archived: boolean;
}

export interface ConversationWithTags extends Conversation {
  tags: ScopeTag[];
  message_count: number;
  last_message_preview: string | null;
}

export interface ChatMessage {
  id: string;
  conversation_id: string;
  role: 'user' | 'assistant' | 'system' | 'tool';
  content: string;
  created_at: string;
  message_index: number;
}

export interface ChatToolCall {
  id: string;
  message_id: string;
  tool_name: string;
  tool_input: unknown;
  tool_output: unknown | null;
  status: 'pending' | 'running' | 'complete' | 'failed';
  created_at: string;
  completed_at: string | null;
}

// What a citation points at. `atom_id` is read according to it: an atom id,
// the tag id of a wiki article, or a finding's atom id. Citations stored
// before source types existed arrive without the field, so it is optional
// and absent means 'atom'.
export type CitationSourceType = 'atom' | 'wiki' | 'finding';

export interface ChatCitation {
  id: string;
  message_id: string;
  citation_index: number;
  atom_id: string;
  chunk_index: number | null;
  excerpt: string;
  relevance_score: number | null;
  source_type?: CitationSourceType;
  // Display name for a source an id can't name — the tag behind a wiki citation.
  source_title?: string | null;
}

export interface ChatMessageWithContext extends ChatMessage {
  tool_calls: ChatToolCall[];
  citations: ChatCitation[];
}

export interface ConversationWithMessages extends Conversation {
  tags: ScopeTag[];
  messages: ChatMessageWithContext[];
}

interface PageContext {
  view: string;
  atom_id?: string | null;
  atom_title?: string | null;
  atom_snippet?: string | null;
  wiki_tag_id?: string | null;
  wiki_tag_name?: string | null;
  selected_tag_id?: string | null;
}

function buildPageContext(): PageContext {
  const ui = useUIStore.getState();

  if (ui.readerState.atomId) {
    return {
      view: ui.readerState.editing ? 'atom_editor' : 'atom_reader',
      atom_id: ui.readerState.atomId,
      selected_tag_id: ui.selectedTagId,
    };
  }

  if (ui.wikiReaderState.tagId) {
    return {
      view: 'wiki_reader',
      wiki_tag_id: ui.wikiReaderState.tagId,
      wiki_tag_name: ui.wikiReaderState.tagName,
      selected_tag_id: ui.selectedTagId,
    };
  }

  if (ui.localGraph.isOpen && ui.localGraph.centerAtomId) {
    return {
      view: 'atom_graph',
      atom_id: ui.localGraph.centerAtomId,
      selected_tag_id: ui.selectedTagId,
    };
  }

  return {
    view: ui.viewMode,
    selected_tag_id: ui.selectedTagId,
  };
}

// ==================== Store ====================

type ChatView = 'list' | 'conversation';

interface ChatStore {
  // View state
  view: ChatView;

  // Current conversation (when view === 'conversation')
  currentConversation: ConversationWithTags | null;
  messages: ChatMessageWithContext[];

  // Conversations list
  conversations: ConversationWithTags[];
  listFilterTagId: string | null;
  /** Archived conversations are hidden until the list asks for them. */
  showArchived: boolean;

  /**
   * Answers already saved as atoms, keyed by message id. Client-session-local
   * by design: nothing records the association server-side, so a reload
   * forgets it and the action offers to save again — which writes a second
   * atom rather than touching the first.
   */
  savedAtomIdByMessageId: Record<string, string>;
  /**
   * Answers whose save is in flight, keyed by message id. Per-message rather
   * than a single slot: saving one answer must not disable the action on
   * every other answer in the conversation.
   */
  savingAnswerMessageIds: Record<string, boolean>;

  // Streaming state
  isLoading: boolean;
  /**
   * The conversation whose turn is currently streaming, or null when nothing
   * is in flight. Every other streaming field belongs to *this* conversation,
   * which is what lets the user leave a streaming turn and come back: the
   * events keep landing, the completion still clears the slot, and a
   * conversation that isn't the one streaming never renders a phantom
   * "Thinking…".
   */
  streamingConversationId: string | null;
  isStreaming: boolean;
  /**
   * A stop has been requested for the current turn. The backend finishes the
   * turn by persisting the partial answer, so streaming state stays up until
   * the completion event arrives — this only drives the button's pending look
   * and keeps repeat clicks from piling up requests.
   */
  isCancelling: boolean;
  streamingContent: string;
  streamingMessageId: string | null;
  /**
   * Tool calls fired during the current streaming turn. Populated from
   * chat-tool-start / chat-tool-complete events so the UI can render
   * collapsible cards that persist through streaming and get superseded
   * (not discarded) when the final ChatComplete message arrives.
   */
  streamingToolCalls: ChatToolCall[];

  // Error state
  error: string | null;

  // Actions - Navigation
  showList: (filterTagId?: string) => void;
  openConversation: (id: string) => Promise<void>;
  openOrCreateForTag: (tagId: string) => Promise<void>;
  goBack: () => void;

  // Actions - CRUD
  fetchConversations: (tagId?: string) => Promise<void>;
  setShowArchived: (show: boolean) => Promise<void>;
  createConversation: (tagIds?: string[]) => Promise<ConversationWithTags>;
  deleteConversation: (id: string) => Promise<void>;
  updateConversationTitle: (id: string, title: string) => Promise<void>;
  setConversationArchived: (id: string, isArchived: boolean) => Promise<void>;

  // Actions - Saving answers
  saveAnswer: (messageId: string) => Promise<void>;

  // Actions - Scope Management
  setScope: (entries: ScopeEntry[]) => Promise<void>;
  // Doubles as "set this tag's mode": a tag already in scope changes mode.
  addTagToScope: (tagId: string, mode?: ScopeMode) => Promise<void>;
  removeTagFromScope: (tagId: string) => Promise<void>;

  // Actions - Messaging
  sendMessage: (content: string, skills?: ChatSkill[]) => Promise<void>;
  cancelResponse: () => Promise<void>;

  // Actions - Streaming updates (called from event handlers).
  //
  // Every one of these takes the conversation the event was about and decides
  // for itself whether it applies: the subscription is global (see
  // `useChatEvents`), so routing lives here rather than in a component that
  // may have unmounted.
  appendStreamContent: (conversationId: string, delta: string) => void;
  startStreamingToolCall: (call: { conversation_id: string; tool_call_id: string; tool_name: string; tool_input: unknown }) => void;
  completeStreamingToolCall: (update: { conversation_id: string; tool_call_id: string; results_count: number; failed: boolean }) => void;
  completeMessage: (conversationId: string, message: ChatMessageWithContext) => void;
  setStreamingError: (conversationId: string, error: string) => void;
  applyConversationTitle: (id: string, title: string) => void;

  // Actions - Utilities
  clearError: () => void;
  reset: () => void;
}

/// Nothing in flight. The single description of "no turn is streaming", so
/// every path that ends a turn ends it the same way.
const NO_STREAM = {
  streamingConversationId: null,
  isStreaming: false,
  isCancelling: false,
  streamingContent: '',
  streamingMessageId: null,
  streamingToolCalls: [] as ChatToolCall[],
};

/// Release the streaming slot, but only if `conversationId` is the turn
/// holding it. A stale completion — a turn that was superseded, an event that
/// arrived after the request already returned — must not clear a turn that is
/// still running.
function releaseStream(state: ChatStore, conversationId: string): Partial<ChatStore> {
  return state.streamingConversationId === conversationId ? NO_STREAM : {};
}

export const useChatStore = create<ChatStore>((set, get) => ({
  // Initial state
  view: 'list',
  currentConversation: null,
  messages: [],
  conversations: [],
  listFilterTagId: null,
  showArchived: false,
  savedAtomIdByMessageId: {},
  savingAnswerMessageIds: {},
  isLoading: false,
  ...NO_STREAM,
  error: null,

  // Navigation
  showList: (filterTagId?: string) => {
    set({
      view: 'list',
      listFilterTagId: filterTagId ?? null,
      currentConversation: null,
      messages: [],
    });
    useUIStore.getState().setChatSidebarConversationId(null);
    get().fetchConversations(filterTagId);
  },

  openConversation: async (id: string) => {
    set({ isLoading: true, error: null });
    try {
      const result = await getTransport().invoke<ConversationWithMessages | null>('get_conversation', {
        conversationId: id,
      });

      if (result) {
        set({
          view: 'conversation',
          currentConversation: {
            ...result,
            message_count: result.messages.length,
            last_message_preview: result.messages.length > 0
              ? result.messages[result.messages.length - 1].content.slice(0, 100)
              : null,
          },
          messages: result.messages,
          isLoading: false,
        });
        useUIStore.getState().setChatSidebarConversationId(id);
      } else {
        // Conversation went missing (deleted, different DB, stale persisted
        // id from a prior session) — fall back to the list rather than
        // leaving the user staring at an error with no recovery path.
        set({ isLoading: false, error: null });
        get().showList();
      }
    } catch (e) {
      set({ error: String(e), isLoading: false });
    }
  },

  openOrCreateForTag: async (tagId: string) => {
    set({ isLoading: true, error: null, listFilterTagId: tagId });
    try {
      // Fetch conversations that contain this tag
      const conversations = await getTransport().invoke<ConversationWithTags[]>('get_conversations', {
        filterTagId: tagId,
        limit: 50,
        offset: 0,
      });

      // Find a conversation with exactly this one tag
      const exactMatch = conversations.find(
        (c) => c.tags.length === 1 && c.tags[0].id === tagId
      );

      if (exactMatch) {
        // Open the existing conversation
        await get().openConversation(exactMatch.id);
      } else {
        // Create a new conversation with this tag
        await get().createConversation([tagId]);
      }
    } catch (e) {
      set({ error: String(e), isLoading: false });
    }
  },

  // Leaving a conversation does not end its turn: the streaming state stays
  // keyed to `streamingConversationId`, so the turn keeps arriving, still
  // clears itself when it completes, and is waiting where the user left it if
  // they come back.
  goBack: () => {
    const { listFilterTagId } = get();
    set({ view: 'list', currentConversation: null, messages: [] });
    useUIStore.getState().setChatSidebarConversationId(null);
    get().fetchConversations(listFilterTagId ?? undefined);
  },

  // CRUD
  fetchConversations: async (tagId?: string) => {
    // Demo visitors: conversations are closed server-side (403); render an
    // empty list under the signup-CTA input instead of an error state.
    if (isDemoInstance()) {
      set({ conversations: [], isLoading: false, listFilterTagId: tagId ?? null });
      return;
    }
    set({ isLoading: true, error: null });
    try {
      const conversations = await getTransport().invoke<ConversationWithTags[]>('get_conversations', {
        filterTagId: tagId ?? null,
        limit: 50,
        offset: 0,
        includeArchived: get().showArchived,
      });
      set({ conversations, isLoading: false, listFilterTagId: tagId ?? null });
    } catch (e) {
      set({ error: String(e), isLoading: false });
    }
  },

  setShowArchived: async (show: boolean) => {
    set({ showArchived: show });
    await get().fetchConversations(get().listFilterTagId ?? undefined);
  },

  createConversation: async (tagIds?: string[]) => {
    set({ isLoading: true, error: null });
    try {
      const conversation = await getTransport().invoke<ConversationWithTags>('create_conversation', {
        tagIds: tagIds ?? [],
        title: null,
      });

      // Add to list and open it
      set((state) => ({
        conversations: [conversation, ...state.conversations],
        view: 'conversation',
        currentConversation: conversation,
        messages: [],
        isLoading: false,
      }));

      return conversation;
    } catch (e) {
      set({ error: String(e), isLoading: false });
      throw e;
    }
  },

  deleteConversation: async (id: string) => {
    try {
      await getTransport().invoke('delete_conversation', { id });
      set((state) => ({
        conversations: state.conversations.filter((c) => c.id !== id),
        // A deleted conversation has nowhere to stream to, so its turn's
        // state goes with it.
        ...releaseStream(state, id),
        // If we deleted the current conversation, go back to list
        ...(state.currentConversation?.id === id
          ? { view: 'list' as const, currentConversation: null, messages: [] }
          : {}),
      }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  updateConversationTitle: async (id: string, title: string) => {
    try {
      await getTransport().invoke('update_conversation', { id, title, isArchived: null });
      get().applyConversationTitle(id, title);
    } catch (e) {
      set({ error: String(e) });
    }
  },

  setConversationArchived: async (id: string, isArchived: boolean) => {
    try {
      await getTransport().invoke('update_conversation', { id, title: null, isArchived });
      set((state) => ({
        // The list only holds archived conversations while it is showing
        // them; otherwise archiving removes the card outright.
        conversations: state.showArchived
          ? state.conversations.map((c) => (c.id === id ? { ...c, is_archived: isArchived } : c))
          : state.conversations.filter((c) => c.id !== id),
        currentConversation:
          state.currentConversation?.id === id
            ? { ...state.currentConversation, is_archived: isArchived }
            : state.currentConversation,
      }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  // Saving answers
  saveAnswer: async (messageId: string) => {
    const { messages, currentConversation, savedAtomIdByMessageId, savingAnswerMessageIds } = get();
    // Guards this answer only: two answers can be saved at once, and a slow
    // save of one must not look like a disabled action on the rest.
    if (savedAtomIdByMessageId[messageId] || savingAnswerMessageIds[messageId]) return;

    const index = messages.findIndex((m) => m.id === messageId);
    if (index === -1) return;

    // The question that prompted the answer titles the atom. Walk back rather
    // than assuming the previous message is the user's — tool turns land in
    // the same list.
    const question = messages
      .slice(0, index)
      .reverse()
      .find((m) => m.role === 'user')?.content;

    const clearInFlight = (state: ChatStore) => {
      const { [messageId]: _saving, ...rest } = state.savingAnswerMessageIds;
      return rest;
    };

    set((state) => ({
      savingAnswerMessageIds: { ...state.savingAnswerMessageIds, [messageId]: true },
      error: null,
    }));
    try {
      const atom = await saveAnswerAsAtom({
        answer: messages[index].content,
        citations: messages[index].citations ?? [],
        question,
        conversationTitle: currentConversation?.title,
      });
      set((state) => ({
        savedAtomIdByMessageId: { ...state.savedAtomIdByMessageId, [messageId]: atom.id },
        savingAnswerMessageIds: clearInFlight(state),
      }));
    } catch (e) {
      set((state) => ({ error: String(e), savingAnswerMessageIds: clearInFlight(state) }));
    }
  },

  // Scope Management
  setScope: async (entries: ScopeEntry[]) => {
    const { currentConversation } = get();
    if (!currentConversation) return;

    try {
      const updated = await getTransport().invoke<ConversationWithTags>('set_conversation_scope', {
        conversationId: currentConversation.id,
        entries,
      });
      set({ currentConversation: updated });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  addTagToScope: async (tagId: string, mode: ScopeMode = 'include') => {
    const { currentConversation } = get();
    if (!currentConversation) return;

    try {
      const updated = await getTransport().invoke<ConversationWithTags>('add_tag_to_scope', {
        conversationId: currentConversation.id,
        tagId,
        mode,
      });
      set({ currentConversation: updated });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  removeTagFromScope: async (tagId: string) => {
    const { currentConversation } = get();
    if (!currentConversation) return;

    try {
      const updated = await getTransport().invoke<ConversationWithTags>('remove_tag_from_scope', {
        conversationId: currentConversation.id,
        tagId,
      });
      set({ currentConversation: updated });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  // Messaging
  sendMessage: async (content: string, skills: ChatSkill[] = []) => {
    const { currentConversation, messages, openConversation } = get();
    if (!currentConversation) {
      set({ error: 'No conversation selected' });
      return;
    }
    const conversationId = currentConversation.id;

    // Add user message optimistically
    const userMessage: ChatMessageWithContext = {
      id: `temp-user-${Date.now()}`,
      conversation_id: conversationId,
      role: 'user',
      content,
      created_at: new Date().toISOString(),
      message_index: messages.length,
      tool_calls: [],
      citations: [],
    };

    set({
      messages: [...messages, userMessage],
      ...NO_STREAM,
      streamingConversationId: conversationId,
      isStreaming: true,
      error: null,
    });

    try {
      // Auto-inject canvas context when chatting from canvas view
      let canvasContext = undefined;
      if (useUIStore.getState().viewMode === 'canvas') {
        const canvasData = useCanvasStore.getState().canvasData;
        if (canvasData) {
          canvasContext = {
            clusters: canvasData.clusters.map((c) => ({
              label: c.label,
              atom_count: c.atom_count,
            })),
          };
        }
      }

      await getTransport().invoke<ChatMessageWithContext>('send_chat_message', {
        conversationId,
        content,
        skills: skills.map(({ name, instructions }) => ({ name, instructions })),
        canvasContext,
        // Built here rather than earlier so it describes where the user is at
        // the moment they send, not where they were when the turn began.
        pageContext: buildPageContext(),
      });

      // The request returning means the turn is over, whether or not its
      // completion event arrived — release the slot before anything else so a
      // dropped event can't strand the UI.
      set((state) => releaseStream(state, conversationId));

      // Refetch the conversation to get the properly saved messages. This
      // ensures correct IDs and ordering from the database — but only if the
      // user is still looking at it; a refetch would otherwise drag them back
      // from wherever they navigated while the turn ran.
      if (get().currentConversation?.id === conversationId) {
        await openConversation(conversationId);
      }
    } catch (e) {
      set((state) => ({
        ...releaseStream(state, conversationId),
        // Remove the temp user message on error — but only from the
        // conversation it belongs to.
        ...(state.currentConversation?.id === conversationId
          ? { messages: state.messages.filter((m) => !m.id.startsWith('temp-')) }
          : {}),
        error: String(e),
      }));
    }
  },

  cancelResponse: async () => {
    const { streamingConversationId, isStreaming, isCancelling } = get();
    if (!streamingConversationId || !isStreaming || isCancelling) return;

    set({ isCancelling: true });
    try {
      const result = await getTransport().invoke<{ cancelled?: boolean }>('cancel_chat_message', {
        conversationId: streamingConversationId,
      });
      // `cancelled: false` means the server found no turn to signal — the
      // request beat its own turn's registration, or the turn already
      // finished. Either way nothing is coming to clear the latch, so the
      // Stop button must not stay stuck in its pending look.
      if (result?.cancelled === false) {
        set((state) =>
          state.streamingConversationId === streamingConversationId
            ? { isCancelling: false }
            : {},
        );
      }
      // Otherwise the turn keeps streaming until the backend finalizes the
      // partial answer; completeMessage clears the streaming state from the
      // event.
    } catch (e) {
      // The request never landed — nothing is coming back to clear the UI,
      // so drop the streaming state here.
      set((state) => ({ ...releaseStream(state, streamingConversationId), error: String(e) }));
    }
  },

  // Streaming updates - deltas arrive one provider chunk at a time
  appendStreamContent: (conversationId: string, delta: string) => {
    set((state) =>
      state.streamingConversationId === conversationId
        ? { streamingContent: state.streamingContent + delta }
        : {},
    );
  },

  startStreamingToolCall: ({ conversation_id, tool_call_id, tool_name, tool_input }) => {
    const now = new Date().toISOString();
    set((state) =>
      state.streamingConversationId === conversation_id
        ? {
            streamingToolCalls: [
              ...state.streamingToolCalls,
              {
                id: tool_call_id,
                message_id: '',
                tool_name,
                tool_input,
                tool_output: null,
                status: 'running' as const,
                created_at: now,
                completed_at: null,
              },
            ],
          }
        : {},
    );
  },

  completeStreamingToolCall: ({ conversation_id, tool_call_id, results_count, failed }) => {
    const now = new Date().toISOString();
    set((state) =>
      state.streamingConversationId === conversation_id
        ? {
            streamingToolCalls: state.streamingToolCalls.map((call) =>
              call.id === tool_call_id
                ? {
                    ...call,
                    status: failed ? ('failed' as const) : ('complete' as const),
                    tool_output: { results_count },
                    completed_at: now,
                  }
                : call,
            ),
          }
        : {},
    );
  },

  completeMessage: (conversationId: string, message: ChatMessageWithContext) => {
    set((state) => {
      // The turn is over for the conversation it belonged to, wherever the
      // user happens to be now.
      const released = releaseStream(state, conversationId);
      // The message only joins the transcript on screen if that transcript is
      // the one it belongs to. Skip if it's already there (event + refetch).
      const belongsHere =
        state.currentConversation?.id === conversationId &&
        !state.messages.some((m) => m.id === message.id);
      return belongsHere ? { ...released, messages: [...state.messages, message] } : released;
    });
  },

  setStreamingError: (conversationId: string, error: string) => {
    set((state) => ({ ...releaseStream(state, conversationId), error }));
  },

  applyConversationTitle: (id: string, title: string) => {
    set((state) => ({
      conversations: state.conversations.map((c) => (c.id === id ? { ...c, title } : c)),
      currentConversation:
        state.currentConversation?.id === id
          ? { ...state.currentConversation, title }
          : state.currentConversation,
    }));
  },

  // Utilities
  clearError: () => set({ error: null }),

  reset: () =>
    set({
      view: 'list',
      currentConversation: null,
      messages: [],
      conversations: [],
      listFilterTagId: null,
      showArchived: false,
      savedAtomIdByMessageId: {},
      savingAnswerMessageIds: {},
      isLoading: false,
      ...NO_STREAM,
      error: null,
    }),
}));
