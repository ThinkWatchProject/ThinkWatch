import type { ReactElement } from 'react'
import { render } from '@testing-library/react'
import { QueryClientProvider } from '@tanstack/react-query'
import { createQueryClient } from '@/lib/query-client'

/**
 * `render` inside a query cache of its own. Screens load through TanStack
 * Query, so they need a provider — and a fresh client per call keeps one
 * test's responses out of the next. Pass `client` to start from a cache the
 * test has seeded.
 */
export function renderWithQueryClient(ui: ReactElement, client = createQueryClient()) {
  return render(<QueryClientProvider client={client}>{ui}</QueryClientProvider>)
}
