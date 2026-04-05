import { useRef, useEffect } from 'react';
import { TagTree } from '../tags/TagTree';
import { SettingsButton } from '../settings';
import { DatabaseSwitcher } from '../DatabaseSwitcher';
import { useUIStore } from '../../stores/ui';
import { isTauri } from '../../lib/platform';

const COLLAPSE_BREAKPOINT = 768;

export function LeftPanel() {
  const setViewMode = useUIStore(s => s.setViewMode);
  const leftPanelOpen = useUIStore(s => s.leftPanelOpen);
  const setLeftPanelOpen = useUIStore(s => s.setLeftPanelOpen);
  const panelRef = useRef<HTMLDivElement>(null);

  // Auto-collapse on small screens, auto-expand on large screens
  useEffect(() => {
    const mq = window.matchMedia(`(max-width: ${COLLAPSE_BREAKPOINT}px)`);
    const handleChange = (e: MediaQueryListEvent | MediaQueryList) => {
      setLeftPanelOpen(!e.matches);
    };
    handleChange(mq);
    mq.addEventListener('change', handleChange);
    return () => mq.removeEventListener('change', handleChange);
  }, [setLeftPanelOpen]);

  // Close panel on outside click when in overlay mode (small screens)
  useEffect(() => {
    if (!leftPanelOpen) return;
    const mq = window.matchMedia(`(max-width: ${COLLAPSE_BREAKPOINT}px)`);
    if (!mq.matches) return;

    const handleClick = (e: MouseEvent) => {
      if (panelRef.current && !panelRef.current.contains(e.target as Node)) {
        setLeftPanelOpen(false);
      }
    };
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [leftPanelOpen, setLeftPanelOpen]);

  return (
    <>
      {/* Backdrop for overlay mode on small screens */}
      <div
        className={`fixed inset-0 bg-black/40 z-30 md:hidden transition-opacity duration-200 ${
          leftPanelOpen ? 'opacity-100' : 'opacity-0 pointer-events-none'
        }`}
      />

      {/* Panel */}
      <aside
        ref={panelRef}
        className={`
          w-[250px] h-full bg-[var(--color-bg-panel)]/80 border-r border-[var(--color-border)] flex flex-col transition-all duration-200 backdrop-blur-xl z-10
          max-md:fixed max-md:top-0 max-md:left-0 max-md:z-40 max-md:shadow-2xl
          ${leftPanelOpen ? 'max-md:translate-x-0' : 'max-md:-translate-x-full'}
          ${leftPanelOpen ? '' : 'hidden md:flex'}
        `}
      >
        {/* Titlebar row with settings button */}
        <div className={`h-[52px] flex items-center px-3 flex-shrink-0 gap-1 ${isTauri() ? 'pl-[78px]' : ''}`} data-tauri-drag-region>
          <DatabaseSwitcher />
          <SettingsButton onClick={() => setViewMode('settings')} />
        </div>

        {/* Tag Tree with integrated search */}
        <div className="flex-1 overflow-hidden">
          <TagTree />
        </div>

      </aside>
    </>
  );
}
