/**
 * Stub for `virtual:pwa-register` in non-web builds (Tauri desktop). The
 * VitePWA plugin — and therefore the real virtual module — only exists when
 * VITE_BUILD_TARGET=web; this mirrors its surface as a no-op, the same
 * pattern as the `@tauri-apps/*` stubs used in the opposite direction.
 */
export function registerSW(_options?: unknown): (reloadPage?: boolean) => Promise<void> {
  return async () => {};
}
