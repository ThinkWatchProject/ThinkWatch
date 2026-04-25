import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'
import path from 'path'

export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    // Vitest covers component / hook unit tests under `src/`. Keep
    // `e2e/` (Playwright browser specs) out of its sweep — picking
    // them up runs Playwright's `test()` inside the vitest harness
    // and explodes with "did not expect test() to be called here".
    exclude: ['e2e/**', 'node_modules/**', 'dist/**'],
  },
  resolve: {
    alias: { '@': path.resolve(__dirname, './src') },
  },
})
