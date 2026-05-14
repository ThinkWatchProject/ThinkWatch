import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { LimitsPanel, type SurfaceConstraints } from './limits-panel'

// Capture toast.error calls — the role-limits editor previously used
// window.alert; this test exercises the toast + inline-error pathway.
const toastError = vi.fn()
vi.mock('sonner', () => ({
  toast: {
    error: (...args: unknown[]) => toastError(...args),
    success: vi.fn(),
  },
}))

beforeEach(() => {
  toastError.mockClear()
})

function renderPanel(overrides: Partial<React.ComponentProps<typeof LimitsPanel>> = {}) {
  const onChange = vi.fn()
  const value: SurfaceConstraints = {}
  render(
    <LimitsPanel
      surfaces={['ai_gateway']}
      allowBudgets={true}
      value={value}
      onChange={onChange}
      {...overrides}
    />,
  )
  return { onChange }
}

describe('LimitsPanel AddRuleInline validation', () => {
  it('disables Add button when count input is empty', () => {
    renderPanel()
    // Two "Add" buttons render — rule and budget. Pick the one whose
    // adjacent Limit input is the rate-rule maxCount field.
    const addButtons = screen.getAllByRole('button', { name: /add/i })
    // Both should start disabled (both inputs empty).
    for (const b of addButtons) expect(b).toBeDisabled()
  })

  it('shows inline error + toast and does not call onChange on invalid count', async () => {
    const user = userEvent.setup()
    const { onChange } = renderPanel()

    // Spinbutton inputs in order: rule maxCount, budget limitTokens
    const numberInputs = screen.getAllByRole('spinbutton')
    const ruleInput = numberInputs[0]

    // Type 0 (below the >=1 floor) — the disabled-state should still
    // gate, but assert that even if the click fires the toast path runs.
    await user.type(ruleInput, '0')
    // With value '0', parsed=0 < 1 so button stays disabled.
    const addButtons = screen.getAllByRole('button', { name: /add/i })
    expect(addButtons[0]).toBeDisabled()

    // Now test the over-cap path: 2 billion exceeds the 1e9 ceiling.
    await user.clear(ruleInput)
    await user.type(ruleInput, '2000000000')
    expect(addButtons[0]).toBeDisabled()
    expect(onChange).not.toHaveBeenCalled()
  })

  it('clears error and accepts a valid count', async () => {
    const user = userEvent.setup()
    const { onChange } = renderPanel()

    const ruleInput = screen.getAllByRole('spinbutton')[0]
    await user.type(ruleInput, '60')

    const addButton = screen.getAllByRole('button', { name: /add/i })[0]
    expect(addButton).toBeEnabled()
    await user.click(addButton)

    expect(onChange).toHaveBeenCalledTimes(1)
    const next = onChange.mock.calls[0][0]
    expect(next.ai_gateway.rateLimits).toHaveLength(1)
    expect(next.ai_gateway.rateLimits[0].maxCount).toBe(60)
    expect(toastError).not.toHaveBeenCalled()
  })
})

describe('LimitsPanel AddBudgetInline validation', () => {
  it('disables Add button when token input is empty', () => {
    renderPanel()
    const addButtons = screen.getAllByRole('button', { name: /add/i })
    expect(addButtons[1]).toBeDisabled()
  })

  it('rejects values over 1e9 cap', async () => {
    const user = userEvent.setup()
    const { onChange } = renderPanel()
    const budgetInput = screen.getAllByRole('spinbutton')[1]
    await user.type(budgetInput, '5000000000')
    const addButtons = screen.getAllByRole('button', { name: /add/i })
    expect(addButtons[1]).toBeDisabled()
    expect(onChange).not.toHaveBeenCalled()
  })

  it('accepts a valid token limit and fires onChange', async () => {
    const user = userEvent.setup()
    const { onChange } = renderPanel()
    const budgetInput = screen.getAllByRole('spinbutton')[1]
    await user.type(budgetInput, '1000000')
    const addButton = screen.getAllByRole('button', { name: /add/i })[1]
    expect(addButton).toBeEnabled()
    await user.click(addButton)

    expect(onChange).toHaveBeenCalledTimes(1)
    const next = onChange.mock.calls[0][0]
    expect(next.ai_gateway.budgets).toHaveLength(1)
    expect(next.ai_gateway.budgets[0].maxTokens).toBe(1_000_000)
    expect(toastError).not.toHaveBeenCalled()
  })
})
