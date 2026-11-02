import { useRef, useState } from 'react';
import { ArrowUp, BookOpen, Plus, Square, X } from 'lucide-react';
import { isDemoInstance, demoSignupUrl } from '../../lib/transport';
import { referencedChatSkills, type ChatSkill } from '../../lib/chatSkills';

interface ChatInputProps {
  value: string;
  onChange: (value: string) => void;
  onSend: () => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
  disabled?: boolean;
  placeholder?: string;
  /** A response is streaming: the send button becomes a stop button. */
  isStreaming?: boolean;
  /** A stop has been requested and we're waiting for the turn to wind down. */
  isCancelling?: boolean;
  onStop?: () => void;
  skills?: ChatSkill[];
  skillError?: string | null;
  onImportSkill?: (file: File) => Promise<void>;
  onRemoveSkill?: (name: string) => void;
}

export function ChatInput({
  value,
  onChange,
  onSend,
  onKeyDown,
  disabled = false,
  placeholder = 'Type a message...',
  isStreaming = false,
  isCancelling = false,
  onStop,
  skills = [],
  skillError,
  onImportSkill,
  onRemoveSkill,
}: ChatInputProps) {
  const [skillsOpen, setSkillsOpen] = useState(false);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // On the public demo, chat is the signup carrot: conversations are
  // account-scoped (anonymous chat would be a shared wall), so the input
  // becomes the CTA. Single chokepoint — every chat surface renders
  // through this component.
  if (isDemoInstance()) {
    return (
      <div className="flex-shrink-0 p-3 border-t border-[var(--color-border)]">
        <a
          href={demoSignupUrl()}
          className="block w-full text-center px-4 py-3 rounded-lg border border-dashed border-[var(--color-border)] text-sm text-[var(--color-text-secondary)] hover:border-[var(--color-accent)] hover:text-[var(--color-text-primary)] transition-colors"
        >
          Chat with your knowledge base — free on your own Atomic →
        </a>
      </div>
    );
  }
  const canSend = value.trim().length > 0 && !disabled;
  const activeSkills = referencedChatSkills(value, skills);

  const insertSkill = (name: string) => {
    const token = `$${name}`;
    if (!activeSkills.some((skill) => skill.name === name)) {
      onChange(`${value}${value && !/\s$/.test(value) ? ' ' : ''}${token} `);
    }
    setSkillsOpen(false);
    requestAnimationFrame(() => textareaRef.current?.focus());
  };

  return (
    <div className="flex-shrink-0 p-3 border-t border-[var(--color-border)]">
      {onImportSkill && (
        <div className="relative mb-2 flex items-center gap-2">
          <button
            type="button"
            onClick={() => setSkillsOpen(!skillsOpen)}
            aria-expanded={skillsOpen}
            aria-controls="chat-skills-menu"
            className="inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-xs text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-card)] hover:text-[var(--color-text-primary)]"
          >
            <BookOpen className="h-3.5 w-3.5" />
            Skills{skills.length > 0 ? ` (${skills.length})` : ''}
          </button>
          {activeSkills.length > 0 && (
            <span className="truncate text-xs text-[var(--color-text-tertiary)]">
              Using {activeSkills.map((skill) => `$${skill.name}`).join(', ')}
            </span>
          )}
          <input
            ref={fileInputRef}
            type="file"
            accept=".md,text/markdown"
            aria-label="Import a SKILL.md file"
            className="hidden"
            onChange={async (event) => {
              const file = event.target.files?.[0];
              if (file) await onImportSkill(file);
              event.target.value = '';
            }}
          />
          {skillsOpen && (
            <div
              id="chat-skills-menu"
              className="absolute bottom-full left-0 z-30 mb-2 w-80 max-w-[calc(100vw-2rem)] rounded-lg border border-[var(--color-border)] bg-[var(--color-bg-card)] p-2 shadow-xl"
            >
              <div className="max-h-64 overflow-y-auto">
                {skills.length === 0 ? (
                  <p className="px-2 py-2 text-xs text-[var(--color-text-secondary)]">No skills loaded yet.</p>
                ) : skills.map((skill) => (
                  <div key={skill.name} className="flex items-start gap-1 rounded-md hover:bg-[var(--color-bg-main)]">
                    <button
                      type="button"
                      onClick={() => insertSkill(skill.name)}
                      className="min-w-0 flex-1 px-2 py-2 text-left"
                    >
                      <span className="block text-sm text-[var(--color-text-primary)]">${skill.name}</span>
                      <span className="block truncate text-xs text-[var(--color-text-tertiary)]" title={skill.description}>{skill.description}</span>
                    </button>
                    <button
                      type="button"
                      onClick={() => onRemoveSkill?.(skill.name)}
                      aria-label={`Remove skill ${skill.name}`}
                      title={`Remove ${skill.name}`}
                      className="mt-1 rounded-md p-1.5 text-[var(--color-text-tertiary)] hover:text-[var(--color-text-primary)]"
                    >
                      <X className="h-3.5 w-3.5" />
                    </button>
                  </div>
                ))}
              </div>
              <button
                type="button"
                onClick={() => fileInputRef.current?.click()}
                className="mt-1 flex w-full items-center gap-1.5 rounded-md border-t border-[var(--color-border)] px-2 py-2 text-left text-xs text-[var(--color-text-primary)] hover:bg-[var(--color-bg-main)]"
              >
                <Plus className="h-3.5 w-3.5" /> Import SKILL.md
              </button>
              <p className="px-2 pt-1 text-xs text-[var(--color-text-tertiary)]">
                Type $name to use a skill in one question. Linked files and scripts are not imported. Saved in this browser for this database.
              </p>
            </div>
          )}
        </div>
      )}
      {skillError && <p role="alert" className="mb-2 text-xs text-red-400">{skillError}</p>}
      <div className="relative">
        {/* `block` on the textarea: inline-level form controls sit on a text
            baseline and leave ~6px of line box below them, which the wrapper
            would inherit — and the bottom-anchored button measures against
            the wrapper, not the control. */}
        <textarea
          ref={textareaRef}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={onKeyDown}
          placeholder={placeholder}
          disabled={disabled}
          rows={1}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          className="block w-full resize-none overflow-hidden bg-[var(--color-bg-main)] border border-[var(--color-border)] rounded-lg pl-4 pr-14 py-3 text-[var(--color-text-primary)] placeholder-[var(--color-text-tertiary)] focus:outline-none focus:border-[var(--color-accent)] disabled:opacity-50 disabled:cursor-not-allowed"
          style={{
            minHeight: '44px',
            maxHeight: '200px',
          }}
          onInput={(e) => {
            // Auto-resize textarea. scrollHeight stops at the padding edge but
            // box-sizing is border-box, so the borders have to be added back —
            // otherwise every grow lands 2px short and clips the last line.
            const target = e.target as HTMLTextAreaElement;
            target.style.height = 'auto';
            const borders = target.offsetHeight - target.clientHeight;
            target.style.height = `${Math.min(target.scrollHeight + borders, 200)}px`;
          }}
        />
        {/* Anchored to the bottom so it travels with the last line as the
            textarea grows. At the resting single-line height — 24px line +
            24px padding + 2px border = 50px — a 9px offset centers the 32px
            button exactly. */}
        {isStreaming ? (
          <button
            type="button"
            onClick={onStop}
            disabled={isCancelling || !onStop}
            title={isCancelling ? 'Stopping…' : 'Stop response'}
            aria-label={isCancelling ? 'Stopping response' : 'Stop response'}
            className="absolute right-2 bottom-[9px] w-8 h-8 flex items-center justify-center rounded-md bg-[var(--color-bg-card)] border border-[var(--color-border)] text-[var(--color-text-primary)] hover:border-[var(--color-accent)] disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            <Square className="w-3.5 h-3.5 fill-current" />
          </button>
        ) : (
          <button
            type="button"
            onClick={onSend}
            disabled={!canSend}
            title="Send message"
            aria-label="Send message"
            className="absolute right-2 bottom-[9px] w-8 h-8 flex items-center justify-center rounded-md bg-[var(--color-accent)] text-white hover:bg-[var(--color-accent-hover)] disabled:opacity-40 disabled:cursor-not-allowed transition-colors"
          >
            <ArrowUp className="w-4 h-4" strokeWidth={2.5} />
          </button>
        )}
      </div>
    </div>
  );
}
