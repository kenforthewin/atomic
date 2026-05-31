import { useEffect, useRef, useState } from 'react';
import { HelpCircle } from 'lucide-react';

interface SignalHelpProps {
  title: string;
  children: React.ReactNode;
}

export function SignalHelp({ title, children }: SignalHelpProps) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!open) return;
    const handlePointerDown = (event: PointerEvent) => {
      if (!ref.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setOpen(false);
      }
    };
    document.addEventListener('pointerdown', handlePointerDown);
    document.addEventListener('keydown', handleKeyDown);
    return () => {
      document.removeEventListener('pointerdown', handlePointerDown);
      document.removeEventListener('keydown', handleKeyDown);
    };
  }, [open]);

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={() => setOpen(value => !value)}
        aria-expanded={open}
        title={`About ${title}`}
        className="rounded p-1 text-[var(--color-text-tertiary)] transition-colors hover:bg-[var(--color-bg-hover)] hover:text-[var(--color-text-primary)]"
      >
        <HelpCircle className="h-3.5 w-3.5" strokeWidth={2} />
      </button>
      {open && (
        <div className="absolute right-0 top-6 z-30 w-72 rounded-md border border-[var(--color-border)] bg-[var(--color-bg-panel)] p-3 text-left shadow-xl">
          <div className="text-xs font-medium uppercase tracking-[0.12em] text-[var(--color-text-tertiary)]">
            {title}
          </div>
          <div className="mt-2 space-y-2 text-xs leading-relaxed text-[var(--color-text-secondary)]">
            {children}
          </div>
        </div>
      )}
    </div>
  );
}
