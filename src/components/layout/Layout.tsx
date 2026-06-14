import { useEffect, useState } from 'react';
import { LeftPanel } from './LeftPanel';
import { MainView } from './MainView';
import { LoadingIndicator } from '../ui/LoadingIndicator';
import { ServerConnectionStatus } from '../ui/ServerConnectionStatus';
import { RouterBridge } from '../../router/RouterBridge';
import { SettingsModal } from '../settings/SettingsModal';
import { OnboardingWizard } from '../onboarding';
import { CommandPalette } from '../command-palette';
import { SearchPalette } from '../search-palette/SearchPalette';
import { useAtomsStore } from '../../stores/atoms';
import { useTagsStore } from '../../stores/tags';
import { useDatabasesStore } from '../../stores/databases';
import { useUIStore } from '../../stores/ui';
import { useTheme, useFont } from '../../hooks';
import { verifyProviderConfigured } from '../../lib/api';
import { isCloudTenant } from '../../lib/transport';
import { useSettingsStore } from '../../stores/settings';
import { isTauri } from '../../lib/platform';

// Per-tenant setting marking that a cloud account finished onboarding. On cloud
// the AI provider is always configured (managed), so provider-configured can't
// gate first-run setup the way it does for self-hosted installs — this flag does.
const CLOUD_ONBOARDING_SETTING = 'onboarding_completed';


export function Layout() {
  useTheme(); // Initialize theme
  useFont(); // Initialize font
  const fetchAtoms = useAtomsStore(s => s.fetchAtoms);
  const fetchTags = useTagsStore(s => s.fetchTags);
  const [isSetupRequired, setIsSetupRequired] = useState<boolean | null>(null); // null = checking
  const [settingsOpen, setSettingsOpen] = useState(false);

  // Command palette state
  const commandPaletteOpen = useUIStore((state) => state.commandPaletteOpen);
  const toggleCommandPalette = useUIStore((state) => state.toggleCommandPalette);
  const closeCommandPalette = useUIStore((state) => state.closeCommandPalette);
  const searchPaletteOpen = useUIStore((state) => state.searchPaletteOpen);
  const searchPaletteInitialQuery = useUIStore((state) => state.searchPaletteInitialQuery);
  const toggleSearchPalette = useUIStore((state) => state.toggleSearchPalette);
  const closeSearchPalette = useUIStore((state) => state.closeSearchPalette);
  const openSearchPalette = useUIStore((state) => state.openSearchPalette);

  // Global keyboard shortcuts
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Don't trigger shortcuts when typing in input fields (except for command palette toggle)
      const isInputActive =
        document.activeElement?.tagName === 'INPUT' ||
        document.activeElement?.tagName === 'TEXTAREA' ||
        (document.activeElement as HTMLElement)?.isContentEditable;

      // Cmd+Shift+P or Ctrl+Shift+P to toggle command palette (works even in inputs)
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key.toLowerCase() === 'p') {
        e.preventDefault();
        toggleCommandPalette();
        return;
      }

      // Cmd+P or Ctrl+P to toggle global search (works even in inputs)
      if ((e.metaKey || e.ctrlKey) && e.key === 'p') {
        e.preventDefault();
        toggleSearchPalette();
        return;
      }

      // Skip other shortcuts if input is active
      if (isInputActive) return;

      // "/" to open the global search palette
      if (e.key === '/' && !commandPaletteOpen && !searchPaletteOpen) {
        e.preventDefault();
        openSearchPalette();
        return;
      }

      // "#" to open the search palette in tag-only mode
      if (e.key === '#' && !commandPaletteOpen && !searchPaletteOpen) {
        e.preventDefault();
        openSearchPalette('#');
        return;
      }

      // Cmd+N or Ctrl+N to create new atom (only when palettes are closed)
      if ((e.metaKey || e.ctrlKey) && e.key === 'n' && !commandPaletteOpen && !searchPaletteOpen) {
        e.preventDefault();
        const { createAtom } = useAtomsStore.getState();
        createAtom('').then((newAtom) => {
          useUIStore.getState().openReaderEditing(newAtom.id);
        }).catch(console.error);
        return;
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [toggleCommandPalette, toggleSearchPalette, openSearchPalette, commandPaletteOpen, searchPaletteOpen]);

  // Listen for custom settings event from command palette
  useEffect(() => {
    const handleOpenSettings = () => setSettingsOpen(true);
    window.addEventListener('open-settings', handleOpenSettings);
    return () => window.removeEventListener('open-settings', handleOpenSettings);
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
        if (isCloudTenant()) {
          // Cloud: the provider is already provisioned, so setup is gated on
          // whether the account finished onboarding (tag categories,
          // integrations). The app initializes regardless — the wizard, if
          // shown, overlays an already-loading app.
          await useSettingsStore.getState().fetchSettings();
          const done = useSettingsStore.getState().settings[CLOUD_ONBOARDING_SETTING] === 'true';
          setIsSetupRequired(!done);
          await initializeApp();
          return;
        }

        const configured = await verifyProviderConfigured();
        setIsSetupRequired(!configured);

        if (configured) {
          // Only initialize app if provider is configured
          await initializeApp();
        }
      } catch (error) {
        console.error('Failed to check provider configuration:', error);
        // If check fails, show setup anyway
        setIsSetupRequired(true);
      }
    };

    checkSetup();
  }, []);

  const initializeApp = async () => {
    // Paint cached tag tree + atoms first (if any) so the UI has something
    // to show while the network call races. Hydration reads the persisted
    // `activeId`; it no-ops on first-ever session (no cache yet).
    //
    // `fetchDatabases` runs in parallel so `activeId` is fresh by the time
    // atoms/tags fetches finish and want to write to the cache. We don't
    // await it because we don't want cache hydration to block on a network
    // round-trip — the offline case still needs to paint instantly.
    void useDatabasesStore.getState().fetchDatabases();
    await Promise.all([
      useAtomsStore.getState().hydrateFromCache(),
      useTagsStore.getState().hydrateFromCache(),
    ]);
    await Promise.all([fetchAtoms(), fetchTags()]);
  };

  const handleSetupComplete = async () => {
    // On cloud, persist completion per-tenant so the wizard doesn't reappear on
    // the next visit or another device. Best-effort: a failed write just means
    // the user sees the (idempotent) wizard again rather than losing data.
    if (isCloudTenant()) {
      try {
        await useSettingsStore.getState().setSetting(CLOUD_ONBOARDING_SETTING, 'true');
      } catch (e) {
        console.error('Failed to record onboarding completion:', e);
      }
    }
    setIsSetupRequired(false);
    // Now initialize the app
    await initializeApp();
  };

  // Show loading while checking
  if (isSetupRequired === null) {
    return (
      <div className={`flex h-full items-center justify-center bg-[var(--color-bg-main)] ${isTauri() ? 'pt-[28px]' : ''}`}>
        <span className="text-[var(--color-text-secondary)]">Loading...</span>
      </div>
    );
  }

  // Show onboarding wizard if setup is required
  if (isSetupRequired) {
    return (
      <div className={`flex h-full overflow-hidden bg-[var(--color-bg-main)] ${isTauri() ? 'pt-[28px]' : ''}`}>
        <OnboardingWizard onComplete={handleSetupComplete} />
      </div>
    );
  }

  return (
    <div className="flex h-full overflow-hidden bg-[var(--color-bg-main)]">
      <RouterBridge />
      <LeftPanel />
      <MainView />
      <LoadingIndicator />
      <ServerConnectionStatus />
      <CommandPalette
        isOpen={commandPaletteOpen}
        onClose={closeCommandPalette}
      />
      <SearchPalette
        isOpen={searchPaletteOpen}
        onClose={closeSearchPalette}
        initialQuery={searchPaletteInitialQuery}
      />
      <SettingsModal
        isOpen={settingsOpen}
        onClose={() => setSettingsOpen(false)}
      />
    </div>
  );
}
