import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { ApiKeysPage } from './api-keys'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPost: vi.fn(),
  apiPatch: vi.fn(),
  apiDelete: vi.fn(),
}))

import { api } from '@/lib/api'

const mockApi = vi.mocked(api)

const makeKey = (overrides: Record<string, unknown> = {}) => ({
  id: 'key-1',
  name: 'test-key',
  key_prefix: 'tw_test_',
  team_name: 'team-a',
  user_id: 'user-1',
  team_id: 'team-1',
  allowed_models: null,
  rate_limit_rpm: 60,
  rate_limit_tpm: null,
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
  ...overrides,
})

beforeEach(() => {
  vi.clearAllMocks()
})

describe('ApiKeysPage', () => {
  it('renders API keys table', async () => {
    mockApi.mockResolvedValue([makeKey()])

    render(<ApiKeysPage />)

    await waitFor(() => {
      expect(screen.getByText('test-key')).toBeInTheDocument()
    })

    // Verify table headers exist
    expect(screen.getByText('Name')).toBeInTheDocument()
    expect(screen.getByText('Key Prefix')).toBeInTheDocument()
    expect(screen.getByText('Team')).toBeInTheDocument()
    expect(screen.getByText('Rate Limit')).toBeInTheDocument()
    expect(screen.getByText('Expires')).toBeInTheDocument()
    expect(screen.getByText('Status')).toBeInTheDocument()
  })

  it('shows status badges for active and expired keys', async () => {
    mockApi.mockResolvedValue([
      makeKey({ id: 'key-1', name: 'active-key', disabled_reason: null, is_active: true }),
      makeKey({ id: 'key-2', name: 'expired-key', disabled_reason: 'expired', is_active: false }),
      makeKey({ id: 'key-3', name: 'revoked-key', disabled_reason: 'revoked', is_active: false }),
    ])

    render(<ApiKeysPage />)

    await waitFor(() => {
      expect(screen.getByText('active-key')).toBeInTheDocument()
    })

    expect(screen.getByText('Active')).toBeInTheDocument()
    expect(screen.getByText('Expired')).toBeInTheDocument()
    expect(screen.getByText('Revoked')).toBeInTheDocument()
  })

  it('shows expiry warning for keys expiring soon', async () => {
    const threeDaysFromNow = new Date(Date.now() + 3 * 24 * 60 * 60 * 1000).toISOString()

    mockApi.mockResolvedValue([
      makeKey({ id: 'key-1', name: 'expiring-key', expires_at: threeDaysFromNow }),
    ])

    render(<ApiKeysPage />)

    await waitFor(() => {
      expect(screen.getByText('expiring-key')).toBeInTheDocument()
    })

    // The ExpiryCell shows a badge with days remaining (e.g. "3d") for keys expiring within 7 days
    expect(screen.getByText('3d')).toBeInTheDocument()
  })

  it('create key dialog opens on button click', async () => {
    mockApi.mockResolvedValue([])

    const user = userEvent.setup()
    render(<ApiKeysPage />)

    // Wait for loading to finish
    await waitFor(() => {
      expect(screen.queryByText('Loading keys...')).not.toBeInTheDocument()
    })

    // Click the create button
    await user.click(screen.getByRole('button', { name: /create api key/i }))

    // Verify dialog content appears
    await waitFor(() => {
      expect(screen.getByText('Generate a new API key for the gateway.')).toBeInTheDocument()
    })
  })
})
