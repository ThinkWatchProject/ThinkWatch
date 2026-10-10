import { describe, it, expect, vi, beforeEach } from 'vitest'
import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { renderWithQueryClient } from '@/test/render'
import { KeyLimitsTab } from './key-limits-tab'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPost: vi.fn(),
  apiDelete: vi.fn(),
}))

import { api, apiDelete, apiPost } from '@/lib/api'

const mockApi = vi.mocked(api)
const mockPost = vi.mocked(apiPost)
const mockDelete = vi.mocked(apiDelete)

const BASE = '/api/admin/limits/api_key/key-1'

// One rule and one budget on the key, each with its counter.
function mockKeyLimits({
  rules = [
    {
      id: 'r1',
      subject_kind: 'api_key_lineage',
      subject_id: 'lin-1',
      surface: 'ai_gateway',
      metric: 'requests',
      window_secs: 60,
      max_count: 100,
      enabled: true,
    },
  ],
  caps = [
    {
      id: 'c1',
      subject_kind: 'api_key_lineage',
      subject_id: 'lin-1',
      period: 'daily',
      limit_tokens: 1_000_000,
      enabled: true,
    },
  ],
  usage = {
    rules: [{ rule_id: 'r1', current: 40, limit: 100 }],
    caps: [{ cap_id: 'c1', current: 250_000, limit: 1_000_000 }],
  },
}: {
  rules?: Record<string, unknown>[]
  caps?: Record<string, unknown>[]
  usage?: Record<string, unknown>
} = {}) {
  mockApi.mockImplementation(async (url: string) => {
    if (url === `${BASE}/rules`) return { items: rules }
    if (url === `${BASE}/budgets`) return { items: caps }
    if (url === `${BASE}/usage`) return usage
    throw new Error(`unexpected ${url}`)
  })
}

beforeEach(() => {
  vi.clearAllMocks()
})

describe('KeyLimitsTab', () => {
  it("lists the key's rules and budgets with what each has used", async () => {
    mockKeyLimits()
    renderWithQueryClient(<KeyLimitsTab keyId="key-1" canEdit />)

    await waitFor(() => {
      expect(screen.getByText('40%')).toBeInTheDocument()
    })
    // 100 - 40 and 1,000,000 - 250,000 (en-US grouping).
    expect(screen.getByText('60 remaining')).toBeInTheDocument()
    expect(screen.getByText('750,000 remaining')).toBeInTheDocument()
    expect(screen.getByText('25%')).toBeInTheDocument()
    expect(screen.getByText('budget · Daily')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /add limit/i })).toBeInTheDocument()
    expect(screen.getAllByRole('button', { name: /edit/i })).toHaveLength(2)
  })

  it('says when the key has no limits of its own', async () => {
    mockKeyLimits({ rules: [], caps: [], usage: { rules: [], caps: [] } })
    renderWithQueryClient(<KeyLimitsTab keyId="key-1" canEdit />)

    await waitFor(() => {
      expect(screen.getByText('This key has no limits of its own.')).toBeInTheDocument()
    })
  })

  it('is read-only without rate_limits:write', async () => {
    mockKeyLimits()
    renderWithQueryClient(<KeyLimitsTab keyId="key-1" canEdit={false} />)

    await waitFor(() => {
      expect(screen.getByText('40%')).toBeInTheDocument()
    })
    expect(screen.queryByRole('button', { name: /add limit/i })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /edit/i })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /delete/i })).not.toBeInTheDocument()
  })

  it("adds a rule to the key's lineage, lasting until removed", async () => {
    mockKeyLimits({ rules: [], caps: [], usage: { rules: [], caps: [] } })
    mockPost.mockResolvedValueOnce({})
    const user = userEvent.setup()
    renderWithQueryClient(<KeyLimitsTab keyId="key-1" canEdit />)

    await user.click(await screen.findByRole('button', { name: /add limit/i }))
    await user.type(screen.getByRole('spinbutton'), '500')
    await user.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => {
      expect(mockPost).toHaveBeenCalledWith(`${BASE}/rules`, {
        surface: 'ai_gateway',
        metric: 'requests',
        window_secs: 3600,
        max_count: 500,
        enabled: true,
        expires_at: null,
        reason: null,
      })
    })
  })

  it('edits a rule in its slot, keeping its expiry and reason', async () => {
    const expires = '2026-11-01T12:00:00.000Z'
    mockKeyLimits({
      rules: [
        {
          id: 'r1',
          surface: 'ai_gateway',
          metric: 'tokens',
          window_secs: 18000,
          max_count: 50_000,
          enabled: true,
          expires_at: expires,
          reason: 'launch week',
        },
      ],
      caps: [],
      usage: { rules: [{ rule_id: 'r1', current: 0, limit: 50_000 }], caps: [] },
    })
    mockPost.mockResolvedValueOnce({})
    const user = userEvent.setup()
    renderWithQueryClient(<KeyLimitsTab keyId="key-1" canEdit />)

    await user.click(await screen.findByRole('button', { name: /edit/i }))
    expect(screen.getByText('Edit limit')).toBeInTheDocument()
    // Type, gateway, metric and window are the slot the row is stored by,
    // so an edit can't change them; the expiry stays open.
    const selects = screen.getAllByRole('combobox')
    expect(selects).toHaveLength(5)
    expect(selects.filter((s) => (s as HTMLButtonElement).disabled)).toHaveLength(4)
    const value = screen.getByRole('spinbutton')
    await user.clear(value)
    await user.type(value, '80000')
    await user.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() => {
      expect(mockPost).toHaveBeenCalledWith(`${BASE}/rules`, {
        surface: 'ai_gateway',
        metric: 'tokens',
        window_secs: 18000,
        max_count: 80_000,
        enabled: true,
        // Prefilled to the minute in local time, sent back as UTC.
        expires_at: expires,
        reason: 'launch week',
      })
    })
  })

  it('deletes a budget from the key after confirmation', async () => {
    mockKeyLimits()
    mockDelete.mockResolvedValueOnce(undefined)
    const user = userEvent.setup()
    renderWithQueryClient(<KeyLimitsTab keyId="key-1" canEdit />)

    await screen.findByText('budget · Daily')
    const deletes = screen.getAllByRole('button', { name: /delete/i })
    // Rules come first, then budgets.
    await user.click(deletes[1])
    const dialog = await screen.findByRole('dialog')
    expect(within(dialog).getByText('Delete this limit?')).toBeInTheDocument()
    await user.click(within(dialog).getByRole('button', { name: 'Delete' }))

    await waitFor(() => {
      expect(mockDelete).toHaveBeenCalledWith(`${BASE}/budgets/c1`)
    })
  })
})
