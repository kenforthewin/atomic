import { registerSW } from 'virtual:pwa-register';
import { toast } from 'sonner';

/**
 * PWA update lifecycle for the web build.
 *
 * The service worker precaches the whole app (including the shell), so
 * without this module a deployed update never reaches an open tab: the
 * browser only checks for a new worker on navigation, and even when one
 * installs, nothing tells the user the page they're looking at is stale.
 * That's how clients end up "stuck" until someone unregisters the worker.
 *
 * Three legs, matching the vite-plugin-pwa `prompt` flow:
 *
 * 1. Register immediately (not on window load) so the update check races
 *    the app boot instead of waiting behind it.
 * 2. Re-check hourly, and when a hidden tab becomes visible again — the
 *    knowledge-base tab people keep pinned for days would otherwise never
 *    look for updates between navigations.
 * 3. When a new worker is installed and waiting, show a persistent toast.
 *    The update applies only when the user clicks Refresh: an automatic
 *    reload could eat an unsaved note, so activation stays user-driven.
 *    (Closing every tab also activates it, the normal SW lifecycle.)
 *
 * In Tauri this module resolves to a stub (see `stubs/pwa-register.ts`) —
 * the desktop app has no service worker.
 */

const UPDATE_CHECK_INTERVAL_MS = 60 * 60 * 1000;
/** Minimum gap between a scheduled check and a visibility-triggered one. */
const VISIBILITY_CHECK_MIN_GAP_MS = 10 * 60 * 1000;

export function setupPwaUpdates(): void {
  if (!('serviceWorker' in navigator)) return;

  const updateSW = registerSW({
    immediate: true,
    onNeedRefresh() {
      toast('A new version of Atomic is ready', {
        id: 'pwa-update',
        duration: Infinity,
        action: {
          label: 'Refresh',
          onClick: () => {
            void updateSW(true);
          },
        },
      });
    },
    onRegisteredSW(_swUrl, registration) {
      if (!registration) return;
      let lastCheck = Date.now();
      const check = () => {
        if (registration.installing || !navigator.onLine) return;
        lastCheck = Date.now();
        registration.update().catch(() => {
          // Server unreachable — the next tick will try again.
        });
      };
      setInterval(check, UPDATE_CHECK_INTERVAL_MS);
      document.addEventListener('visibilitychange', () => {
        if (
          document.visibilityState === 'visible' &&
          Date.now() - lastCheck > VISIBILITY_CHECK_MIN_GAP_MS
        ) {
          check();
        }
      });
    },
    onRegisterError(err) {
      // Non-fatal: the app works without a SW (e.g. Capacitor webviews,
      // where custom schemes reject registration).
      console.warn('Service worker registration failed:', err);
    },
  });
}
