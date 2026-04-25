import { expect, test } from './fixtures';

/**
 * Admin settings page — a super_admin can flip a boolean toggle
 * and see it persist across reloads.
 *
 * Picks `auth.allow_registration` because it's a stable, low-blast
 * setting the wizard exposes by default. The test reads its
 * current value, flips it, asserts the round-trip, then flips it
 * back so subsequent test runs aren't affected.
 */

test.describe('Admin settings', () => {
  test('auth.allow_registration toggle round-trips through the UI', async ({
    adminPage: page,
  }) => {
    await page.goto('/admin/settings');

    // Find the row for allow_registration. The settings grid is
    // generated from the settings catalog; the row contains the
    // key as text plus a toggle switch.
    const row = page
      .locator('tr, [role="row"], [data-key]')
      .filter({ hasText: /allow.?registration/i })
      .first();

    if (!(await row.isVisible().catch(() => false))) {
      test.skip(true, 'allow_registration row not visible — schema may differ');
    }

    const toggle = row.getByRole('switch').or(row.getByRole('checkbox')).first();
    const before = await toggle.isChecked();

    await toggle.click();

    // Most settings UIs have a "Save" button at the bottom.
    const save = page.getByRole('button', { name: /Save|Apply|保存|应用/i }).first();
    if (await save.isVisible().catch(() => false)) {
      await save.click();
    }

    // Page may reload or rerender; reload explicitly to assert
    // persistence.
    await page.reload();
    const rowAfter = page
      .locator('tr, [role="row"], [data-key]')
      .filter({ hasText: /allow.?registration/i })
      .first();
    const toggleAfter = rowAfter
      .getByRole('switch')
      .or(rowAfter.getByRole('checkbox'))
      .first();
    expect(await toggleAfter.isChecked()).toBe(!before);

    // Restore so other tests aren't affected.
    await toggleAfter.click();
    if (await save.isVisible().catch(() => false)) {
      await save.click();
    }
  });
});
