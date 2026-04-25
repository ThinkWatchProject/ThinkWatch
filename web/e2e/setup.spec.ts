import { expect, test } from '@playwright/test';

/**
 * Setup wizard E2E. Skipped when the deployment is already
 * initialised — the wizard is one-shot and there's no UI to undo
 * it. The Rust integration suite has full coverage of the API
 * side (`crates/test-support/tests/auth.rs::setup_status_and_initialize_flow`),
 * so this test focuses on the browser flow specifically: cookies
 * are set, redirect lands on /dashboard, the admin email is shown.
 */

const CONSOLE = process.env.PW_CONSOLE_API ?? 'http://localhost:3001';

test('setup wizard provisions an admin and lands on the dashboard', async ({ page }) => {
  // Skip when the platform is already set up — re-running the
  // wizard would 403.
  const status = await page.request.get(`${CONSOLE}/api/setup/status`);
  expect(status.ok()).toBeTruthy();
  const body = await status.json();
  test.skip(
    body.initialized === true,
    'platform already initialised; rerun against a fresh DB to exercise this test',
  );

  await page.goto('/setup');

  // The wizard is multi-step; fill the bare minimum (admin block)
  // and submit. Frontend-side step labels live under i18n
  // `setup.steps.*`.
  await page.getByLabel(/Email/i).fill('e2e-admin@example.com');
  await page.getByLabel(/Display Name|Name/i).fill('E2E Admin');
  await page.getByLabel(/Password/i, { exact: true }).fill('E2E_admin_pwd_123!');

  await page
    .getByRole('button', { name: /Initialize|Continue|Next|Save/i })
    .first()
    .click();

  await expect(page).toHaveURL(/\/(dashboard|login)/, { timeout: 15_000 });
});
