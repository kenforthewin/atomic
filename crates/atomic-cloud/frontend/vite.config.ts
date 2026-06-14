import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// https://vite.dev/config/
export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    // The cloud server (actix-files) serves this dist with an SPA fallback.
    // A flat, predictable output keeps that wiring trivial.
    outDir: 'dist',
    emptyOutDir: true,
  },
});
