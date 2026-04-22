import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { SettingsPage } from './settings'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPatch: vi.fn(),
}))

// SettingsPage now reads / writes the active tab via TanStack Router's
// useSearch + useNavigate. These hooks need a RouterProvider context to
// run; in unit tests there isn't one. Stub the hooks so the component
// behaves as if the URL has no `?tab=` and navigation is a no-op.
vi.mock('@tanstack/react-router', () => ({
  useSearch: () => ({} as Record<string, unknown>),
  useNavigate: () => vi.fn(),
}))

import { api, apiPatch } from '@/lib/api'

const mockApi = vi.mocked(api)
const mockApiPatch = vi.mocked(apiPatch)

const systemInfo = {
  version: '1.2.3',
  uptime: '3h 15m',
  rust_version: '1.78.0',
  server_host: '0.0.0.0',
  gateway_port: 8080,
  console_port: 3000,
}

const settingsData: Record<string, Array<{ key: string; value: unknown; category: string; description: string; updated_at: string }>> = {
  setup: [
    { key: 'setup.site_name', value: 'TestThinkWatch', category: 'setup', description: '', updated_at: '' },
  ],
  general: [],
  auth: [],
  gateway: [],
  security: [],
  budget: [],
  api_keys: [],
  data: [],
}

beforeEach(() => {
  vi.clearAllMocks()

  mockApi.mockImplementation((path: string) => {
    if (path === '/api/admin/settings/system') return Promise.resolve(systemInfo)
    if (path === '/api/admin/settings/oidc') return Promise.resolve({ issuer_url: '', client_id: '', enabled: false })
    if (path === '/api/admin/settings/audit') return Promise.resolve({ clickhouse_url: '', clickhouse_db: '' })
    if (path === '/api/admin/settings') return Promise.resolve(settingsData)
    if (path === '/api/health') return Promise.resolve({ postgres: true, redis: true, clickhouse: true })
    if (path === '/api/admin/roles') return Promise.resolve({ items: [] })
    return Promise.resolve({})
  })
})

describe('SettingsPage', () => {
  it('renders settings tabs', async () => {
    render(<SettingsPage />)

    await waitFor(() => {
      expect(screen.queryByText('Loading...')).not.toBeInTheDocument()
    })

    expect(screen.getByRole('tab', { name: /general/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /authentication/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /gateway/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /security/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /api key policies/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /audit/i })).toBeInTheDocument()
    // The old global "Budget" tab is gone — budget caps moved into the
    // per-rule limits flow.
  })

  it('loads and displays system info', async () => {
    render(<SettingsPage />)

    await waitFor(() => {
      expect(screen.getByText('1.2.3')).toBeInTheDocument()
    })

    expect(screen.getByText('3h 15m')).toBeInTheDocument()
    expect(screen.getByText('1.78.0')).toBeInTheDocument()
  })

  // The old "click Save" workflow is gone — every field on the page now
  // autosaves on change via useFieldAutosave (default 600ms debounce).
  // Edit the site-name input and wait for the debounced apiPatch.
  it('autosaves a setting after the field is edited', async () => {
    mockApiPatch.mockResolvedValue({ ok: true })

    const user = userEvent.setup()
    render(<SettingsPage />)

    // Wait for the page to finish loading + seed the autosave snapshot.
    const siteInput = await screen.findByDisplayValue('TestThinkWatch')

    await user.clear(siteInput)
    await user.type(siteInput, 'NewName')

    await waitFor(
      () => {
        expect(mockApiPatch).toHaveBeenCalledWith(
          '/api/admin/settings',
          expect.objectContaining({
            settings: expect.objectContaining({ 'setup.site_name': 'NewName' }),
          }),
        )
      },
      { timeout: 2000 },
    )
  })
})
