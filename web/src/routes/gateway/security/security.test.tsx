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
  inspect_tools: {
    mode: 'enforce',
    rules: [
      {
        id: 'curl-pipe-sh',
        custom: false,
        name: 'Download and run',
        why: 'Downloads and runs it straight away',
        kind: 'command',
        matcher: { kind: 'regex', pattern: '(curl|wget)[^\\n|]*\\|\\s*sh' },
        enabled: true,
        on_by_default: true,
        action: 'cut',
        default_action: 'cut',
      },
      {
        id: 'secret-to-unknown-host',
        custom: false,
        name: 'Send a credential to an unknown host',
        why: "Sends a credential to a host that is neither local nor the credential's own provider",
        kind: 'command',
        matcher: { kind: 'builtin', check: 'credential-to-network' },
        enabled: true,
        on_by_default: true,
        action: 'cut',
        default_action: 'cut',
      },
    ],
  },
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
    // The pattern is tried with the action chosen for it.
    await user.type(within(dialog).getByLabelText('Test text'), 'a b')
    await waitFor(() =>
      expect(api).toHaveBeenCalledWith(
        '/api/admin/security/content/test',
        expect.objectContaining({
          method: 'POST',
          body: { sample: 'a b', pattern: 'U+200B-U+200D', match: 'codepoints', action: 'strip' },
        }),
      ),
    )
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

  it('describes a tool-call check implemented in code and offers no copy of it', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Tool-call inspection/ }))
    const check = "A credential sent to a host other than this machine and the credential's own provider"
    expect(await screen.findByText(check)).toBeInTheDocument()

    await user.click(screen.getByText('Send a credential to an unknown host'))
    const dialog = await screen.findByRole('dialog')
    expect(within(dialog).getByText(check)).toBeInTheDocument()
    expect(within(dialog).queryByRole('button', { name: 'Copy as a custom rule' })).not.toBeInTheDocument()
    await user.keyboard('{Escape}')

    await user.click(screen.getByText('Download and run'))
    expect(
      await within(await screen.findByRole('dialog')).findByRole('button', { name: 'Copy as a custom rule' }),
    ).toBeInTheDocument()
  })

  it('tries a redaction pattern under its placeholder name, without an action', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('button', { name: 'New rule' }))
    const dialog = await screen.findByRole('dialog')
    await user.type(within(dialog).getByLabelText('Regular expression'), 'PRJ-\\d+')
    await user.clear(within(dialog).getByLabelText('Placeholder name'))
    await user.type(within(dialog).getByLabelText('Placeholder name'), 'PROJECT')
    await user.type(within(dialog).getByLabelText('Test text'), 'see PRJ-12')

    await waitFor(() =>
      expect(api).toHaveBeenCalledWith(
        '/api/admin/security/redact/test',
        expect.objectContaining({
          body: { sample: 'see PRJ-12', pattern: 'PRJ-\\d+', label: 'PROJECT' },
        }),
      ),
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
