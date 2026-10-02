import { beforeEach, describe, expect, it, vi } from 'vitest'
import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { renderWithQueryClient } from '@/test/render'
import type { SecurityDetail } from '@/lib/security-types'
import { GatewaySecurityPage } from '.'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPatch: vi.fn(),
  hasPermission: vi.fn(() => true),
}))

import { api, apiPatch, hasPermission } from '@/lib/api'

const detail = (): SecurityDetail => ({
  redact: {
    mode: 'observe',
    rules: [
      {
        id: 'anthropic-api-key',
        custom: false,
        name: 'Anthropic API key',
        kind: 'api-keys',
        matcher: { kind: 'prefix', prefix: 'sk-ant-', min_tail: 20 },
        enabled: true,
        on_by_default: true,
        label: 'SECRET',
      },
      {
        id: 'email',
        custom: false,
        name: 'Email address',
        kind: 'personal',
        matcher: { kind: 'email' },
        enabled: false,
        on_by_default: false,
        label: 'EMAIL',
      },
    ],
  },
  inspect_tools: { mode: 'observe', rules: [] },
  content: {
    mode: 'observe',
    rules: [
      {
        id: 'unicode-tags',
        custom: false,
        name: 'Unicode tag characters',
        why: 'Entirely invisible in an editor.',
        kind: 'invisible',
        matcher: { kind: 'codepoints', ranges: ['U+E0000–U+E007F'] },
        enabled: true,
        on_by_default: true,
        action: 'strip',
        default_action: 'strip',
      },
    ],
  },
})

beforeEach(() => {
  vi.clearAllMocks()
  vi.mocked(hasPermission).mockReturnValue(true)
  vi.mocked(api).mockImplementation(async (url: string) => {
    if (url === '/api/admin/security') return detail()
    if (url.endsWith('/test')) return { hits: [], output: null, refused: false }
    throw new Error(`unexpected request: ${url}`)
  })
  vi.mocked(apiPatch).mockResolvedValue({})
})

describe('GatewaySecurityPage', () => {
  it('lists the rules of each guard by group', async () => {
    renderWithQueryClient(<GatewaySecurityPage />)

    expect(await screen.findByText('Anthropic API key')).toBeInTheDocument()
    expect(screen.getByText('API keys')).toBeInTheDocument()
    expect(screen.getByText('Personal information')).toBeInTheDocument()
    expect(screen.getByText('<<TW_EMAIL_1>>')).toBeInTheDocument()
    // The third mode of redaction is named for what it does.
    expect(screen.getByRole('radio', { name: 'Replace' })).toBeInTheDocument()
  })

  it('writes the whole policy when a built-in rule is switched on', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('switch', { name: 'Turn on "Email address"' }))

    await waitFor(() =>
      expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
        settings: { 'security.redact': { mode: 'observe', enable: ['email'] } },
      }),
    )
  })

  it('switches a guard to its third mode', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Content filter/ }))
    const modes = await screen.findByRole('radiogroup', { name: 'Content filter mode' })
    await user.click(within(modes).getByRole('radio', { name: 'Enforce' }))

    await waitFor(() =>
      expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
        settings: { 'security.content': { mode: 'enforce' } },
      }),
    )
  })

  it('checks code points as they are typed and saves a custom rule', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Content filter/ }))
    await user.click(await screen.findByRole('button', { name: 'New rule' }))
    const dialog = await screen.findByRole('dialog')
    await user.type(within(dialog).getByLabelText('Name'), 'Zero width')
    await user.click(within(dialog).getByRole('radio', { name: 'Code points' }))
    await user.click(within(dialog).getByRole('radio', { name: 'Delete' }))
    const field = within(dialog).getByLabelText('Code points')

    await user.type(field, 'U+200B, 200C')
    expect(within(dialog).getAllByText('Not a code point or a range: 200C').length).toBeGreaterThan(0)
    expect(within(dialog).getByRole('button', { name: 'Create' })).toBeDisabled()

    await user.clear(field)
    await user.type(field, 'U+200B-U+200D')
    await user.click(within(dialog).getByRole('button', { name: 'Create' }))

    await waitFor(() =>
      expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
        settings: {
          'security.content': {
            mode: 'observe',
            custom: [{ name: 'Zero width', pattern: 'U+200B-U+200D', match: 'codepoints', action: 'strip' }],
          },
        },
      }),
    )
  })

  it('keeps a refused rule in its dialog with the reason', async () => {
    const user = userEvent.setup()
    vi.mocked(apiPatch).mockRejectedValueOnce(new Error('regex parse error: unclosed group'))
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('button', { name: 'New rule' }))
    const dialog = await screen.findByRole('dialog')
    await user.type(within(dialog).getByLabelText('Name'), 'Broken')
    await user.type(within(dialog).getByLabelText('Regular expression'), 'PRJ-(')
    await user.click(within(dialog).getByRole('button', { name: 'Create' }))

    expect(await within(dialog).findByText(/unclosed group/)).toBeInTheDocument()
    expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
      settings: {
        'security.redact': {
          mode: 'observe',
          custom: [{ name: 'Broken', pattern: 'PRJ-(', label: 'SECRET' }],
        },
      },
    })
  })

  it('offers no writes without the write permission', async () => {
    vi.mocked(hasPermission).mockImplementation((perm: string) => !perm.endsWith(':write'))
    renderWithQueryClient(<GatewaySecurityPage />)

    expect(await screen.findByRole('switch', { name: 'Turn on "Email address"' })).toBeDisabled()
    expect(screen.getByRole('button', { name: 'New rule' })).toBeDisabled()
    expect(screen.getByRole('radio', { name: 'Replace' })).toBeDisabled()
    expect(screen.getByRole('button', { name: 'Test…' })).toBeEnabled()
  })
})
