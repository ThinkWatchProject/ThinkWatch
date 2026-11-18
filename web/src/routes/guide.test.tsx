import { describe, it, expect, vi, beforeEach } from 'vitest'
import { screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import type { ReactNode } from 'react'
import { renderWithQueryClient } from '@/test/render'
import { GuidePage } from './guide'

vi.mock('@/lib/api', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/lib/api')>()),
  api: vi.fn(),
  hasPermission: vi.fn(() => false),
}))

// The guide links to other console pages; there is no router here.
vi.mock('@tanstack/react-router', () => ({
  Link: ({ to, children }: { to: string; children: ReactNode }) => <a href={to}>{children}</a>,
}))

import { api } from '@/lib/api'

const mockApi = vi.mocked(api)

// jsdom's page: http://localhost:3000/. Its address without the
// configuration — the browser's scheme and host, port 3000.
const FROM_BROWSER = `${window.location.protocol}//${window.location.hostname}:3000`

async function claudeCodeSnippet() {
  const user = userEvent.setup()
  await user.click(screen.getByRole('tab', { name: 'Claude Code' }))
  return (await screen.findByText(/ANTHROPIC_BASE_URL=/)).textContent ?? ''
}

beforeEach(() => {
  vi.clearAllMocks()
})

describe('GuidePage', () => {
  it('shows the gateway Codex profile and the native catalog endpoint', async () => {
    mockApi.mockResolvedValue({
      public_protocol: 'https', public_host: 'gw.example.com', public_port: 443, gateway_port: 3000,
    })
    renderWithQueryClient(<GuidePage />)

    const user = userEvent.setup()
    await user.click(screen.getByRole('tab', { name: 'Codex' }))
    await waitFor(() => {
      const config = screen.getByText(/api_key_model_discovery = true/).textContent
      expect(config).toContain('base_url = "https://gw.example.com/v1"')
      expect(config).toContain('model_catalog_url = "https://gw.example.com/codex/models"')
      expect(config).toContain('env_key = "THINKWATCH_GATEWAY_KEY"')
      expect(config).toContain('[profiles.thinkwatch]')
    })
    expect(screen.getByText(/codex --profile thinkwatch/)).toBeInTheDocument()
  })

  it('shows the configured gateway address', async () => {
    mockApi.mockResolvedValue({
      public_protocol: 'https',
      public_host: 'gw.example.com',
      public_port: null,
      gateway_port: 8443,
    })
    renderWithQueryClient(<GuidePage />)

    expect(await screen.findByText('https://gw.example.com:8443')).toBeInTheDocument()
    expect(await claudeCodeSnippet()).toContain('ANTHROPIC_BASE_URL=https://gw.example.com:8443\n')
  })

  it("uses the browser's address while the configured one loads", async () => {
    mockApi.mockReturnValue(new Promise(() => {}))
    renderWithQueryClient(<GuidePage />)

    expect(await claudeCodeSnippet()).toContain(`ANTHROPIC_BASE_URL=${FROM_BROWSER}\n`)
  })

  it("uses the browser's address when the configured one cannot be read", async () => {
    mockApi.mockRejectedValue(new Error('500'))
    renderWithQueryClient(<GuidePage />)

    await waitFor(() => expect(screen.getByText(FROM_BROWSER)).toBeInTheDocument())
    expect(screen.getByText(`${FROM_BROWSER}/mcp`)).toBeInTheDocument()
    expect(await claudeCodeSnippet()).toContain(`ANTHROPIC_BASE_URL=${FROM_BROWSER}\n`)
  })
})
