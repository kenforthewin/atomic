import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

/**
 * In production the cloud server serves this build same-origin, so every API
 * call is a same-origin path under `credentials: 'include'`. In `vite dev` we
 * reproduce that by proxying the backend route families to a local
 * `atomic-cloud serve` (default `:8080`, override with `ATOMIC_CLOUD_DEV_API`)
 * — so the dev page is same-origin with the API and the session cookie rides
 * along exactly as it will in production. Only the server's route families are
 * proxied; everything else is served by Vite (the SPA + HMR).
 *
 * https://vite.dev/config/
 */
const API_TARGET = process.env.ATOMIC_CLOUD_DEV_API ?? 'http://localhost:8080';

// The backend route families the cloud server owns (see `server.rs`); Vite
// serves every other path as the SPA. NB `/signup` and `/login` are SPA *page*
// routes — only their backend sub-paths (`/signup/request-link`,
// `/login/request-link`, `…/complete`) proxy, so navigating to the page itself
// is served by Vite while the form POST reaches the API.
const PROXIED = [
  '/api',
  '/signup/request-link',
  '/signup/complete',
  '/login/request-link',
  '/login/complete',
  '/oauth',
  '/.well-known',
  '/mcp',
  '/ws',
  '/health',
  '/ready',
  '/billing',
];

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    proxy: Object.fromEntries(
      PROXIED.map((path) => [
        path,
        { target: API_TARGET, changeOrigin: true, ws: path === '/ws' },
      ]),
    ),
  },
  build: {
    // The cloud server (actix-files) serves this dist with an SPA fallback.
    // A flat, predictable output keeps that wiring trivial.
    outDir: 'dist',
    emptyOutDir: true,
  },
});
