import { isDemoInstance, demoSignupUrl } from '../../lib/transport';

interface ChatInputProps {
  value: string;
  onChange: (value: string) => void;
  onSend: () => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
  disabled?: boolean;
  placeholder?: string;
}

export function ChatInput({
  value,
  onChange,
  onSend: _onSend,
  onKeyDown,
  disabled = false,
  placeholder = 'Type a message...',
}: ChatInputProps) {
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
  return (
    <div className="flex-shrink-0 p-3 border-t border-[var(--color-border)]">
      <textarea
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
        className="w-full resize-none overflow-hidden bg-[var(--color-bg-main)] border border-[var(--color-border)] rounded-lg px-4 py-3 text-[var(--color-text-primary)] placeholder-[var(--color-text-tertiary)] focus:outline-none focus:border-[var(--color-accent)] disabled:opacity-50 disabled:cursor-not-allowed"
        style={{
          minHeight: '44px',
          maxHeight: '200px',
        }}
        onInput={(e) => {
          // Auto-resize textarea
          const target = e.target as HTMLTextAreaElement;
          target.style.height = 'auto';
          target.style.height = `${Math.min(target.scrollHeight, 200)}px`;
        }}
      />
    </div>
  );
}
