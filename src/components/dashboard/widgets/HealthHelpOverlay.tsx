import { createPortal } from 'react-dom';
import { X } from 'lucide-react';
import { useEffect } from 'react';

const SHORTCUTS = [
  { key: 'r', desc: 'Refresh all checks' },
  { key: 'f', desc: 'Open fix confirmation' },
  { key: 'e', desc: 'Export to markdown' },
  { key: '1 – 9', desc: 'Expand / collapse Nth check in list' },
  { key: '?', desc: 'Toggle this help overlay' },
  { key: 'Esc', desc: 'Close modal / overlay' },
];

interface Props {
  onClose: () => void;
}

export function HealthHelpOverlay({ onClose }: Props) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === 'Escape' || e.key === '?') onClose();
    };
    document.addEventListener('keydown', handler);
    return () => document.removeEventListener('keydown', handler);
  }, [onClose]);

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={e => { if (e.target === e.currentTarget) onClose(); }}
    >
      <div className="bg-[#1e1e1e] border border-white/10 rounded-lg shadow-2xl w-full max-w-sm mx-4">
        <div className="flex items-center justify-between px-5 py-4 border-b border-white/5">
          <h2 className="text-sm font-semibold text-white">Keyboard shortcuts</h2>
          <button onClick={onClose} className="text-gray-500 hover:text-gray-300 transition-colors p-1">
            <X className="w-4 h-4" />
          </button>
        </div>
        <div className="px-5 py-4 space-y-2">
          {SHORTCUTS.map(({ key, desc }) => (
            <div key={key} className="flex items-center justify-between text-xs">
              <kbd className="px-2 py-0.5 bg-[#2a2a2a] border border-white/10 rounded font-mono text-gray-300">{key}</kbd>
              <span className="text-gray-500 ml-3">{desc}</span>
            </div>
          ))}
        </div>
      </div>
    </div>,
    document.body,
  );
}
