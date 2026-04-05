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
  server: {
    proxy: {
      // Console API → console port (configurable via env)
      '/api': process.env.VITE_CONSOLE_URL || 'http://localhost:3001',
      // Gateway API → gateway port (configurable via env)
      '/v1': process.env.VITE_GATEWAY_URL || 'http://localhost:3000',
      '/mcp': process.env.VITE_GATEWAY_URL || 'http://localhost:3000',
    },
  },
})
