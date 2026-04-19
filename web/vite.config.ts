import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { visualizer } from 'rollup-plugin-visualizer'
import path from 'path'
import { readFileSync } from 'fs'

// Workspace version source of truth is the root Cargo.toml. Parsed once at
// config eval and injected as __APP_VERSION__ so the web UI footer stays in
// lockstep with the Rust crates without a manual bump.
function readWorkspaceVersion(): string {
  try {
    const toml = readFileSync(path.resolve(__dirname, '../Cargo.toml'), 'utf8')
    const m = toml.match(/^\s*version\s*=\s*"([^"]+)"/m)
    return m?.[1] ?? '0.0.0'
  } catch {
    return '0.0.0'
  }
}

export default defineConfig({
  define: {
    __APP_VERSION__: JSON.stringify(readWorkspaceVersion()),
  },
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
      // Trailing slash prevents matching frontend routes like /api-keys.
      '/api/': {
        target: process.env.VITE_CONSOLE_URL || 'http://localhost:3001',
        ws: true,
        changeOrigin: true,
      },
    },
  },
})
