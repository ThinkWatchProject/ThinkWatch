import { describe, it, expect, vi, beforeEach } from 'vitest'
import type { ReactNode } from 'react'
import { act, renderHook, waitFor } from '@testing-library/react'
import { QueryClientProvider } from '@tanstack/react-query'
import { createQueryClient } from '@/lib/query-client'
import { useAuth } from './use-auth'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPost: vi.fn(),
  broadcastLogout: vi.fn(),
  clearCachedPermissions: vi.fn(),
  registerKeyPair: vi.fn(),
  setCachedPermissions: vi.fn(),
}))

// logout() loads the key store lazily, and jsdom has no IndexedDB behind it.
vi.mock('@/lib/crypto-store', () => ({ clearSigningKey: vi.fn() }))

import { api, apiPost } from '@/lib/api'

const signedIn = {
  id: 'user-1',
  email: 'admin@example.com',
  permissions: [],
  denied_permissions: [],
}
const teamsKey = ['admin', 'teams']

function renderAuth() {
  const client = createQueryClient()
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  )
  return { client, ...renderHook(() => useAuth(), { wrapper }) }
}

beforeEach(() => {
  vi.clearAllMocks()
  vi.mocked(api).mockResolvedValue(signedIn)
  vi.mocked(apiPost).mockResolvedValue({})
})

// The query cache outlives a session in the tab. Ending a session has to
// take every cached screen with it, or the next person to sign in here
// would be shown the previous user's data until each screen refetched.
//
// The cache is swept synchronously; the hook's own re-render arrives with
// the query layer's next notification, hence `waitFor` for `user`.
describe('useAuth — ending a session', () => {
  it('logout signs the user out and drops everything cached for them', async () => {
    const { client, result } = renderAuth()
    await waitFor(() => expect(result.current.user).toEqual(signedIn))
    client.setQueryData(teamsKey, [{ id: 'team-1', name: 'engineering' }])

    await act(() => result.current.logout())

    expect(client.getQueryData(teamsKey)).toBeUndefined()
    await waitFor(() => expect(result.current.user).toBeNull())
  })

  it('a logout in another tab ends the session here too', async () => {
    const { client, result } = renderAuth()
    await waitFor(() => expect(result.current.user).toEqual(signedIn))
    client.setQueryData(teamsKey, [{ id: 'team-1', name: 'engineering' }])

    act(() => {
      window.dispatchEvent(new CustomEvent('thinkwatch:logged-out'))
    })

    expect(client.getQueryData(teamsKey)).toBeUndefined()
    await waitFor(() => expect(result.current.user).toBeNull())
  })
})
