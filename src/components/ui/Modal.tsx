import { ReactNode, useEffect, useRef } from 'react';
import { createPortal } from 'react-dom';
import { X } from 'lucide-react';
import { Button } from './Button';

interface ModalProps {
  isOpen: boolean;
  onClose: () => void;
  title: string;
  children: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  onConfirm?: () => void;
  confirmVariant?: 'primary' | 'danger';
  confirmDisabled?: boolean;
  showFooter?: boolean;
  /// Width tier. `md` (the default) keeps existing call sites unchanged.
  /// `lg` is for forms with multi-field layouts; `xl` for editors with
  /// progressive-disclosure sections.
  width?: 'md' | 'lg' | 'xl';
}

const WIDTH_CLASSES: Record<NonNullable<ModalProps['width']>, string> = {
  md: 'max-w-md',
  lg: 'max-w-xl',
  xl: 'max-w-2xl',
};

export function Modal({
  isOpen,
  onClose,
  title,
  children,
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  onConfirm,
  confirmVariant = 'primary',
  confirmDisabled = false,
  showFooter = true,
  width = 'md',
}: ModalProps) {
  const overlayRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleEscape = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        onClose();
      }
    };

    if (isOpen) {
      document.addEventListener('keydown', handleEscape);
      document.body.style.overflow = 'hidden';
    }

    return () => {
      document.removeEventListener('keydown', handleEscape);
      document.body.style.overflow = '';
    };
  }, [isOpen, onClose]);

  const handleOverlayClick = (e: React.MouseEvent) => {
    if (e.target === overlayRef.current) {
      onClose();
    }
  };

  if (!isOpen) return null;

  return createPortal(
    <div
      ref={overlayRef}
      onClick={handleOverlayClick}
      data-modal="true"
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 backdrop-blur-sm safe-area-padding"
    >
      <div className={`bg-[var(--color-bg-panel)] rounded-lg shadow-xl border border-[var(--color-border)] w-full ${WIDTH_CLASSES[width]} mx-4 max-h-[90vh] flex flex-col animate-in fade-in zoom-in-95 duration-200`}>
        <div className="flex items-center justify-between px-6 py-4 border-b border-[var(--color-border)]">
          <h2 className="text-lg font-semibold text-[var(--color-text-primary)]">{title}</h2>
          <button
            onClick={onClose}
            className="text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] transition-colors"
          >
            <X className="w-5 h-5" strokeWidth={2} />
          </button>
        </div>
        <div className="px-6 py-4 text-[var(--color-text-primary)] overflow-y-auto flex-1 min-h-0">
          {children}
        </div>
        {showFooter && (
          <div className="flex justify-end gap-3 px-6 py-4 border-t border-[var(--color-border)] flex-shrink-0">
            <Button variant="secondary" onClick={onClose}>
              {cancelLabel}
            </Button>
            {onConfirm && (
              <Button variant={confirmVariant} onClick={onConfirm} disabled={confirmDisabled}>
                {confirmLabel}
              </Button>
            )}
          </div>
        )}
      </div>
    </div>,
    document.body
  );
}

