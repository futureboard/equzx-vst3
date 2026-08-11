import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react(), tailwindcss()],
  // The plugin build serves these files from inside the binary over a custom
  // protocol, where the document lives at the root of a synthetic host. Relative
  // URLs work there and on a plain static server alike; absolute ones would only
  // work if the page were always served from a domain root.
  base: './',
  build: {
    // Inlined assets become data: URIs, which the protocol handler never has to
    // serve — worth it for the small SVGs, not for the fonts.
    assetsInlineLimit: 8192,
    // The webview is a current Chromium/WebKit; there is no old browser to
    // support, so don't spend bundle size on transpiling for one.
    target: 'esnext',
    rollupOptions: {
      output: {
        // A single chunk loads in one protocol round trip and keeps the editor's
        // first paint off a waterfall.
        manualChunks: undefined,
      },
    },
  },
})
