import { useEffect, useState } from 'react';
import { LeftPanel } from './LeftPanel';
import { MainView } from './MainView';
import { RightDrawer } from './RightDrawer';
import { LoadingIndicator } from '../ui/LoadingIndicator';
import { ServerConnectionStatus } from '../ui/ServerConnectionStatus';
import { OnboardingWizard } from '../onboarding';
import { CommandPalette } from '../command-palette';
import { EventLogModal } from '../event-log';
import { PipelineStatus } from '../pipeline';
import { useAtomsStore } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { useUIStore } from '../../stores/ui';
import { useTheme, useFont } from '../../hooks';
import { verifyProviderConfigured } from '../../lib/api';
import { isTauri } from '../../lib/platform';


export function Layout() {
  useTheme(); // Initialize theme
  useFont(); // Initialize font
  const fetchAtoms = useAtomsStore(s => s.fetchAtoms);
  const fetchTags = useTagsStore(s => s.fetchTags);
  const [isSetupRequired, setIsSetupRequired] = useState<boolean | null>(null); // null = checking
  const [eventLogOpen, setEventLogOpen] = useState(false);
  const [pipelineStatusOpen, setPipelineStatusOpen] = useState(false);

  // Command palette state
  const commandPaletteOpen = useUIStore((state) => state.commandPaletteOpen);
  const commandPaletteInitialQuery = useUIStore((state) => state.commandPaletteInitialQuery);
  const toggleCommandPalette = useUIStore((state) => state.toggleCommandPalette);
  const closeCommandPalette = useUIStore((state) => state.closeCommandPalette);
  const openCommandPalette = useUIStore((state) => state.openCommandPalette);
  const openDrawer = useUIStore((state) => state.openDrawer);

  // Global keyboard shortcuts
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Don't trigger shortcuts when typing in input fields (except for command palette toggle)
      const isInputActive =
        document.activeElement?.tagName === 'INPUT' ||
        document.activeElement?.tagName === 'TEXTAREA' ||
        (document.activeElement as HTMLElement)?.isContentEditable;

      // Cmd+P or Ctrl+P to toggle command palette (works even in inputs)
      if ((e.metaKey || e.ctrlKey) && e.key === 'p') {
        e.preventDefault();
        toggleCommandPalette();
        return;
      }

      // Skip other shortcuts if input is active
      if (isInputActive) return;

      // "/" to open command palette in search mode
      if (e.key === '/' && !commandPaletteOpen) {
        e.preventDefault();
        openCommandPalette('/');
        return;
      }

      // "#" to open command palette in tag filter mode
      if (e.key === '#' && !commandPaletteOpen) {
        e.preventDefault();
        openCommandPalette('#');
        return;
      }

      // Cmd+N or Ctrl+N to create new atom (only when palette is closed)
      if ((e.metaKey || e.ctrlKey) && e.key === 'n' && !commandPaletteOpen) {
        e.preventDefault();
        openDrawer('editor');
        return;
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [toggleCommandPalette, openCommandPalette, openDrawer, commandPaletteOpen]);

  // Listen for custom settings event from command palette
  useEffect(() => {
    const handleOpenSettings = () => useUIStore.getState().setViewMode('settings');
    window.addEventListener('open-settings', handleOpenSettings);
    return () => window.removeEventListener('open-settings', handleOpenSettings);
  }, []);

  // Listen for custom event log event from command palette
  useEffect(() => {
    const handleOpenEventLog = () => setEventLogOpen(true);
    window.addEventListener('open-event-log', handleOpenEventLog);
    return () => window.removeEventListener('open-event-log', handleOpenEventLog);
  }, []);

  // Listen for pipeline status event from command palette
  useEffect(() => {
    const handleOpenPipelineStatus = () => setPipelineStatusOpen(true);
    window.addEventListener('open-pipeline-status', handleOpenPipelineStatus);
    return () => window.removeEventListener('open-pipeline-status', handleOpenPipelineStatus);
  }, []);

  // Listen for auth expiry (stale/revoked token) and transition to setup mode
  useEffect(() => {
    const handler = () => setIsSetupRequired(true);
    window.addEventListener('atomic:auth-expired', handler);
    return () => window.removeEventListener('atomic:auth-expired', handler);
  }, []);

  // Check if setup is needed on mount
  useEffect(() => {
    const checkSetup = async () => {
      try {
        // Skip onboarding if env vars provided a connection
        const envConfigured = !!(import.meta.env.VITE_ATOMIC_URL && import.meta.env.VITE_ATOMIC_TOKEN);
        const configured = envConfigured || await verifyProviderConfigured();
        setIsSetupRequired(!configured);

        if (configured) {
          await initializeApp();
        }
      } catch (error) {
        console.error('Failed to check provider configuration:', error);
        setIsSetupRequired(true);
      }
    };

    checkSetup();
  }, []);

  const initializeApp = async () => {
    await Promise.all([fetchAtoms(), fetchTags()]);
  };

  const handleSetupComplete = async () => {
    setIsSetupRequired(false);
    // Now initialize the app
    await initializeApp();
  };

  // Show loading while checking
  if (isSetupRequired === null) {
    return (
      <div className={`flex h-screen items-center justify-center bg-[var(--color-bg-main)] ${isTauri() ? 'pt-[28px]' : ''}`}>
        <span className="text-[var(--color-text-secondary)]">Loading...</span>
      </div>
    );
  }

  // Show onboarding wizard if setup is required
  if (isSetupRequired) {
    return (
      <div className={`flex h-screen overflow-hidden bg-[var(--color-bg-main)] ${isTauri() ? 'pt-[28px]' : ''}`}>
        <OnboardingWizard onComplete={handleSetupComplete} />
      </div>
    );
  }

  return (
    <div className="flex h-screen overflow-hidden bg-[var(--color-bg-main)]">
      <LeftPanel />
      <MainView />
      <RightDrawer />
      <LoadingIndicator />
      <ServerConnectionStatus />
      <CommandPalette
        isOpen={commandPaletteOpen}
        onClose={closeCommandPalette}
        initialQuery={commandPaletteInitialQuery}
      />
      <EventLogModal
        isOpen={eventLogOpen}
        onClose={() => setEventLogOpen(false)}
      />
      <PipelineStatus
        isOpen={pipelineStatusOpen}
        onClose={() => setPipelineStatusOpen(false)}
      />
    </div>
  );
}

