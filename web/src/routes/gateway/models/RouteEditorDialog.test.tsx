import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClientProvider } from '@tanstack/react-query'
import { createQueryClient } from '@/lib/query-client'
import { RouteEditorDialog } from './RouteEditorDialog'
import type { RouteRow } from './types'
import type { Provider } from '../provider-types'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPatch: vi.fn(),
  apiPost: vi.fn(),
}))

import { api, apiPatch } from '@/lib/api'

const provider = { id: 'prov-1', name: 'bedrock', display_name: 'Bedrock' } as Provider

const route: RouteRow = {
  id: 'route-1',
  model_id: 'claude-sonnet',
  provider_id: 'prov-1',
  provider_name: 'Bedrock',
  upstream_model: 'us.anthropic.claude-sonnet-4-5-20250929-v1:0',
  weight: 100,
  enabled: true,
}

// Mounted closed and then opened, as the Models page does: opening is
// when the dialog loads the route into its form.
function renderEditor() {
  const client = createQueryClient()
  const editor = (open: boolean) => (
    <QueryClientProvider client={client}>
      <RouteEditorDialog
        open={open}
        route={route}
        targetModel={null}
        providers={[provider]}
        routeHealth={{}}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />
    </QueryClientProvider>
  )
  render(editor(false)).rerender(editor(true))
}

beforeEach(() => {
  vi.clearAllMocks()
  vi.mocked(api).mockImplementation(async (url: string) => {
    if (url === '/api/admin/providers/prov-1/remote-models') {
      return [
        { id: 'amazon.nova-lite-v1:0' },
        { id: 'global.anthropic.claude-sonnet-4-5-20250929-v1:0' },
        { id: 'us.anthropic.claude-sonnet-4-5-20250929-v1:0' },
      ]
    }
    throw new Error(`unexpected request: ${url}`)
  })
})

/** The upstream model field, once the provider's list has arrived. */
async function upstreamField() {
  const field = screen.getByLabelText('Upstream Model')
  await vi.waitFor(() => expect(api).toHaveBeenCalled())
  return field
}

describe('RouteEditorDialog', () => {
  // The provider's listing is a suggestion: it can leave out a model the
  // upstream serves, and the admin knows best.
  it('takes an upstream model the provider does not list', async () => {
    const user = userEvent.setup()
    renderEditor()

    const field = await upstreamField()
    await user.clear(field)
    await user.type(field, 'arn:aws:bedrock:us-east-1:123456789012:custom-model/x')
    await user.click(screen.getByRole('button', { name: 'Save' }))

    expect(apiPatch).toHaveBeenCalledWith(
      '/api/admin/model-routes/route-1',
      expect.objectContaining({
        upstream_model: 'arn:aws:bedrock:us-east-1:123456789012:custom-model/x',
      }),
    )
  })

  it('suggests the models the provider lists, filtered by what is typed', async () => {
    const user = userEvent.setup()
    renderEditor()

    const field = await upstreamField()
    await user.clear(field)
    await user.type(field, 'sonnet')
    expect(await screen.findAllByRole('option')).toHaveLength(2)
    await user.click(
      screen.getByRole('option', { name: 'global.anthropic.claude-sonnet-4-5-20250929-v1:0' }),
    )

    expect(field).toHaveValue('global.anthropic.claude-sonnet-4-5-20250929-v1:0')
    expect(screen.queryByRole('option')).not.toBeInTheDocument()
  })

  it('picks a suggestion with the keyboard without saving', async () => {
    const user = userEvent.setup()
    renderEditor()

    const field = await upstreamField()
    await user.clear(field)
    await user.type(field, 'nova')
    await user.keyboard('{ArrowDown}{Enter}')

    expect(field).toHaveValue('amazon.nova-lite-v1:0')
    expect(apiPatch).not.toHaveBeenCalled()
  })
})
