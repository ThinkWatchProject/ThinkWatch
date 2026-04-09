import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { visualizer } from 'rollup-plugin-visualizer'
import path from 'path'

export default defineConfig({
  plugins: [
    react(),
    tailwindcss(),
    // Bundle analysis: generates stats.html after `pnpm build`
    visualizer({ filename: 'stats.html', gzipSize: true }) as any,
  ],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    chunkSizeWarningLimit: 600,
    rollupOptions: {
      output: {
        // Conservative manual chunking. We only carve out the
        // truly heavy route-specific deps (codemirror on /roles,
        // recharts on /dashboard + /analytics) so they don't bloat
        // the initial page load and are cached separately. React,
        // radix, i18next etc. all stay in the default vendor split
        // — the bundler does a better job with those than a
        // hand-rolled regex would.
        manualChunks: (id: string) => {
          if (!id.includes('node_modules')) return undefined
          if (id.includes('@codemirror') || id.includes('@lezer')) {
            return 'vendor-codemirror'
          }
          if (id.includes('recharts') || id.includes('d3-')) {
            return 'vendor-charts'
          }
          // Date pickers pull in date-fns + react-day-picker which
          // are ~90KB combined and only used on a couple of pages.
          if (id.includes('react-day-picker') || id.includes('date-fns')) {
            return 'vendor-date'
          }
          return undefined
        },
      },
    },
  },
  server: {
    proxy: {
      // Console API → console port (configurable via env). `ws: true`
      // forwards WebSocket upgrade requests so /api/dashboard/ws works in
      // dev exactly like in prod.
      '/api': {
        target: process.env.VITE_CONSOLE_URL || 'http://localhost:3001',
        ws: true,
        changeOrigin: true,
      },
      // Gateway API → gateway port (configurable via env)
      '/v1': process.env.VITE_GATEWAY_URL || 'http://localhost:3000',
      '/mcp': process.env.VITE_GATEWAY_URL || 'http://localhost:3000',
    },
  },
})
