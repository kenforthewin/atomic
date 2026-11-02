import { useState, useRef, useEffect, useCallback, useMemo } from 'react';
import { MessageSquare } from 'lucide-react';
import { useChatStore, scopeModeOf } from '../../stores/chat';
import { useDatabasesStore } from '../../stores/databases';
import { useUIStore } from '../../stores/ui';
import { loadChatSkills, parseChatSkill, saveChatSkills, skillsForTurn, type ChatSkill } from '../../lib/chatSkills';
import { useContentSearch } from '../../hooks';
import { ChatHeader } from './ChatHeader';
import { ChatMessage } from './ChatMessage';
import { ChatInput } from './ChatInput';
import { SearchBar } from '../ui/SearchBar';

export function ChatView() {
  const currentConversation = useChatStore(s => s.currentConversation);
  const messages = useChatStore(s => s.messages);
  const isLoading = useChatStore(s => s.isLoading);
  const streamingConversationId = useChatStore(s => s.streamingConversationId);
  const isStreaming = useChatStore(s => s.isStreaming);
  const isCancelling = useChatStore(s => s.isCancelling);
  const streamingContent = useChatStore(s => s.streamingContent);
  const streamingToolCalls = useChatStore(s => s.streamingToolCalls);
  const error = useChatStore(s => s.error);
  const sendMessage = useChatStore(s => s.sendMessage);
  const cancelResponse = useChatStore(s => s.cancelResponse);
  const goBack = useChatStore(s => s.goBack);

  // The streaming fields describe one conversation's turn. Rendering them here
  // is only correct when that conversation is the one on screen — otherwise a
  // turn left running elsewhere would show up as this conversation thinking.
  const isStreamingHere =
    isStreaming && streamingConversationId !== null &&
    streamingConversationId === currentConversation?.id;

  const openReader = useUIStore(s => s.openReader);
  const databaseId = useDatabasesStore(s => s.activeId);

  const [inputValue, setInputValue] = useState('');
  const [skills, setSkills] = useState<ChatSkill[]>(() => loadChatSkills(databaseId));
  const [skillError, setSkillError] = useState<string | null>(null);
  useEffect(() => {
    setSkills(loadChatSkills(databaseId));
    setSkillError(null);
  }, [databaseId]);
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const messagesContainerRef = useRef<HTMLDivElement>(null);
  const isNearBottomRef = useRef(true);

  // Combine all message content for search
  const allContent = useMemo(() => {
    return messages.map(m => m.content).join('\n\n');
  }, [messages]);

  // Content search across all messages
  const {
    isOpen: isSearchOpen,
    query: searchQuery,
    searchedQuery,
    currentIndex,
    totalMatches,
    setQuery: setSearchQuery,
    openSearch,
    closeSearch,
    goToNext,
    goToPrevious,
    highlightText,
  } = useContentSearch(allContent);

  // Keyboard handler for Ctrl+F / Cmd+F
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key === 'f') {
        e.preventDefault();
        openSearch();
      }
    };
    document.addEventListener('keydown', handleKeyDown);
    return () => document.removeEventListener('keydown', handleKeyDown);
  }, [openSearch]);

  // Check if user is near the bottom of the scroll container
  const checkIfNearBottom = useCallback(() => {
    const container = messagesContainerRef.current;
    if (!container) return true;

    const threshold = 100; // pixels from bottom to consider "at bottom"
    const { scrollTop, scrollHeight, clientHeight } = container;
    return scrollHeight - scrollTop - clientHeight < threshold;
  }, []);

  // Update near-bottom state on scroll
  const handleScroll = useCallback(() => {
    isNearBottomRef.current = checkIfNearBottom();
  }, [checkIfNearBottom]);

  // Auto-scroll to bottom only if user is already near bottom
  useEffect(() => {
    if (isNearBottomRef.current) {
      messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' });
    }
  }, [messages, streamingContent]);

  // Always scroll to bottom when a new message is sent (user action)
  const scrollToBottom = useCallback(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' });
    isNearBottomRef.current = true;
  }, []);

  const handleSend = async () => {
    if (!inputValue.trim() || isStreamingHere) return;

    const content = inputValue.trim();
    let selectedSkills: ChatSkill[];
    try {
      selectedSkills = skillsForTurn(content, skills);
    } catch (error) {
      setSkillError(String(error instanceof Error ? error.message : error));
      return;
    }
    setSkillError(null);
    setInputValue('');
    scrollToBottom(); // Scroll to bottom when user sends a message
    await sendMessage(content, selectedSkills);
  };

  const handleImportSkill = async (file: File) => {
    try {
      if (file.name.toLowerCase() !== 'skill.md') {
        throw new Error('Choose a SKILL.md file.');
      }
      if (file.size > 100_000) {
        throw new Error('This skill file is too large to import.');
      }
      const skill = parseChatSkill(await file.text());
      const next = [...skills.filter((item) => item.name !== skill.name), skill]
        .sort((a, b) => a.name.localeCompare(b.name));
      saveChatSkills(databaseId, next);
      setSkills(next);
      setSkillError(null);
    } catch (error) {
      setSkillError(String(error instanceof Error ? error.message : error));
    }
  };

  const handleRemoveSkill = (name: string) => {
    try {
      const next = skills.filter((skill) => skill.name !== name);
      saveChatSkills(databaseId, next);
      setSkills(next);
      setSkillError(null);
    } catch (error) {
      setSkillError(String(error instanceof Error ? error.message : error));
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
  };

  // Handle viewing an atom from citation - switch drawer to viewer mode
  const handleViewAtom = useCallback((atomId: string, highlightText?: string) => {
    openReader(atomId, highlightText);
  }, [openReader]);

  if (!currentConversation) {
    return (
      <div className="flex items-center justify-center h-full text-[var(--color-text-secondary)]">
        {isLoading ? 'Loading conversation...' : 'No conversation selected'}
      </div>
    );
  }

  const scopeSubjects = currentConversation.tags
    .filter((t) => scopeModeOf(t) !== 'exclude')
    .map((t) => t.name);

  return (
    <div className="h-full flex flex-col">
      {/* Header with back button and scope */}
      <ChatHeader conversation={currentConversation} onBack={goBack} />

      {/* Messages area */}
      <div
        ref={messagesContainerRef}
        onScroll={handleScroll}
        className="flex-1 overflow-y-auto p-4 space-y-4 relative"
        style={{ overflowAnchor: 'none' }}
      >
        {/* Search bar */}
        {isSearchOpen && (
          <SearchBar
            query={searchQuery}
            searchedQuery={searchedQuery}
            onQueryChange={setSearchQuery}
            currentIndex={currentIndex}
            totalMatches={totalMatches}
            onNext={goToNext}
            onPrevious={goToPrevious}
            onClose={closeSearch}
          />
        )}
        {messages.length === 0 && !isStreamingHere && (
          <div className="flex flex-col items-center justify-center h-full gap-4 text-center">
            <div className="w-16 h-16 rounded-full bg-[var(--color-bg-card)] flex items-center justify-center">
              <MessageSquare className="w-8 h-8 text-[var(--color-accent)]" strokeWidth={2} />
            </div>
            <div>
              <p className="text-[var(--color-text-primary)] font-medium mb-1">Start the conversation</p>
              <p className="text-[var(--color-text-secondary)] text-sm max-w-sm">
                Ask questions about your knowledge base. The AI will search through your atoms to find relevant information.
              </p>
            </div>
          </div>
        )}

        {messages.map((message) => (
          <ChatMessage
            key={message.id}
            message={message}
            onViewAtom={handleViewAtom}
            searchQuery={isSearchOpen ? searchQuery : ''}
            highlightText={highlightText}
          />
        ))}

        {/* Streaming bubble — rendered for the whole streaming turn so tool
            calls show up while the model is still thinking and persist as
            streaming content flows in. ChatMessage handles the empty-content
            "Thinking…" fallback. */}
        {isStreamingHere && (
          <ChatMessage
            message={{
              id: 'streaming',
              conversation_id: currentConversation.id,
              role: 'assistant',
              content: streamingContent,
              created_at: new Date().toISOString(),
              message_index: messages.length,
              tool_calls: streamingToolCalls,
              citations: [],
            }}
            isStreaming
            onViewAtom={handleViewAtom}
            searchQuery={isSearchOpen ? searchQuery : ''}
            highlightText={highlightText}
          />
        )}

        {/* Error message */}
        {error && (
          <div className="p-3 bg-red-500/10 border border-red-500/30 rounded-lg text-red-400 text-sm">
            {error}
          </div>
        )}

        <div ref={messagesEndRef} />
      </div>

      {/* Input area */}
      <ChatInput
        value={inputValue}
        onChange={setInputValue}
        onSend={handleSend}
        onKeyDown={handleKeyDown}
        disabled={isStreamingHere}
        isStreaming={isStreamingHere}
        isCancelling={isStreamingHere && isCancelling}
        onStop={cancelResponse}
        skills={skills}
        skillError={skillError}
        onImportSkill={handleImportSkill}
        onRemoveSkill={handleRemoveSkill}
        placeholder={
          // Excluded tags say what the conversation isn't about — naming them
          // here would read as the opposite of the scope.
          scopeSubjects.length > 0
            ? `Ask about ${scopeSubjects.join(', ')}...`
            : 'Ask anything about your knowledge base...'
        }
      />
    </div>
  );
}
