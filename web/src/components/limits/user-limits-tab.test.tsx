import { describe, it, expect, vi, beforeEach } from 'vitest'
import { screen, waitFor } from '@testing-library/react'
import { renderWithQueryClient } from '@/test/render'
import { UserLimitsTab } from './user-limits-tab'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPost: vi.fn(),
}))

import { api } from '@/lib/api'

const mockApi = vi.mocked(api)

// Minimal dashboard payload — one rule, one cap. Each test overrides
// `current` / `max_count` / `limit_tokens` to exercise the
// remaining / exceeded branches without re-stating every field.
const dashboard = (overrides: {
  ruleMax?: number
  ruleCurrent?: number
  capLimit?: number
  capCurrent?: number
}) => ({
  rules: [
    {
      source: 'role' as const,
      surface: 'ai_gateway' as const,
      metric: 'requests' as const,
      window_secs: 3600,
      max_count: overrides.ruleMax ?? 1000,
      current: overrides.ruleCurrent ?? 250,
      enabled: true,
    },
  ],
  caps: [
    {
      source: 'role' as const,
      surface: 'ai_gateway' as const,
      period: 'monthly' as const,
      limit_tokens: overrides.capLimit ?? 1_000_000,
      current: overrides.capCurrent ?? 600_000,
      enabled: true,
    },
  ],
  usage_7d: [],
  recent_events: [],
})

beforeEach(() => {
  vi.clearAllMocks()
})

describe('UserLimitsTab — remaining / exceeded labels', () => {
  it('shows the remaining count beside the percentage when under cap', async () => {
    mockApi.mockResolvedValueOnce(
      dashboard({ ruleMax: 1000, ruleCurrent: 250, capLimit: 1_000_000, capCurrent: 600_000 }),
    )

    renderWithQueryClient(<UserLimitsTab userId="user-1" />)

    await waitFor(() => {
      expect(screen.getByText('25%')).toBeInTheDocument()
    })

    // 1000 - 250 = 750
    expect(screen.getByText('750 remaining')).toBeInTheDocument()
    // 1,000,000 - 600,000 = 400,000  (en-US grouping)
    expect(screen.getByText('400,000 remaining')).toBeInTheDocument()
  })

  it('shows "Exceeded" instead of remaining when current >= cap', async () => {
    mockApi.mockResolvedValueOnce(
      dashboard({ ruleMax: 100, ruleCurrent: 100, capLimit: 500, capCurrent: 600 }),
    )

    renderWithQueryClient(<UserLimitsTab userId="user-2" />)

    await waitFor(() => {
      // Rule pct caps at 100 because of Math.min, cap pct also caps at 100
      expect(screen.getAllByText('100%').length).toBeGreaterThanOrEqual(2)
    })

    const exceeded = screen.getAllByText('Exceeded')
    // One badge per overflowing row (rule + cap).
    expect(exceeded.length).toBe(2)
    expect(screen.queryByText(/remaining/)).not.toBeInTheDocument()
  })

  it('hides the remaining label when the slot has no defined cap (max_count=0)', async () => {
    mockApi.mockResolvedValueOnce({
      rules: [
        {
          source: 'role' as const,
          surface: 'ai_gateway' as const,
          metric: 'requests' as const,
          window_secs: 3600,
          max_count: 0,
          current: 0,
          enabled: true,
        },
      ],
      caps: [],
      usage_7d: [],
      recent_events: [],
    })

    renderWithQueryClient(<UserLimitsTab userId="user-3" />)

    await waitFor(() => {
      expect(screen.getByText('0%')).toBeInTheDocument()
    })

    // 0 - 0 = 0, not > 0, so no label. And not over cap (max_count not > 0).
    expect(screen.queryByText(/remaining/)).not.toBeInTheDocument()
    expect(screen.queryByText('Exceeded')).not.toBeInTheDocument()
  })
})
