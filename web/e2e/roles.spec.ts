import { expect, test } from './fixtures';

/**
 * Admin → Roles & Permissions page.
 *
 * The page lists every system + custom role with policy preview,
 * member count, and per-row Edit / Reset / Delete affordances.
 * The table is rendered via a synchronous query, so loading is
 * effectively instant once the admin session is authenticated.
 */

test('roles page renders the seeded system roles', async ({ adminPage: page }) => {
  await page.goto('/admin/roles');

  // Page heading — pinned via the i18n key the page uses.
  await expect(
    page.getByRole('heading', { name: /Roles & Permissions|角色与权限/i }),
  ).toBeVisible({ timeout: 15_000 });

  // The seeded roles always present in a fresh deployment. Names
  // are hard-coded in the migrations so any of these missing means
  // the role-list query is broken.
  for (const name of ['super_admin', 'admin', 'developer', 'viewer']) {
    await expect(
      page.getByText(new RegExp(`\\b${name}\\b`, 'i')).first(),
    ).toBeVisible();
  }

  // No red error banner — the role list query failing would render
  // an Alert here.
  expect(await page.getByRole('alert').count()).toBe(0);
});

test('roles page exposes a create-role affordance for super-admin', async ({
  adminPage: page,
}) => {
  await page.goto('/admin/roles');

  // The "create role" button is gated on `roles:create` (super-admin
  // only). Our fixture lands a super-admin session, so it must be
  // visible. A regression that flips the permission check would
  // hide the button.
  const createBtn = page
    .getByRole('button', { name: /Create|New role|新建角色/i })
    .first();
  await expect(createBtn).toBeVisible({ timeout: 10_000 });
});
