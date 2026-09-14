import { describe, it, expect, vi, beforeEach } from 'vitest'
import { screen } from '@testing-library/react'
import type { QueryClient } from '@tanstack/react-query'
import { createQueryClient } from '@/lib/query-client'
import { renderWithQueryClient } from '@/test/render'
import { BatchImportDialog } from './BatchImportDialog'
import type { Provider } from '../provider-types'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPost: vi.fn(),
}))

import { api } from '@/lib/api'

const provider = { id: 'prov-1', name: 'openai', display_name: 'OpenAI' } as Provider
const routesKey = ['admin', 'model-routes', { provider_id: 'prov-1', page: 1, page_size: 10000 }]

/// Serve the provider's upstream catalog, and `routes` as its imported routes.
function serve(routes: { model_id: string; upstream_model: string }[]) {
  vi.mocked(api).mockImplementation(async (url: string) => {
    if (url === '/api/admin/models/ids') return []
    if (url === '/api/admin/providers/prov-1/remote-models') {
      return [{ id: 'gpt-4o' }, { id: 'o3', available: false, reason: 'not enabled' }]
    }
    if (url.startsWith('/api/admin/model-routes?provider_id=prov-1')) {
      return { items: routes, total: routes.length }
    }
    throw new Error(`unexpected request: ${url}`)
  })
}

// Opened with a provider already chosen, the way the Providers page's
// "Import models" shortcut opens it.
function renderDialog(client?: QueryClient) {
  return renderWithQueryClient(
    <BatchImportDialog
      open
      initialProviderId="prov-1"
      providers={[provider]}
      detailModelId={null}
      onClose={vi.fn()}
      onSaved={vi.fn()}
      onSavedForModel={vi.fn()}
    />,
    client,
  )
}

beforeEach(() => {
  vi.clearAllMocks()
  serve([])
})

describe('BatchImportDialog', () => {
  // The deeplink usually finds the provider list cached, so the dialog can
  // mount already open, with no closed-to-open transition to reset on.
  it('pre-selects the deeplinked provider when it mounts already open', async () => {
    renderDialog()

    expect(await screen.findByText('gpt-4o')).toBeInTheDocument()
    expect(screen.getByText('o3')).toBeInTheDocument()
  })

  // A cached copy of the provider's lists can predate an import made since.
  // Offering from it would show a just-imported model as importable again.
  it('offers nothing from a cached copy until the refetch lands', async () => {
    const client = createQueryClient()
    client.setQueryData(['admin', 'providers', 'prov-1', 'remote-models'], [{ id: 'gpt-4o' }])
    client.setQueryData(routesKey, { items: [], total: 0 })
    serve([{ model_id: 'gpt-4o', upstream_model: 'gpt-4o' }])

    renderDialog(client)

    expect(screen.queryByText('gpt-4o')).not.toBeInTheDocument()
    expect(await screen.findByText('(Already exists)')).toBeInTheDocument()
  })
})
