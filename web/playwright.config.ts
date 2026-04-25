import { defineConfig, devices } from '@playwright/test';

/**
 * Playwright configuration for ThinkWatch frontend E2E tests.
 *
 * Required infra (the test runner does NOT spin them up):
 *   - Postgres + Redis from `make infra`
 *   - Backend from `make dev-backend` (gateway :3000 + console :3001)
 *
 * The vite dev server IS started for us via the `webServer` block.
 * Override host/port with PW_BASE_URL when pointing at a deployed
 * environment.
 *
 * Each test file is responsible for seeding any DB state it relies
 * on — there are no global setup hooks. The setup-wizard test, in
 * particular, must run against a fresh database.
 */

const BASE_URL = process.env.PW_BASE_URL ?? 'http://localhost:5173';

export default defineConfig({
  testDir: './e2e',
  fullyParallel: false, // shares one backend; cap at 1 worker
  workers: 1,
  reporter: [['list'], ['html', { open: 'never' }]],
  timeout: 60_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: BASE_URL,
    trace: 'retain-on-failure',
    video: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  webServer: process.env.PW_NO_WEBSERVER
    ? undefined
    : {
        command: 'pnpm dev',
        url: BASE_URL,
        reuseExistingServer: true,
        timeout: 60_000,
      },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
});
