import { describe, it, expect, vi, beforeAll, beforeEach } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent, { type UserEvent } from '@testing-library/user-event'
import { QueryClientProvider } from '@tanstack/react-query'
import { createQueryClient } from '@/lib/query-client'
import { RouteEditorDialog } from './RouteEditorDialog'
import type { ModelRow, RouteRow } from './types'
import type { Provider } from '../provider-types'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPatch: vi.fn(),
  apiPost: vi.fn(),
  ApiError: class ApiError extends Error {
    constructor(
      message: string,
      public status: number,
      public type?: string,
    ) {
      super(message)
    }
  },
}))

import { api, apiPatch, apiPost, ApiError } from '@/lib/api'

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

const model = { model_id: 'claude-opus' } as ModelRow

// Mounted closed and then opened, as the Models page does: opening is
// when the dialog loads the route into its form. Edits `route`, or adds
// one to `target`.
function renderEditor(target?: ModelRow) {
  const client = createQueryClient()
  const editor = (open: boolean) => (
    <QueryClientProvider client={client}>
      <RouteEditorDialog
        open={open}
        route={target ? null : route}
        targetModel={target ?? null}
        providers={[provider]}
        routeHealth={{}}
        onClose={vi.fn()}
        onSaved={vi.fn()}
      />
    </QueryClientProvider>
  )
  render(editor(false)).rerender(editor(true))
}

beforeAll(() => {
  // Radix Select captures the pointer and scrolls its options into view;
  // jsdom implements neither.
  Object.assign(Element.prototype, {
    hasPointerCapture: () => false,
    setPointerCapture: () => {},
    releasePointerCapture: () => {},
    scrollIntoView: () => {},
  })
})

/** Add a route for `upstream` on the Bedrock provider, and save. */
async function addRoute(user: UserEvent, upstream: string) {
  renderEditor(model)
  await user.click(screen.getByLabelText('Provider'))
  await user.click(await screen.findByRole('option', { name: 'Bedrock' }))
  const field = screen.getByLabelText('Upstream Model')
  await user.clear(field)
  await user.type(field, upstream)
  await user.click(screen.getByRole('button', { name: 'Save' }))
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

  it('creates a route the provider refused once told to', async () => {
    const user = userEvent.setup()
    vi.mocked(apiPost)
      .mockRejectedValueOnce(
        new ApiError(
          "Provider does not serve 'us.anthropic.claude-opus-4-1-20250805-v1:0': refused",
          400,
          'model_not_served',
        ),
      )
      .mockResolvedValueOnce({})

    await addRoute(user, 'us.anthropic.claude-opus-4-1-20250805-v1:0')
    expect(await screen.findByText(/Provider does not serve/)).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Create anyway' }))

    expect(apiPost).toHaveBeenCalledTimes(2)
    expect(apiPost).toHaveBeenLastCalledWith(
      '/api/admin/models/claude-opus/routes',
      expect.objectContaining({
        provider_id: 'prov-1',
        upstream_model: 'us.anthropic.claude-opus-4-1-20250805-v1:0',
        force: true,
      }),
    )
  })

  // Forcing helps only past the provider's refusal: any other error
  // would come back the same.
  it('offers no way past any other error', async () => {
    const user = userEvent.setup()
    vi.mocked(apiPost).mockRejectedValueOnce(
      new ApiError('A route for this model+provider+upstream already exists', 400, 'bad_request'),
    )

    await addRoute(user, 'amazon.nova-lite-v1:0')

    expect(await screen.findByText(/already exists/)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Create anyway' })).not.toBeInTheDocument()
    expect(vi.mocked(apiPost).mock.calls[0][1]).not.toHaveProperty('force')
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
