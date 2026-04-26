import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
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

import { api } from '@/lib/api'

const mockApi = vi.mocked(api)

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
  lineage_id: '00000000-0000-0000-0000-000000000001',
  cost_center: null,
  ...overrides,
})

// Different endpoints have different response shapes — route by URL so
// /api/keys returns the paginated key list while /api/keys/cost-centers
// and /api/keys/policy-scope get their own appropriate shapes.
function mockKeysFetch(keys: ReturnType<typeof makeKey>[]) {
  mockApi.mockImplementation(async (url: string) => {
    if (url.startsWith('/api/keys/cost-centers')) return [] as string[]
    if (url.startsWith('/api/keys/policy-scope'))
      return { allowed_models: null, allowed_mcp_tools: null }
    return { data: keys, total: keys.length, page: 1, page_size: 20 }
  })
}

beforeEach(() => {
  vi.clearAllMocks()
})

describe('ApiKeysPage', () => {
  it('renders API keys table', async () => {
    mockKeysFetch([makeKey()])

    render(<ApiKeysPage />)

    await waitFor(() => {
      expect(screen.getByText('test-key')).toBeInTheDocument()
    })

    // Verify table headers exist
    expect(screen.getByText('Name')).toBeInTheDocument()
    expect(screen.getByText('Key Prefix')).toBeInTheDocument()
    expect(screen.getByText('Expires')).toBeInTheDocument()
    expect(screen.getByText('Status')).toBeInTheDocument()
  })

  it('shows status badges for active and expired keys', async () => {
    mockKeysFetch([
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

    mockKeysFetch([makeKey({ id: 'key-1', name: 'expiring-key', expires_at: threeDaysFromNow })])

    render(<ApiKeysPage />)

    await waitFor(() => {
      expect(screen.getByText('expiring-key')).toBeInTheDocument()
    })

    // The ExpiryCell shows a badge with days remaining (e.g. "3d") for keys expiring within 7 days
    expect(screen.getByText('3d')).toBeInTheDocument()
  })

  it('create key dialog opens on button click', async () => {
    mockKeysFetch([])

    const user = userEvent.setup()
    render(<ApiKeysPage />)

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
})
