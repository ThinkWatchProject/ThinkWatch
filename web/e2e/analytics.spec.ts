import { expect, test } from './fixtures';

/**
 * Analytics pages — `/analytics/usage` and `/analytics/costs`.
 *
 * Both render charts driven by `/api/admin/analytics/...`
 * endpoints. We don't assert chart contents (recharts SVGs are
 * brittle to compare); we assert the page heading renders, a
 * canvas/svg chart node mounts, and there's no error banner.
 */

test('usage analytics page renders heading and chart container', async ({
  adminPage: page,
}) => {
  await page.goto('/analytics/usage');

  await expect(
    page.getByRole('heading', { name: /Usage Analytics|使用分析|Usage/i }),
  ).toBeVisible({ timeout: 15_000 });

  // Recharts renders into a `<svg class="recharts-surface">`. Either
  // that or any `<svg>` root in the main panel proves the chart
  // mounted (vs an empty error placeholder).
  const chart = page.locator('svg').first();
  await expect(chart).toBeVisible({ timeout: 10_000 });

  expect(await page.getByRole('alert').count()).toBe(0);
});

test('cost analytics page renders heading and chart container', async ({
  adminPage: page,
}) => {
  await page.goto('/analytics/costs');

  await expect(
    page.getByRole('heading', { name: /Cost Analytics|成本分析|Cost/i }),
  ).toBeVisible({ timeout: 15_000 });

  const chart = page.locator('svg').first();
  await expect(chart).toBeVisible({ timeout: 10_000 });

  expect(await page.getByRole('alert').count()).toBe(0);
});
