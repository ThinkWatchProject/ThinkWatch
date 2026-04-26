import { expect, test } from './fixtures';
import { request as playwrightRequest } from '@playwright/test';

/**
 * Route-level RBAC guard: a developer who types /admin/users in the
 * address bar must land on the Forbidden page, not on a half-rendered
 * admin shell where every API call returns 403.
 *
 * The admin fixture is the canonical "everything works" baseline.
 * For the negative case we provision a fresh `developer` user via
 * the admin API on first run, then log in as that user and exercise
 * a handful of admin-only routes.
 */

const CONSOLE =
  process.env.PW_CONSOLE_API ?? 'http://127.0.0.1:3001';
const ADMIN_EMAIL = process.env.PW_ADMIN_EMAIL ?? 'admin@thinkwatch.local';
const ADMIN_PASSWORD = process.env.PW_ADMIN_PASSWORD ?? 'Admin_pass_1!';

const DEV_EMAIL = 'e2e-dev-route-guard@example.com';
const DEV_PASSWORD = 'DevTest_pass_1!';

/**
 * Stand up a developer user once per worker. The admin fixture path
 * already ensures the platform is initialised + the admin exists,
 * but it doesn't seed downstream users.
 */
async function ensureDeveloperUser() {
  const ctx = await playwrightRequest.newContext();
  try {
    // 1. Log in as admin to get a session cookie.
    const login = await ctx.post(`${CONSOLE}/api/auth/login`, {
      data: { email: ADMIN_EMAIL, password: ADMIN_PASSWORD },
    });
    if (!login.ok()) {
      throw new Error(
        `admin login failed: ${login.status()}; cannot provision dev user`,
      );
    }

    // 2. See if the user already exists (idempotency — a previous
    //    run might have created them and tests share the dev DB).
    const list = await ctx.get(
      `${CONSOLE}/api/admin/users?per_page=200&page=1`,
    );
    if (list.ok()) {
      const body = (await list.json()) as { data?: Array<{ email: string }> };
      const exists = (body.data ?? []).some((u) => u.email === DEV_EMAIL);
      if (exists) return;
    }

    // 3. Resolve the `developer` role id so we can attach it.
    const roles = await ctx.get(`${CONSOLE}/api/admin/roles`);
    const rolesBody = (await roles.json()) as {
      items?: Array<{ id: string; name: string }>;
    };
    const developerRole = (rolesBody.items ?? []).find(
      (r) => r.name === 'developer',
    );
    if (!developerRole) {
      throw new Error(
        'developer role not found — migration seed missing?',
      );
    }

    // 4. Create the user with the developer role at global scope.
    const create = await ctx.post(`${CONSOLE}/api/admin/users`, {
      data: {
        email: DEV_EMAIL,
        display_name: 'E2E Developer (route guard)',
        password: DEV_PASSWORD,
        role_assignments: [
          { role_id: developerRole.id, scope_kind: 'global', scope_id: null },
        ],
      },
    });
    if (!create.ok() && create.status() !== 409) {
      throw new Error(
        `dev user create failed: ${create.status()} ${await create.text()}`,
      );
    }
  } finally {
    await ctx.dispose();
  }
}

test.describe('route guards', () => {
  test.beforeAll(async () => {
    await ensureDeveloperUser();
  });

  // The fixture-managed admin login is unrelated here; we sign in
  // manually as the developer user.
  test('developer typing /admin/users lands on the Forbidden page', async ({
    page,
  }) => {
    await page.goto('/login');
    await page.getByLabel(/Email/i).fill(DEV_EMAIL);
    await page.getByLabel(/Password/i).fill(DEV_PASSWORD);
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await page.waitForURL((u) => !u.pathname.startsWith('/login'), {
      timeout: 15_000,
    });

    // Direct-URL navigation to an admin route the developer lacks
    // `users:read` for.
    await page.goto('/admin/users');

    // The Forbidden page renders an aria-friendly heading carrying
    // the i18n string (en + zh both alias here via the same key).
    await expect(
      page.getByRole('heading', {
        name: /You don't have access to this page|无权访问此页面/i,
      }),
    ).toBeVisible({ timeout: 10_000 });

    // The required permission name is surfaced for support tickets;
    // a regression that drops this hint shows up here.
    await expect(page.getByText(/users:read/)).toBeVisible();

    // Sanity: the URL stays /admin/users (we render-in-place, not
    // redirect, so refresh / sharing the URL keeps the user where
    // they are).
    expect(page.url()).toContain('/admin/users');

    // No half-rendered admin table either: the "Add user" CTA must
    // NOT exist on the page.
    await expect(
      page.getByRole('button', { name: /Add user|添加用户/i }),
    ).toHaveCount(0);
  });

  test('developer sees an empty admin nav group in the sidebar', async ({
    page,
  }) => {
    await page.goto('/login');
    await page.getByLabel(/Email/i).fill(DEV_EMAIL);
    await page.getByLabel(/Password/i).fill(DEV_PASSWORD);
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await page.waitForURL((u) => !u.pathname.startsWith('/login'), {
      timeout: 15_000,
    });

    // The "Admin" group label and every entry under it disappears
    // for a developer — the sidebar's `permissionForRoute` filter
    // drops items the user can't reach, then drops empty groups.
    // Scope to the sidebar to avoid matching unrelated "Users"
    // strings elsewhere on the page.
    await page.waitForSelector('[data-slot="sidebar"]', { timeout: 10_000 });
    const sidebar = page.locator('[data-slot="sidebar"]');
    await expect(
      sidebar.getByRole('button', { name: /^Users$|^用户$/i }),
    ).toHaveCount(0);
    await expect(
      sidebar.getByRole('button', { name: /^Roles$|^角色$/i }),
    ).toHaveCount(0);
  });
});
