import { useState, useRef, useEffect } from 'react';
import { isDemoInstance } from '../../lib/transport';
import { TagTree } from '../tags/TagTree';
import { SettingsButton, SettingsModal, type SettingsTab } from '../settings';
import { DatabaseSwitcher } from '../DatabaseSwitcher';
import { useUIStore } from '../../stores/ui';
import { isTauri } from '../../lib/platform';

const COLLAPSE_BREAKPOINT = 768;
export function LeftPanel() {
  const [isSettingsOpen, setIsSettingsOpen] = useState(false);
  const [settingsInitialTab, setSettingsInitialTab] = useState<SettingsTab | undefined>(undefined);
  const leftPanelOpen = useUIStore(s => s.leftPanelOpen);
  const setLeftPanelOpen = useUIStore(s => s.setLeftPanelOpen);
  const panelRef = useRef<HTMLDivElement>(null);

  // On mount, force-collapse on small screens (the sidebar covers content
  // there). On large screens we respect the persisted preference. Resize
  // events still auto-toggle in both directions.
  useEffect(() => {
    const mq = window.matchMedia(`(max-width: ${COLLAPSE_BREAKPOINT}px)`);
    if (mq.matches) {
      setLeftPanelOpen(false);
    }
    const handleChange = (e: MediaQueryListEvent) => {
      setLeftPanelOpen(!e.matches);
    };
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
          relative h-full z-10 flex-shrink-0 overflow-hidden
          md:transition-[width,border-color] md:duration-300 md:ease-in-out
          max-md:transition-all max-md:duration-300 max-md:ease-in-out
          max-md:fixed max-md:top-0 max-md:left-0 max-md:z-40 max-md:shadow-2xl max-md:w-[250px]
          max-md:pt-[env(safe-area-inset-top)] max-md:pb-[env(safe-area-inset-bottom)] max-md:pl-[env(safe-area-inset-left)]
          ${leftPanelOpen ? 'max-md:translate-x-0' : 'max-md:-translate-x-full'}
          ${leftPanelOpen ? 'md:w-[250px]' : 'md:w-0'}
        `}
      >
        <div
          className="h-full w-[250px] bg-[var(--color-bg-panel)]/80 border-r border-[var(--color-border)] backdrop-blur-xl flex flex-col overflow-hidden md:absolute md:inset-y-0 md:left-0 md:translate-x-0 md:pointer-events-auto"
        >
          {/* Titlebar row with settings button */}
          <div className={`h-[52px] flex items-center px-3 flex-shrink-0 gap-1 ${isTauri() ? 'pl-[78px]' : ''}`} data-tauri-drag-region>
            <DatabaseSwitcher />
            {!isDemoInstance() && (
              <SettingsButton onClick={() => { setSettingsInitialTab(undefined); setIsSettingsOpen(true); }} />
            )}
          </div>

          {/* Tag Tree with integrated search */}
          <div className="flex-1 overflow-hidden">
            <TagTree
              onOpenTagSettings={() => {
                setSettingsInitialTab('tag-categories');
                setIsSettingsOpen(true);
              }}
            />
          </div>
        </div>

        <SettingsModal
          isOpen={isSettingsOpen}
          onClose={() => setIsSettingsOpen(false)}
          initialTab={settingsInitialTab}
        />
      </aside>
    </>
  );
}
