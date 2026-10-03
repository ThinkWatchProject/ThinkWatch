import { beforeEach, describe, expect, it, vi } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { ModelEditorDialog } from './ModelEditorDialog'
import type { ModelRow } from './types'

vi.mock('@/lib/api', () => ({
  apiPatch: vi.fn(),
  apiPost: vi.fn(),
}))

import { apiPatch, apiPost } from '@/lib/api'

const model = {
  id: 'm-1',
  model_id: 'gpt-4o',
  display_name: 'GPT-4o',
  input_weight: '1.0',
  output_weight: '1.0',
  cache_read_weight: null,
  cache_write_weight: null,
  cache_write_1h_weight: null,
  max_output_tokens: 8192,
} as ModelRow

// Mounted closed and then opened, as the Models page does: opening is
// when the dialog loads the model into its form.
function renderEditor(m: ModelRow | null) {
  const editor = (open: boolean) => (
    <ModelEditorDialog open={open} model={m} pricing={null} onClose={vi.fn()} onSaved={vi.fn()} />
  )
  render(editor(false)).rerender(editor(true))
}

beforeEach(() => {
  vi.clearAllMocks()
  vi.mocked(apiPatch).mockResolvedValue({})
  vi.mocked(apiPost).mockResolvedValue({})
})

describe('ModelEditorDialog max output tokens', () => {
  it('shows the limit and clears it to no limit', async () => {
    const user = userEvent.setup()
    renderEditor(model)

    const field = screen.getByLabelText('Max output tokens')
    expect(field).toHaveValue('8192')
    await user.clear(field)
    await user.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() =>
      expect(apiPatch).toHaveBeenCalledWith(
        '/api/admin/models/m-1',
        expect.objectContaining({ max_output_tokens: null }),
      ),
    )
  })

  it('sends a new limit as a number', async () => {
    const user = userEvent.setup()
    renderEditor(null)

    await user.type(screen.getByLabelText('Model ID'), 'claude-opus')
    await user.type(screen.getByLabelText('Max output tokens'), '4096')
    await user.click(screen.getByRole('button', { name: 'Save' }))

    await waitFor(() =>
      expect(apiPost).toHaveBeenCalledWith(
        '/api/admin/models',
        expect.objectContaining({ model_id: 'claude-opus', max_output_tokens: 4096 }),
      ),
    )
  })

  it('refuses anything but a whole number from 1', async () => {
    const user = userEvent.setup()
    renderEditor(model)

    const field = screen.getByLabelText('Max output tokens')
    for (const bad of ['0', '1.5', 'abc', '-3']) {
      await user.clear(field)
      await user.type(field, bad)
      await user.click(screen.getByRole('button', { name: 'Save' }))
      expect(await screen.findByText(/Max output tokens must be a whole number/)).toBeInTheDocument()
    }
    expect(apiPatch).not.toHaveBeenCalled()
  })
})
