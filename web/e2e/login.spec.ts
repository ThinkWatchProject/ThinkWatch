import { expect, test } from '@playwright/test';

/**
 * Smoke + happy-path tests for the login screen. Drives only what
 * is reachable from a fresh browser session: the page renders, the
 * email + password fields exist, an obviously bad credential shows
 * an error.
 *
 * Tests that exercise the full authenticated flow live in
 * `auth.spec.ts` and rely on a seeded admin user.
 */

test.describe('Login page', () => {
  test('renders the email + password form', async ({ page }) => {
    await page.goto('/login');

    // The form is rendered as native inputs labelled by the i18n
    // keys `auth.email` / `auth.password`. shadcn Input + Label
    // wires the labels through `htmlFor`, so accessible-name
    // selectors find them reliably across translations.
    await expect(page.getByLabel(/Email/i)).toBeVisible();
    await expect(page.getByLabel(/Password/i)).toBeVisible();
    await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
  });

  test('shows an error banner for invalid credentials', async ({ page }) => {
    await page.goto('/login');

    await page.getByLabel(/Email/i).fill('does-not-exist@example.com');
    await page.getByLabel(/Password/i).fill('definitely-wrong-password');
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();

    // Backend returns 401 → frontend surfaces a generic auth error.
    // We don't lock to a specific string because the i18n bundle
    // owns the wording — assert the alert region appears instead.
    await expect(page.getByRole('alert')).toBeVisible({ timeout: 15_000 });
  });

  test('the language switcher toggles UI strings to Chinese', async ({ page }) => {
    await page.goto('/login');

    // Find the language switcher (button with English label) and
    // flip it. The button text varies between two-letter codes
    // ("EN" / "中文") and full names depending on the component;
    // accept either.
    const switcher = page.getByRole('button', { name: /中文|EN|English/i }).first();
    if (await switcher.isVisible().catch(() => false)) {
      await switcher.click();
      // After a switch, the password label text changes.
      await expect(
        page.getByText(/密码|登录|登入/, { exact: false }).first(),
      ).toBeVisible({ timeout: 10_000 });
    } else {
      test.skip(true, 'Language switcher not visible on login screen');
    }
  });
});
