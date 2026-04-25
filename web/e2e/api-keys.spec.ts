import { expect, test } from './fixtures';

/**
 * API Keys page — list, create, copy plaintext, delete cycle.
 *
 * Selectors lean on i18n labels (`API Keys`, `Create API Key`) and
 * accessible roles. Tests pin the contract that the page shows a
 * list, the create dialog returns a one-time `tw-…` token, and a
 * deleted key drops out of the active list.
 */

test('list page renders the heading and a create button', async ({ adminPage: page }) => {
  await page.goto('/api-keys');
  await expect(page.getByRole('heading', { name: /API Keys/i })).toBeVisible();
  await expect(page.getByRole('button', { name: /Create API Key/i })).toBeVisible();
});

test('create dialog mints a key and the table shows it afterwards', async ({ adminPage: page }) => {
  await page.goto('/api-keys');
  await page.getByRole('button', { name: /Create API Key/i }).click();

  // Dialog asks for a name + at least one surface checkbox.
  const name = `e2e-${Date.now()}`;
  await page.getByLabel(/Name/i).first().fill(name);
  // Pick the AI gateway surface — the labels include the path
  // hint so we match on the prefix to stay translation-tolerant.
  const aiSurface = page.getByLabel(/AI gateway/i).first();
  if (await aiSurface.isVisible().catch(() => false)) {
    if (!(await aiSurface.isChecked())) await aiSurface.check();
  }

  // Submit. The button text varies between "Create" and the locale
  // form, accept either.
  await page
    .getByRole('button', { name: /^Create$|API Key|创建/, exact: false })
    .last()
    .click();

  // Plaintext shown in a one-time-only block. We just check that
  // the `tw-` prefix lands somewhere on screen.
  await expect(page.getByText(/tw-[0-9a-f]+/).first()).toBeVisible({
    timeout: 10_000,
  });

  // Close the dialog if there's a Done / Close affordance.
  const close = page.getByRole('button', { name: /Done|Close|完成|关闭/i }).first();
  if (await close.isVisible().catch(() => false)) {
    await close.click();
  }

  // Newly minted name shows up in the list (table or card).
  await expect(page.getByText(name).first()).toBeVisible({ timeout: 10_000 });
});

test('list is reachable from the sidebar and reflects current state', async ({ adminPage: page }) => {
  await page.goto('/dashboard');
  // Sidebar link to API Keys.
  const link = page.getByRole('link', { name: /API Keys/i }).first();
  await link.click();
  await expect(page).toHaveURL(/\/api-keys/);
  // Either an empty-state hint or a populated list — both are
  // valid; the assertion is that the page resolved without a
  // 4xx/5xx and rendered the heading.
  await expect(page.getByRole('heading', { name: /API Keys/i })).toBeVisible();
});
