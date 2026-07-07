import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { VitePWA } from 'vite-plugin-pwa'
import path from 'path'

const isWebBuild = process.env.VITE_BUILD_TARGET === 'web'
const EDITOR_PEER_DEPS = [
  '@codemirror/autocomplete',
  '@codemirror/commands',
  '@codemirror/lang-markdown',
  '@codemirror/language',
  '@codemirror/search',
  '@codemirror/state',
  '@codemirror/view',
  '@lezer/common',
  '@lezer/highlight',
]
const EDITOR_DEPENDENCY_MARKERS = [
  `${path.sep}node_modules${path.sep}@codemirror${path.sep}`,
  `${path.sep}node_modules${path.sep}@lezer${path.sep}`,
  `${path.sep}node_modules${path.sep}codemirror${path.sep}`,
  `${path.sep}node_modules${path.sep}crelt${path.sep}`,
  `${path.sep}node_modules${path.sep}w3c-keyname${path.sep}`,
]

// CodeMirror language grammars (both `@codemirror/lang-*` and the `@lezer/*`
// parser grammars they pull in) are loaded on demand via
// `@codemirror/language-data`. Rollup splits dynamic imports into their own
// chunks by default, but our `editor` manualChunk rule was greedy enough to
// swallow them — which made "on demand" a lie and bloated the editor chunk.
// Exclude them from the editor chunk so each grammar stays as its own lazy
// chunk that loads only when a user picks that language in a code block.
const LEZER_CORE_PACKAGES = new Set(['common', 'lr', 'highlight'])
function isLazyGrammarModule(id: string): boolean {
  if (id.includes(`${path.sep}node_modules${path.sep}@codemirror${path.sep}lang-`)) {
    return true
  }
  const lezerMatch = id.match(/[\\/]node_modules[\\/]@lezer[\\/]([^\\/]+)[\\/]/)
  return lezerMatch !== null && !LEZER_CORE_PACKAGES.has(lezerMatch[1])
}

export default defineConfig({
  plugins: [
    react(),
    tailwindcss(),
    // The service worker + manifest only make sense for the web build. In
    // Tauri desktop the assets are already bundled, and a SW here would just
    // be dead code (Tauri's webview runs on custom schemes that don't play
    // well with SW registration anyway).
    ...(isWebBuild
      ? [
          VitePWA({
            registerType: 'autoUpdate',
            injectRegister: 'auto',
            // We author the manifest in `public/manifest.webmanifest` so it's
            // readable without a build step. Tell the plugin not to generate
            // its own copy.
            manifest: false,
            includeAssets: ['icons/icon-256.png', 'icons/icon-1024.png', 'vite.svg'],
            workbox: {
              // App assets (JS/CSS/fonts/images) — cache-first via precache.
              globPatterns: ['**/*.{js,css,html,svg,png,webmanifest,woff,woff2}'],
              // Never precache the SPA shell as precache — we want the SW to
              // fall back to index.html for every navigation so deep-links
              // like /atoms/:id work when the server is a static host.
              navigateFallback: '/index.html',
              // Server endpoints must NEVER be intercepted and rewritten to
              // index.html. /oauth/ and /.well-known/ are hit by top-level
              // browser navigations (MCP remote-auth flow from claude.ai),
              // so without this denylist the SPA shell takes over the URL
              // and the user lands on the dashboard with OAuth params in
              // the querystring instead of the consent page.
              navigateFallbackDenylist: [
                /^\/api\//,
                /^\/health/,
                /^\/ready/,
                /^\/ws/,
                /^\/oauth\//,
                /^\/\.well-known\//,
                /^\/mcp(\/|$)/,
                // Atomic Cloud server-owned pages on the same origin. The
                // account dashboard and auth pages are a different SPA — if
                // the product SW answers these navigations it serves the
                // wrong app's shell (and on tenant hosts, hijacks the
                // dashboard entirely).
                /^\/account(\/|$)/,
                /^\/signup(\/|$)/,
                /^\/login(\/|$)/,
                /^\/billing(\/|$)/,
              ],
              runtimeCaching: [
                {
                  // Google Fonts stylesheets — cache for a day.
                  urlPattern: /^https:\/\/fonts\.googleapis\.com\//,
                  handler: 'StaleWhileRevalidate',
                  options: { cacheName: 'google-fonts-stylesheets' },
                },
                {
                  // Font files themselves — long-lived, cache for a year.
                  urlPattern: /^https:\/\/fonts\.gstatic\.com\//,
                  handler: 'CacheFirst',
                  options: {
                    cacheName: 'google-fonts-webfonts',
                    expiration: { maxAgeSeconds: 60 * 60 * 24 * 365, maxEntries: 30 },
                  },
                },
              ],
            },
          }),
        ]
      : []),
  ],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    allowedHosts: true,
    watch: {
      // The Capacitor iOS scaffold copies dist-web into mobile/ios/App/App/public
      // during `cap sync`. Vite would otherwise crawl those files and get
      // confused by the stale absolute source paths in their sourcemaps.
      ignored: ['**/mobile/ios/**', '**/target/**', '**/dist-web/**', '**/dist/**'],
    },
    proxy: isWebBuild
      ? {
          '/api': {
            target: 'http://127.0.0.1:8080',
            changeOrigin: true,
          },
          '/health': {
            target: 'http://127.0.0.1:8080',
            changeOrigin: true,
          },
          '/ws': {
            target: 'ws://127.0.0.1:8080',
            ws: true,
            configure: (proxy) => {
              proxy.on('error', () => {});
            },
          },
        }
      : undefined,
  },
  resolve: {
    // Required when developing against a local file:../atomic-editor package.
    // CM6 extensions use instanceof checks internally, so the editor package
    // and the app must share one copy of each CodeMirror peer dependency.
    dedupe: EDITOR_PEER_DEPS,
    ...(isWebBuild
      ? {
          alias: {
            '@tauri-apps/api/core': path.resolve(__dirname, 'src/lib/stubs/tauri-core.ts'),
            '@tauri-apps/api/event': path.resolve(__dirname, 'src/lib/stubs/tauri-event.ts'),
            '@tauri-apps/plugin-dialog': path.resolve(__dirname, 'src/lib/stubs/tauri-dialog.ts'),
            '@tauri-apps/plugin-opener': path.resolve(__dirname, 'src/lib/stubs/tauri-opener.ts'),
            '@tauri-apps/plugin-fs': path.resolve(__dirname, 'src/lib/stubs/tauri-fs.ts'),
          },
        }
      : {}),
  },
  build: {
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (isLazyGrammarModule(id)) return
          if (EDITOR_DEPENDENCY_MARKERS.some((marker) => id.includes(marker))) {
            return 'editor'
          }
        },
      },
    },
  },
})
