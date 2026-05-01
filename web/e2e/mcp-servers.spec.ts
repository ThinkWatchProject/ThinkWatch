import { expect, test } from './fixtures';

/**
 * MCP servers — registration wizard, edit-mode parity, and list-row
 * auth-mode badges.
 *
 * Backend interactions are mocked via `page.route` so the tests don't
 * depend on a real upstream MCP server. The real backend still serves
 * login + cookie-based auth via the `adminPage` fixture; these mocks
 * only intercept `/api/mcp/servers*` calls inside the page so we can
 * exercise UI behaviour deterministically.
 */

const SAMPLE_OAUTH_SERVER = {
  id: 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa',
  name: 'github-mcp',
  namespace_prefix: 'github',
  description: 'GitHub OAuth',
  endpoint_url: 'https://api.example.com/mcp',
  transport_type: 'streamable_http',
  oauth_issuer: 'https://github.com',
  oauth_authorization_endpoint: null,
  oauth_token_endpoint: null,
  oauth_revocation_endpoint: null,
  oauth_userinfo_endpoint: 'https://api.github.com/user',
  oauth_client_id: 'iv-123',
  oauth_scopes: ['repo', 'read:user'],
  allow_static_token: false,
  static_token_help_url: null,
  status: 'connected',
  last_health_check: null,
  tools_count: 5,
  call_count: 0,
  config_json: {},
  created_at: '2026-01-01T00:00:00Z',
};

const SAMPLE_PUBLIC_SERVER = {
  ...SAMPLE_OAUTH_SERVER,
  id: 'bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb',
  name: 'public-mcp',
  namespace_prefix: 'public_mcp',
  description: 'No-auth MCP',
  endpoint_url: 'https://public.example.com/mcp',
  oauth_issuer: null,
  oauth_userinfo_endpoint: null,
  oauth_client_id: null,
  oauth_scopes: [],
};

const SAMPLE_STATIC_SERVER = {
  ...SAMPLE_PUBLIC_SERVER,
  id: 'cccccccc-cccc-cccc-cccc-cccccccccccc',
  name: 'static-mcp',
  namespace_prefix: 'static_mcp',
  allow_static_token: true,
  static_token_help_url: 'https://example.com/get-token',
};

const SAMPLE_HEADERS_SERVER = {
  ...SAMPLE_PUBLIC_SERVER,
  id: 'dddddddd-dddd-dddd-dddd-dddddddddddd',
  name: 'headers-mcp',
  namespace_prefix: 'headers_mcp',
  config_json: { custom_headers: { 'X-API-Key': 'fixed-secret' } },
};

test.describe('MCP servers — registration wizard', () => {
  test('public mode: 4 modes shown → fill basics → step 3 success → save', async ({
    adminPage: page,
  }) => {
    let createdPayload: unknown = null;
    await page.route('**/api/mcp/servers', async (route) => {
      if (route.request().method() === 'GET') {
        return route.fulfill({ json: [] });
      }
      if (route.request().method() === 'POST') {
        createdPayload = route.request().postDataJSON();
        return route.fulfill({ json: { ...SAMPLE_PUBLIC_SERVER, name: 'wizard-test' } });
      }
      return route.continue();
    });
    await page.route('**/api/mcp/servers/test', (route) =>
      route.fulfill({
        json: {
          success: true,
          requires_auth: false,
          message: 'Connected — 2 tools available',
          latency_ms: 42,
          tools_count: 2,
          tools: [
            { name: 'echo', description: 'Echo input' },
            { name: 'reverse', description: 'Reverse a string' },
          ],
        },
      }),
    );

    await page.goto('/mcp/servers');
    await expect(page.getByRole('heading', { name: /MCP Servers/i })).toBeVisible();

    await page.getByRole('button', { name: /Register Server/i }).click();

    // Step 1 — all four modes plus the store link.
    const dialog = page.getByRole('dialog');
    await expect(dialog.getByRole('button', { name: /OAuth login/i })).toBeVisible();
    await expect(dialog.getByRole('button', { name: /Static token/i })).toBeVisible();
    await expect(dialog.getByRole('button', { name: /Custom request headers/i })).toBeVisible();
    await expect(dialog.getByRole('button', { name: /Public \(no auth\)/i })).toBeVisible();

    await dialog.getByRole('button', { name: /Public \(no auth\)/i }).click();

    // Step 2 — public mode shows only the basics, no OAuth section.
    await expect(dialog.getByLabel('Name', { exact: true })).toBeVisible();
    await expect(dialog.getByLabel(/Endpoint URL/i)).toBeVisible();
    await expect(dialog.getByText(/Issuer/i)).toHaveCount(0);

    await dialog.getByLabel('Name', { exact: true }).fill('wizard-test');
    await dialog.getByLabel(/Endpoint URL/i).fill('https://test.example.com/mcp');
    await dialog.getByRole('button', { name: /Next: test connection/i }).click();

    // Step 3 — auto-tests, success panel shows tools_count + latency.
    await expect(dialog.getByText(/tools discovered/i)).toBeVisible();
    await expect(dialog.getByText('42ms')).toBeVisible();
    // The discovered tool list renders names inside <code>; match that
    // specific element to avoid colliding with the "Echo input"
    // description text that shares the substring.
    await expect(dialog.locator('code', { hasText: /^echo$/ })).toBeVisible();

    await dialog.getByRole('button', { name: /^Register Server$/i }).click();

    // Dialog closes on save; the POST payload reflects the fields we
    // entered (no OAuth keys for public mode).
    await expect(dialog).toBeHidden();
    expect(createdPayload).toMatchObject({
      name: 'wizard-test',
      endpoint_url: 'https://test.example.com/mcp',
      allow_static_token: false,
    });
    expect((createdPayload as Record<string, unknown>).oauth_issuer ?? null).toBeNull();
  });

  test('OAuth mode: validation blocks advance without issuer', async ({ adminPage: page }) => {
    await page.route('**/api/mcp/servers', (route) =>
      route.request().method() === 'GET' ? route.fulfill({ json: [] }) : route.continue(),
    );

    await page.goto('/mcp/servers');
    await page.getByRole('button', { name: /Register Server/i }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByRole('button', { name: /OAuth login/i }).click();

    await dialog.getByLabel('Name', { exact: true }).fill('oauth-test');
    await dialog.getByLabel(/Endpoint URL/i).fill('https://oauth.example.com/mcp');
    // Click advance without filling issuer.
    await dialog.getByRole('button', { name: /Next: test connection/i }).click();

    await expect(dialog.getByRole('alert')).toContainText(/issuer is required/i);
    // Still on Step 2 — the issuer field is the proof.
    await expect(dialog.getByLabel(/^Issuer$/)).toBeVisible();
  });

  test('step indicator: clicking step 1 from step 2 jumps back', async ({ adminPage: page }) => {
    await page.route('**/api/mcp/servers', (route) =>
      route.request().method() === 'GET' ? route.fulfill({ json: [] }) : route.continue(),
    );

    await page.goto('/mcp/servers');
    await page.getByRole('button', { name: /Register Server/i }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByRole('button', { name: /Public \(no auth\)/i }).click();

    // We're on Step 2 — step 1's number circle should be a button now.
    const step1Button = dialog.getByRole('button', { name: /Auth mode/i });
    await expect(step1Button).toBeVisible();
    await step1Button.click();

    // Back at Step 1 — the four mode cards are visible again.
    await expect(dialog.getByRole('button', { name: /OAuth login/i })).toBeVisible();
  });
});

test.describe('MCP servers — edit dialog mode parity', () => {
  test('OAuth server: badge shows OAuth, OAuth fields rendered', async ({ adminPage: page }) => {
    await page.route('**/api/mcp/servers', (route) =>
      route.request().method() === 'GET'
        ? route.fulfill({ json: [SAMPLE_OAUTH_SERVER] })
        : route.continue(),
    );

    await page.goto('/mcp/servers');
    await expect(page.getByRole('cell', { name: 'github-mcp' })).toBeVisible();

    // Edit is an icon-only ghost button — match by its title attribute.
    await page.locator('button[title="Edit"]').first().click();
    const dialog = page.getByRole('dialog');

    // Auth-mode badge in the header reflects derived mode.
    await expect(dialog.getByText(/OAuth login/i)).toBeVisible();
    // OAuth-specific fields are present.
    await expect(dialog.getByLabel(/^Issuer$/)).toHaveValue('https://github.com');
    await expect(dialog.getByLabel(/^Client ID$/)).toHaveValue('iv-123');
    // The "delete + recreate to change" hint is shown.
    await expect(dialog.getByText(/Auth mode is fixed/i)).toBeVisible();
  });

  test('public server: no OAuth fields', async ({ adminPage: page }) => {
    await page.route('**/api/mcp/servers', (route) =>
      route.request().method() === 'GET'
        ? route.fulfill({ json: [SAMPLE_PUBLIC_SERVER] })
        : route.continue(),
    );

    await page.goto('/mcp/servers');
    await page.locator('button[title="Edit"]').first().click();
    const dialog = page.getByRole('dialog');

    await expect(dialog.getByText(/Public \(no auth\)/i)).toBeVisible();
    // OAuth fields should NOT render for a public server.
    await expect(dialog.getByLabel(/^Issuer$/)).toHaveCount(0);
    await expect(dialog.getByLabel(/^Client ID$/)).toHaveCount(0);
  });
});

test.describe('MCP servers — list auth-mode column', () => {
  test('compact badge tooltip names the auth mode', async ({ adminPage: page }) => {
    await page.route('**/api/mcp/servers', (route) =>
      route.request().method() === 'GET'
        ? route.fulfill({
            json: [
              SAMPLE_OAUTH_SERVER,
              SAMPLE_STATIC_SERVER,
              SAMPLE_HEADERS_SERVER,
              SAMPLE_PUBLIC_SERVER,
            ],
          })
        : route.continue(),
    );

    await page.goto('/mcp/servers');
    await expect(page.getByRole('cell', { name: 'github-mcp' })).toBeVisible();

    // Each row has a compact badge — match by aria-label which carries
    // the mode title regardless of locale-specific tooltip rendering.
    await expect(page.getByLabel('OAuth login').first()).toBeVisible();
    await expect(page.getByLabel('Static token (PAT / API key)')).toBeVisible();
    await expect(page.getByLabel('Custom request headers')).toBeVisible();
    await expect(page.getByLabel('Public (no auth)')).toBeVisible();
  });
});
