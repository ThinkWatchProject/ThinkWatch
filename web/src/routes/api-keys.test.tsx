import { describe, it, expect, vi, beforeEach } from 'vitest'
import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { renderWithQueryClient } from '@/test/render'
import { ApiKeysPage } from './api-keys'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPost: vi.fn(),
  apiPatch: vi.fn(),
  apiDelete: vi.fn(),
  // ApiKeysPage gates the "Create" button on this. Stub to true so
  // the button renders in tests; individual cases that need denial
  // can `vi.mocked(hasPermission).mockReturnValueOnce(false)`.
  hasPermission: vi.fn(() => true),
}))

// `api-keys.tsx` loops over /api/admin/models and /api/mcp/tools via
// fetchAllPaginated to populate the scope pickers. The page renders
// without them, so a no-op stub keeps the mount path clean.
vi.mock('@/lib/paginated-fetch', () => ({
  fetchAllPaginated: vi.fn().mockResolvedValue([]),
}))

import { api, apiPost, hasPermission } from '@/lib/api'

const mockApi = vi.mocked(api)
const mockApiPost = vi.mocked(apiPost)
const mockHasPermission = vi.mocked(hasPermission)

const makeKey = (overrides: Record<string, unknown> = {}) => ({
  id: 'key-1',
  name: 'test-key',
  key_prefix: 'tw_test_',
  user_id: 'user-1',
  surfaces: ['ai_gateway'],
  allowed_models: null,
  allowed_mcp_tools: null,
  expires_at: null,
  is_active: true,
  last_used_at: null,
  created_at: '2025-01-01T00:00:00Z',
  deleted_at: null,
  rotation_period_days: null,
  rotated_from_id: null,
  grace_period_ends_at: null,
  inactivity_timeout_days: null,
  disabled_reason: null,
  last_rotation_at: null,
  cost_center: null,
  ...overrides,
})

// Different endpoints have different response shapes — route by URL so
// /api/keys returns the paginated key list while /api/keys/cost-centers
// and /api/keys/policy-scope get their own appropriate shapes.
function mockKeysFetch(
  keys: ReturnType<typeof makeKey>[],
  limits: (url: string) => unknown = () => ({ items: [], rules: [], caps: [] }),
) {
  mockApi.mockImplementation(async (url: string) => {
    if (url.startsWith('/api/keys/cost-centers')) return [] as string[]
    if (url.startsWith('/api/keys/policy-scope'))
      return { allowed_models: null, allowed_mcp_tools: null }
    if (url.startsWith('/api/admin/limits/')) return limits(url)
    if (url === '/api/gateway/info')
      return {
        public_protocol: 'https',
        public_host: 'gateway.example.com',
        public_port: 443,
        gateway_port: 3000,
      }
    return { data: keys, total: keys.length, page: 1, page_size: 20 }
  })
}

beforeEach(() => {
  vi.clearAllMocks()
  mockHasPermission.mockImplementation(() => true)
})

describe('ApiKeysPage', () => {
  it('renders API keys table', async () => {
    mockKeysFetch([makeKey()])

    renderWithQueryClient(<ApiKeysPage />)

    await waitFor(() => {
      expect(screen.getByText('test-key')).toBeInTheDocument()
    })

    // Verify table headers exist
    expect(screen.getByText('Name')).toBeInTheDocument()
    expect(screen.getByText('Key Prefix')).toBeInTheDocument()
    expect(screen.getByText('Expires')).toBeInTheDocument()
    expect(screen.getByText('Status')).toBeInTheDocument()
  })

  it('shows the active status badge on the active tab', async () => {
    // Default tab is now "Active" — filters to `is_active &&
    // !disabled_reason`. An expired-disabled key in the same fixture
    // is intentionally hidden from this tab (it lives under "Inactive")
    // so the assertion only covers the active row that should be
    // visible. "Active" text appears both as the tab trigger AND the
    // status badge, so use the row-scoped name lookup to disambiguate.
    mockKeysFetch([
      makeKey({ id: 'key-1', name: 'active-key', disabled_reason: null, is_active: true }),
      makeKey({ id: 'key-2', name: 'expired-key', disabled_reason: 'expired', is_active: false }),
    ])

    renderWithQueryClient(<ApiKeysPage />)

    await waitFor(() => {
      expect(screen.getByText('active-key')).toBeInTheDocument()
    })

    // The expired key is filtered out of the default Active tab.
    expect(screen.queryByText('expired-key')).not.toBeInTheDocument()
    // "Active" matches the tab AND the badge; assert at least one
    // (the row badge) is rendered alongside the active-key row.
    expect(screen.getAllByText('Active').length).toBeGreaterThanOrEqual(1)
  })

  it('shows expiry warning for keys expiring soon', async () => {
    const threeDaysFromNow = new Date(Date.now() + 3 * 24 * 60 * 60 * 1000).toISOString()

    mockKeysFetch([makeKey({ id: 'key-1', name: 'expiring-key', expires_at: threeDaysFromNow })])

    renderWithQueryClient(<ApiKeysPage />)

    await waitFor(() => {
      expect(screen.getByText('expiring-key')).toBeInTheDocument()
    })

    // The ExpiryCell shows a badge with days remaining (e.g. "3d") for keys expiring within 7 days
    expect(screen.getByText('3d')).toBeInTheDocument()
  })

  it('create key dialog opens on button click', async () => {
    mockKeysFetch([])

    const user = userEvent.setup()
    renderWithQueryClient(<ApiKeysPage />)

    // Wait for loading to finish
    await waitFor(() => {
      expect(screen.queryByText('Loading keys...')).not.toBeInTheDocument()
    })

    // Both the page header and the empty-state CTA carry the same
    // "Create API Key" label — either opens the same dialog, so click
    // the first match.
    await user.click(screen.getAllByRole('button', { name: /create api key/i })[0])

    // Verify dialog content appears
    await waitFor(() => {
      expect(screen.getByText('Generate a new API key for the gateway.')).toBeInTheDocument()
    })
  })

  it('offers a new key to ThinkWatch Lite beside Copy, without leaving the page', async () => {
    mockKeysFetch([])
    mockApiPost.mockResolvedValue({ id: 'new', key: 'tw-new-key' })
    const user = userEvent.setup()
    renderWithQueryClient(<ApiKeysPage />)

    await user.click((await screen.findAllByRole('button', { name: /create api key/i }))[0])
    await user.type(screen.getByLabelText('Name'), 'laptop')
    await user.click(screen.getByRole('button', { name: 'Create Key' }))

    const add = await screen.findByRole('button', { name: 'Add to ThinkWatch Lite' })
    await waitFor(() => expect(add).toBeEnabled())
    expect(screen.getByRole('button', { name: 'Copy to Clipboard' })).toBeInTheDocument()
    // The button is no link the page itself would follow.
    expect(screen.queryByRole('link', { name: 'Add to ThinkWatch Lite' })).not.toBeInTheDocument()

    const page = window.location.href
    await user.click(add)
    // The link goes to a hidden iframe; the page and its key stay.
    expect(window.location.href).toBe(page)
    const frame = document.querySelector('iframe')
    expect(frame).toHaveAttribute(
      'src',
      'thinkwatch://import?service=thinkwatch&name=thinkwatch' +
        '&url=https%3A%2F%2Fgateway.example.com&key=tw-new-key',
    )
    expect(frame).not.toBeVisible()
    expect(screen.getByText('tw-new-key')).toBeInTheDocument()
    expect(screen.getByText('Nothing happened?')).toBeInTheDocument()
    frame?.remove()
  })

  it('names why a key cannot be added to Lite when its owner may not use the gateway', async () => {
    mockKeysFetch([])
    mockHasPermission.mockImplementation((perm: string) => perm !== 'ai_gateway:use')
    mockApiPost.mockResolvedValue({ id: 'new', key: 'tw-new-key' })
    const user = userEvent.setup()
    renderWithQueryClient(<ApiKeysPage />)

    await user.click((await screen.findAllByRole('button', { name: /create api key/i }))[0])
    await user.type(screen.getByLabelText('Name'), 'laptop')
    await user.click(screen.getByRole('button', { name: 'Create Key' }))

    const add = await screen.findByRole('button', { name: 'Add to ThinkWatch Lite' })
    expect(add).toBeDisabled()
    expect(add).toHaveAccessibleDescription(
      'No role of this account grants use of the AI gateway (ai_gateway:use), so its API keys cannot be used from ThinkWatch Lite.',
    )
  })

  it('names why a key without the AI gateway cannot be added to Lite', async () => {
    mockKeysFetch([])
    mockApiPost.mockResolvedValue({ id: 'new', key: 'tw-new-key' })
    const user = userEvent.setup()
    renderWithQueryClient(<ApiKeysPage />)

    await user.click((await screen.findAllByRole('button', { name: /create api key/i }))[0])
    await user.type(screen.getByLabelText('Name'), 'tools only')
    await user.click(screen.getByRole('checkbox', { name: /AI gateway/ }))
    await user.click(screen.getByRole('button', { name: 'Create Key' }))

    const add = await screen.findByRole('button', { name: 'Add to ThinkWatch Lite' })
    expect(add).toBeDisabled()
    expect(add).toHaveAccessibleDescription(
      'This API key cannot call the AI gateway, so it cannot be added to ThinkWatch Lite.',
    )
  })

  it("edits the key's own limits in a tab of the edit dialog", async () => {
    mockKeysFetch([makeKey()], (url) =>
      url.endsWith('/usage')
        ? { rules: [{ rule_id: 'r1', current: 3, limit: 10 }], caps: [] }
        : url.endsWith('/rules')
          ? {
              items: [
                {
                  id: 'r1',
                  surface: 'ai_gateway',
                  metric: 'requests',
                  window_secs: 60,
                  max_count: 10,
                  enabled: true,
                },
              ],
            }
          : { items: [] },
    )
    const user = userEvent.setup()
    renderWithQueryClient(<ApiKeysPage />)

    await user.click(await screen.findByRole('button', { name: 'Edit' }))
    await user.click(await screen.findByRole('tab', { name: 'Limits' }))

    expect(await screen.findByText('30%')).toBeInTheDocument()
    expect(screen.getByText('7 remaining')).toBeInTheDocument()
    expect(mockApi).toHaveBeenCalledWith(
      '/api/admin/limits/api_key/key-1/rules',
      expect.anything(),
    )
  })

  it('shows no limits tab without rate_limits:read', async () => {
    mockHasPermission.mockImplementation((perm) => perm !== 'rate_limits:read')
    mockKeysFetch([makeKey()])
    const user = userEvent.setup()
    renderWithQueryClient(<ApiKeysPage />)

    await user.click(await screen.findByRole('button', { name: 'Edit' }))
    expect(await screen.findByText('Edit Key')).toBeInTheDocument()
    expect(screen.queryByRole('tab', { name: 'Limits' })).not.toBeInTheDocument()
    expect(mockApi).not.toHaveBeenCalledWith(
      expect.stringContaining('/api/admin/limits/'),
      expect.anything(),
    )
  })

  it("shows no limits tab when the key is outside the caller's scope", async () => {
    mockKeysFetch([makeKey()], () => {
      throw Object.assign(new Error('Forbidden'), { status: 403 })
    })
    const user = userEvent.setup()
    renderWithQueryClient(<ApiKeysPage />)

    await user.click(await screen.findByRole('button', { name: 'Edit' }))
    await waitFor(() => {
      expect(mockApi).toHaveBeenCalledWith(
        '/api/admin/limits/api_key/key-1/rules',
        expect.anything(),
      )
    })
    await waitFor(() => {
      expect(screen.queryByRole('tab', { name: 'Limits' })).not.toBeInTheDocument()
    })
    // The settings form is there as before.
    expect(within(screen.getByRole('dialog')).getByText('Gateway access')).toBeInTheDocument()
  })

  async function rotateAndOpenLite() {
    mockApiPost.mockResolvedValue({
      id: 'key-2',
      key: 'tw-rotated-key',
      name: 'test-key',
      key_prefix: 'tw_rota',
    })
    const user = userEvent.setup()
    renderWithQueryClient(<ApiKeysPage />)
    await user.click(await screen.findByRole('button', { name: 'Rotate' }))
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Rotate' }))
    const add = await screen.findByRole('button', { name: 'Add to ThinkWatch Lite' })
    await waitFor(() => expect(add).toBeEnabled())
    return { user, add }
  }

  const LINK =
    'thinkwatch://import?service=thinkwatch&name=thinkwatch' +
    '&url=https%3A%2F%2Fgateway.example.com&key=tw-rotated-key'

  it("carries the rotated key's request limits to ThinkWatch Lite", async () => {
    mockKeysFetch([makeKey()], (url) =>
      url.endsWith('/rules')
        ? {
            items: [
              { id: 'r1', surface: 'ai_gateway', metric: 'requests', window_secs: 60, max_count: 30, enabled: true },
              { id: 'r2', surface: 'ai_gateway', metric: 'requests', window_secs: 18000, max_count: 600, enabled: true },
              // Left out: weighted tokens, a sliding week, an MCP rule.
              { id: 'r3', surface: 'ai_gateway', metric: 'tokens', window_secs: 60, max_count: 9000, enabled: true },
              { id: 'r4', surface: 'ai_gateway', metric: 'requests', window_secs: 604800, max_count: 9000, enabled: true },
              { id: 'r5', surface: 'mcp_gateway', metric: 'requests', window_secs: 60, max_count: 5, enabled: true },
            ],
          }
        : { items: [] },
    )
    const { user, add } = await rotateAndOpenLite()

    await waitFor(() =>
      expect(add).toHaveAccessibleDescription(
        "The link includes this key's request-count limits and opens in ThinkWatch Lite 2026.10.26 or later.",
      ),
    )
    expect(mockApi).toHaveBeenCalledWith('/api/admin/limits/api_key/key-2/rules', expect.anything())
    await user.click(add)
    const frame = document.querySelector('iframe')
    expect(frame).toHaveAttribute(
      'src',
      LINK + '&limits=minute%7Crequests%7C30%3Bhour%7Crequests%7C600%7C5',
    )
    expect(screen.getByRole('link', { name: 'Install ThinkWatch Lite (2026.10.26 or later) first' }))
      .toBeInTheDocument()
    frame?.remove()
  })

  it('builds the link without limits when none of the rotated key maps', async () => {
    mockKeysFetch([makeKey()])
    const { user, add } = await rotateAndOpenLite()

    await waitFor(() =>
      expect(mockApi).toHaveBeenCalledWith('/api/admin/limits/api_key/key-2/rules', expect.anything()),
    )
    expect(add).not.toHaveAccessibleDescription()
    await user.click(add)
    const frame = document.querySelector('iframe')
    expect(frame).toHaveAttribute('src', LINK)
    expect(screen.getByRole('link', { name: 'Install ThinkWatch Lite (2026.10.16 or later) first' }))
      .toBeInTheDocument()
    frame?.remove()
  })

  it("builds the link without limits when the key's limits cannot be read", async () => {
    mockKeysFetch([makeKey()], () => {
      throw Object.assign(new Error('Forbidden'), { status: 403 })
    })
    const { user, add } = await rotateAndOpenLite()

    await waitFor(() =>
      expect(mockApi).toHaveBeenCalledWith('/api/admin/limits/api_key/key-2/rules', expect.anything()),
    )
    expect(add).toBeEnabled()
    expect(add).not.toHaveAccessibleDescription()
    await user.click(add)
    const frame = document.querySelector('iframe')
    expect(frame).toHaveAttribute('src', LINK)
    frame?.remove()
  })

  it('reads no limits for the link without rate_limits:read', async () => {
    mockHasPermission.mockImplementation((perm) => perm !== 'rate_limits:read')
    mockKeysFetch([makeKey()])
    const { user, add } = await rotateAndOpenLite()

    await user.click(add)
    const frame = document.querySelector('iframe')
    expect(frame).toHaveAttribute('src', LINK)
    expect(mockApi).not.toHaveBeenCalledWith(
      expect.stringContaining('/api/admin/limits/'),
      expect.anything(),
    )
    frame?.remove()
  })
})
