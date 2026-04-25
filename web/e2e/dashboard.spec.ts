import { expect, test } from './fixtures';

/**
 * Dashboard happy path — login lands here and the route renders
 * the "operational" status pill plus the analytics cards.
 *
 * The dashboard also opens a WebSocket for live stats; we don't
 * assert on push payloads (that path is exercised in the Rust
 * integration suite once the dashboard layout endpoint is wired
 * up there) — only that the static cards render without errors.
 */

test('dashboard renders core widgets after login', async ({ adminPage: page }) => {
  await page.goto('/dashboard');

  // Status indicator carries one of the systemStatus.* labels.
  await expect(
    page
      .getByText(/operational|degraded|down|系统/i)
      .first(),
  ).toBeVisible({ timeout: 15_000 });

  // The page must be free of red error banners (lucide AlertCircle
  // backed by role=alert).
  const alertCount = await page.getByRole('alert').count();
  expect(alertCount).toBe(0);
});

test('navigation: dashboard → analytics → back', async ({ adminPage: page }) => {
  await page.goto('/dashboard');

  const analyticsLink = page.getByRole('link', { name: /Analytics|Costs|Usage/i }).first();
  if (!(await analyticsLink.isVisible().catch(() => false))) {
    test.skip(true, 'analytics nav link not present in this build');
  }
  await analyticsLink.click();
  await expect(page).toHaveURL(/\/(analytics|costs|usage)/i);

  await page.goBack();
  await expect(page).toHaveURL(/\/dashboard/);
});
