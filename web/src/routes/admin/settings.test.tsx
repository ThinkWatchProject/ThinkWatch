import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { SettingsPage } from './settings'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPatch: vi.fn(),
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
  general: [
    { key: 'site_name', value: 'TestBastion', category: 'general', description: '', updated_at: '' },
  ],
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
    expect(screen.getByRole('tab', { name: /budget/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /api key policies/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /data retention/i })).toBeInTheDocument()
  })

  it('loads and displays system info', async () => {
    render(<SettingsPage />)

    await waitFor(() => {
      expect(screen.getByText('1.2.3')).toBeInTheDocument()
    })

    expect(screen.getByText('3h 15m')).toBeInTheDocument()
    expect(screen.getByText('1.78.0')).toBeInTheDocument()
  })

  it('saves settings on button click', async () => {
    mockApiPatch.mockResolvedValue({ ok: true })

    const user = userEvent.setup()
    render(<SettingsPage />)

    // Wait for loading to complete
    await waitFor(() => {
      expect(screen.getByText('1.2.3')).toBeInTheDocument()
    })

    // Click save button
    await user.click(screen.getByRole('button', { name: /save/i }))

    expect(mockApiPatch).toHaveBeenCalledWith('/api/admin/settings', expect.objectContaining({
      settings: expect.objectContaining({
        site_name: 'TestBastion',
      }),
    }))

    // Verify success message
    await waitFor(() => {
      expect(screen.getByText('Settings saved successfully')).toBeInTheDocument()
    })
  })
})
