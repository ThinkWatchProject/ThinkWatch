import { expect, test } from './fixtures';

/**
 * MCP Store — public marketplace + install flow.
 *
 * The page hits `/api/mcp-store/list` to populate the catalog and
 * presents a per-row Install button that opens a config dialog.
 * We don't drive a full install (it'd require live registry sync);
 * we just assert the page renders without errors and the search
 * input is present.
 */

test('MCP store page renders the catalog header', async ({ adminPage: page }) => {
  await page.goto('/mcp/store');

  await expect(
    page.getByRole('heading', { name: /MCP Store/i }),
  ).toBeVisible({ timeout: 15_000 });

  // The search box drives the catalog filter. Pin its presence so
  // a refactor that drops the input is caught loudly.
  const search = page
    .getByPlaceholder(/Search|搜索/i)
    .or(page.getByRole('searchbox'))
    .or(page.getByRole('textbox'))
    .first();
  await expect(search).toBeVisible();

  expect(await page.getByRole('alert').count()).toBe(0);
});
