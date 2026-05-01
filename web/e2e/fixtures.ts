import { test as base, type Page, type APIRequestContext } from '@playwright/test';

/**
 * Shared E2E fixtures.
 *
 * `adminPage` lands an authenticated super-admin session in the
 * browser. The credentials default to whatever the repo's setup
 * wizard provisioned the first time someone hit `/setup` — most
 * dev environments use `admin@thinkwatch.local` / `Admin_pass_1!`,
 * so override via env when CI seeds a different pair:
 *
 *   PW_ADMIN_EMAIL  PW_ADMIN_PASSWORD
 *
 * The fixture also ensures the platform is initialised: a fresh
 * database whose setup wizard hasn't run yet trips the wizard via
 * the public POST /api/setup/initialize and uses the same
 * email/password to land an admin.
 */

// Use the IPv4 literal so node's resolver doesn't hit `::1` first
// (macOS resolves `localhost` to IPv6 by default and the backend
// binds 0.0.0.0 on IPv4 only).
const CONSOLE = process.env.PW_CONSOLE_API ?? 'http://127.0.0.1:3001';
const ADMIN_EMAIL = process.env.PW_ADMIN_EMAIL ?? 'admin@thinkwatch.local';
const ADMIN_PASSWORD = process.env.PW_ADMIN_PASSWORD ?? 'Admin_pass_1!';

async function ensureInitialised(request: APIRequestContext) {
  const status = await request.get(`${CONSOLE}/api/setup/status`);
  if (!status.ok()) return;
  const body = await status.json();
  if (body.initialized) return;

  // Wizard is one-shot — initialise with the test admin.
  const init = await request.post(`${CONSOLE}/api/setup/initialize`, {
    data: {
      admin: {
        email: ADMIN_EMAIL,
        display_name: 'E2E Admin',
        password: ADMIN_PASSWORD,
      },
      site_name: 'ThinkWatch (E2E)',
    },
  });
  if (!init.ok()) {
    const text = await init.text();
    throw new Error(`setup/initialize failed: ${init.status()} ${text}`);
  }
}

async function loginAdmin(page: Page) {
  await page.goto('/');
  await page.getByLabel(/Email/i).fill(ADMIN_EMAIL);
  await page.getByLabel(/Password/i).fill(ADMIN_PASSWORD);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  // The SPA doesn't navigate the URL on login — it just swaps the
  // rendered tree from <LoginPage> to <AppShell>. Wait for the shell
  // (sidebar nav) to appear; that's the post-login signal.
  try {
    await page
      .getByRole('button', { name: /Toggle Sidebar/i })
      .first()
      .waitFor({ state: 'visible', timeout: 15_000 });
  } catch (e) {
    throw new Error(
      `Login failed for ${ADMIN_EMAIL} — set PW_ADMIN_EMAIL and PW_ADMIN_PASSWORD to match your dev DB.\n` +
        `Defaults: admin@thinkwatch.local / Admin_pass_1!\n` +
        `Underlying error: ${e instanceof Error ? e.message : e}`,
    );
  }
}

type Fixtures = {
  adminPage: Page;
};

export const test = base.extend<Fixtures>({
  adminPage: async ({ page, request }, use) => {
    await ensureInitialised(request);
    await loginAdmin(page);
    await use(page);
  },
});

export { expect } from '@playwright/test';
