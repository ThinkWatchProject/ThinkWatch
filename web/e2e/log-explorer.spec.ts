import { expect, test } from './fixtures';

/**
 * Unified log explorer — `/logs`.
 *
 * Reads CH-backed `gateway_logs` / `mcp_logs` / `audit_logs` /
 * `app_logs` through `/api/admin/logs/unified`. The page renders
 * filter inputs + a virtualised table; we assert the heading and
 * a search affordance render, with no error banner.
 */

test('log explorer page renders header + filter input', async ({ adminPage: page }) => {
  await page.goto('/logs');

  await expect(page.getByRole('heading', { name: /Logs|日志/i })).toBeVisible({
    timeout: 15_000,
  });

  // The search input drives substring filter on the unified row
  // stream. Either a placeholder-tagged input or any textbox works.
  const search = page
    .getByPlaceholder(/Search|搜索/i)
    .or(page.getByRole('searchbox'))
    .or(page.getByRole('textbox').first());
  await expect(search.first()).toBeVisible();

  expect(await page.getByRole('alert').count()).toBe(0);
});
