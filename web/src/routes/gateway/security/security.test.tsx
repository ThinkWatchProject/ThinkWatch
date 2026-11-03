import type { ReactNode } from 'react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { renderWithQueryClient } from '@/test/render'
import type { SecurityDetail } from '@/lib/security-types'
import { GatewaySecurityPage } from '.'

vi.mock('@/lib/api', () => ({
  api: vi.fn(),
  apiPatch: vi.fn(),
  apiPost: vi.fn(),
  apiDelete: vi.fn(),
  hasPermission: vi.fn(() => true),
  ApiError: class ApiError extends Error {
    status: number
    constructor(message: string, status: number) {
      super(message)
      this.status = status
    }
  },
}))

vi.mock('@tanstack/react-router', () => ({
  Link: ({ children }: { children: ReactNode }) => <a href="#trace">{children}</a>,
}))

import { api, apiDelete, apiPatch, apiPost, hasPermission } from '@/lib/api'

const exception = {
  sha256: 'a'.repeat(64),
  masked: 'sk-an…AAAA',
  rule: 'anthropic-api-key',
  added_at: Date.UTC(2026, 9, 29, 8),
}
let exceptions: { items: (typeof exception)[]; max: number } = { items: [], max: 500 }

const detail = (): SecurityDetail => ({
  redact: {
    mode: 'observe',
    rules: [
      {
        id: 'thinkwatch-saved-secret',
        custom: false,
        name: 'Key saved in ThinkWatch',
        name_zh: 'ThinkWatch 中保存的密钥',
        about: 'The provider API keys saved in ThinkWatch, by value, whatever their format',
        about_zh: '按值识别 ThinkWatch 中保存的供应商 API 密钥，与格式无关',
        kind: 'saved',
        matcher: { kind: 'saved' },
        enabled: true,
        on_by_default: true,
        action: 'placeholder',
        default_action: 'placeholder',
        label: 'SECRET',
      },
      {
        id: 'anthropic-api-key',
        custom: false,
        name: 'Anthropic API key',
        name_zh: 'Anthropic API 密钥',
        about: 'Starts with `sk-ant-`, followed by at least 20 characters',
        kind: 'ai-services',
        matcher: { kind: 'prefix', prefix: 'sk-ant-', min_tail: 20 },
        enabled: true,
        on_by_default: true,
        action: 'placeholder',
        default_action: 'placeholder',
        label: 'SECRET',
      },
      {
        id: 'email',
        custom: false,
        name: 'Email address',
        name_zh: '邮箱地址',
        about: 'Email addresses, such as `name@example.com`',
        kind: 'contact',
        matcher: { kind: 'email' },
        enabled: false,
        on_by_default: false,
        action: 'placeholder',
        default_action: 'placeholder',
        label: 'EMAIL',
      },
    ],
    kinds: [
      { slug: 'saved', name: 'Keys saved in ThinkWatch', name_zh: 'ThinkWatch 中保存的密钥' },
      { slug: 'ai-services', name: 'AI services', name_zh: 'AI 服务' },
      { slug: 'contact', name: 'Contact details', name_zh: '联系方式' },
      { slug: 'custom', name: 'Custom', name_zh: '自定义' },
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
        why_zh: '将凭据发往既非本机、也不是该凭据服务商的主机',
        kind: 'command',
        matcher: { kind: 'builtin', check: 'credential-to-network' },
        enabled: true,
        on_by_default: true,
        action: 'cut',
        default_action: 'cut',
      },
    ],
    kinds: [
      { slug: 'command', name: 'Built-in', name_zh: '内置' },
      { slug: 'custom', name: 'Custom', name_zh: '自定义' },
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
        scope: ['request'],
      },
    ],
    kinds: [
      { slug: 'invisible', name: 'Hidden characters', name_zh: '隐藏字符' },
      { slug: 'custom', name: 'Custom', name_zh: '自定义' },
    ],
  },
})

const events = {
  total: 3,
  items: [
    {
      id: 'e1',
      action: 'gateway.content_replaced',
      user_email: 'dev@example.com',
      created_at: '2026-10-12T08:00:00Z',
      detail: {
        trace_id: 't1',
        model: 'gpt-5',
        rule: 'Codename',
        custom: true,
        outcome: 'replaced',
        mode: 'enforce',
        direction: 'response',
        count: 2,
      },
    },
    {
      id: 'e2',
      action: 'gateway.redaction_flagged',
      api_key_id: 'k1',
      created_at: '2026-10-12T07:00:00Z',
      detail: { trace_id: 't2', rule: 'anthropic-api-key', custom: false, outcome: 'recorded', mode: 'observe' },
    },
    {
      id: 'e3',
      action: 'gateway.redaction_replaced',
      api_key_id: 'k1',
      created_at: '2026-10-12T06:00:00Z',
      detail: {
        trace_id: 't3',
        rule: 'thinkwatch-saved-secret',
        custom: false,
        outcome: 'replaced',
        mode: 'enforce',
        sources: [{ kind: 'upstream-key', name: 'DeepSeek' }],
      },
    },
  ],
}

beforeEach(() => {
  vi.clearAllMocks()
  vi.mocked(hasPermission).mockReturnValue(true)
  vi.mocked(api).mockImplementation(async (url: string) => {
    if (url === '/api/admin/security') return detail()
    if (url === '/api/admin/security/redact/exceptions') return exceptions
    if (url.startsWith('/api/audit/logs?')) return events
    if (url.endsWith('/test')) return { hits: [], output: null, refused: false }
    throw new Error(`unexpected request: ${url}`)
  })
  vi.mocked(apiPatch).mockResolvedValue({})
  exceptions = { items: [], max: 500 }
})

describe('GatewaySecurityPage', () => {
  it('lists the rules of each guard by group', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    expect(await screen.findByText('Anthropic API key')).toBeInTheDocument()
    // Groups in the server's order, saved keys first.
    const groups = ['Keys saved in ThinkWatch', 'AI services', 'Contact details']
    expect(groups.map((g) => screen.getByText(g).compareDocumentPosition(screen.getByText('Anthropic API key')))).toEqual([
      Node.DOCUMENT_POSITION_FOLLOWING,
      Node.DOCUMENT_POSITION_FOLLOWING,
      Node.DOCUMENT_POSITION_PRECEDING,
    ])
    expect(screen.getByText('The provider API keys saved in ThinkWatch, by value, whatever their format')).toBeInTheDocument()
    expect(screen.getByText('<<TW_EMAIL_1>>')).toBeInTheDocument()
    // The same three states on every guard.
    const modes = screen.getByRole('radiogroup', { name: 'Secrets & privacy mode' })
    expect(within(modes).getAllByRole('radio').map((r) => r.textContent)).toEqual(['On', 'Monitor', 'Off'])
    expect(within(modes).getByRole('radio', { name: 'Monitor' })).toBeChecked()
    expect(screen.getByText('2 of 3 rules on.')).toBeInTheDocument()
    // Monitoring: the action stays as set, and says that only recording happens.
    expect(screen.getAllByText('Monitoring')).toHaveLength(2)
  })

  it('opens on the log, which reads the guard events from the audit log', async () => {
    renderWithQueryClient(<GatewaySecurityPage />)

    expect(await screen.findByText('Answer · text')).toBeInTheDocument()
    expect(screen.getByText('Replaced in 2 places')).toBeInTheDocument()
    expect(screen.getByText('Codename')).toBeInTheDocument()
    // A built-in rule is named; a monitor-mode hit says so.
    expect(screen.getByText('Anthropic API key')).toBeInTheDocument()
    expect(screen.getByText('Monitoring')).toBeInTheDocument()
    // A saved key is named by where it came from, never by its value.
    expect(screen.getByText('Key saved in ThinkWatch')).toBeInTheDocument()
    expect(screen.getByText('API key of provider DeepSeek')).toBeInTheDocument()
    const call = vi.mocked(api).mock.calls.find(([url]) => String(url).startsWith('/api/audit/logs?'))
    const params = new URLSearchParams(String(call?.[0]).split('?')[1])
    expect(params.get('action')?.split(',')).toContain('gateway.content_replaced')
    expect(params.get('action')?.split(',')).toContain('gateway.tool_call_blocked')
  })

  it('leaves the log out without the permission to read the audit log', async () => {
    vi.mocked(hasPermission).mockImplementation((perm: string) => perm !== 'logs:read_all')
    renderWithQueryClient(<GatewaySecurityPage />)

    expect(await screen.findByText('Anthropic API key')).toBeInTheDocument()
    expect(screen.queryByRole('tab', { name: 'Log' })).not.toBeInTheDocument()
  })

  it('switches a whole group at once and filters by a search', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    await user.click(await screen.findByRole('checkbox', { name: 'Turn every rule in "Contact details" on or off' }))
    await waitFor(() =>
      expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
        settings: { 'security.redact': { mode: 'observe', enable: ['email'] } },
      }),
    )

    await user.type(screen.getByRole('searchbox', { name: 'Search rules' }), 'anthropic')
    expect(screen.getByText('Anthropic API key')).toBeInTheDocument()
    expect(screen.queryByText('Email address')).not.toBeInTheDocument()
    await user.clear(screen.getByRole('searchbox', { name: 'Search rules' }))
    await user.type(screen.getByRole('searchbox', { name: 'Search rules' }), 'nothing like it')
    expect(screen.getByText('No rule matches the search.')).toBeInTheDocument()
  })

  it('writes the whole policy when a built-in rule is switched on', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    await user.click(await screen.findByRole('switch', { name: 'Turn on "Email address"' }))

    await waitFor(() =>
      expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
        settings: { 'security.redact': { mode: 'observe', enable: ['email'] } },
      }),
    )
  })

  it('switches a guard on', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Content filter/ }))
    const modes = await screen.findByRole('radiogroup', { name: 'Content filter mode' })
    await user.click(within(modes).getByRole('radio', { name: 'On' }))

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
          body: { sample: 'a b', pattern: 'U+200B-U+200D', match: 'codepoints', action: 'strip', scope: 'request' },
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

  it('checks answers with a rule that replaces, and rules out what answers cannot take', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Content filter/ }))
    await user.click(await screen.findByRole('button', { name: 'New rule' }))
    const dialog = await screen.findByRole('dialog')
    // A new content rule records until told otherwise, and checks requests.
    expect(within(dialog).getByRole('radio', { name: 'Record only' })).toBeChecked()
    expect(within(dialog).getByRole('radio', { name: 'Requests' })).toBeChecked()
    await user.type(within(dialog).getByLabelText('Name'), 'Codename')
    await user.click(within(dialog).getByRole('radio', { name: 'Code points' }))
    await user.click(within(dialog).getByRole('radio', { name: 'Refuse' }))

    // Answers take neither: both switch, and the dialog says so.
    await user.click(within(dialog).getByRole('radio', { name: 'Requests and answers' }))
    expect(within(dialog).getByRole('radio', { name: 'Contains' })).toBeChecked()
    expect(within(dialog).getByRole('radio', { name: 'Code points' })).toBeDisabled()
    expect(within(dialog).getByRole('radio', { name: 'Delete' })).toBeChecked()
    expect(within(dialog).getByRole('radio', { name: 'Refuse' })).toBeDisabled()
    expect(within(dialog).getByText('The rule checks answers, so its action changed to "Delete".')).toBeInTheDocument()
    expect(within(dialog).getByText('The rule checks answers, so matching changed to "Contains".')).toBeInTheDocument()

    await user.click(within(dialog).getByRole('radio', { name: 'Replace' }))
    expect(within(dialog).getByLabelText('Replacement text')).toHaveValue('***')
    await user.click(within(dialog).getByRole('switch', { name: 'Case-sensitive' }))
    await user.type(within(dialog).getByLabelText('Text to find'), 'Polaris')
    await user.click(within(dialog).getByRole('radio', { name: 'Test as an answer' }))
    await user.type(within(dialog).getByLabelText('Test text'), 'Polaris ships')
    await waitFor(() =>
      expect(api).toHaveBeenCalledWith(
        '/api/admin/security/content/test',
        expect.objectContaining({
          body: {
            sample: 'Polaris ships',
            pattern: 'Polaris',
            match: 'contains',
            case_sensitive: true,
            action: 'replace',
            scope: 'response',
            replacement: '***',
          },
        }),
      ),
    )
    // The guard monitors: the dialog says what that means for the rule.
    expect(
      within(dialog).queryByText('This section is monitoring: rules only record hits and change nothing.'),
    ).toBeInTheDocument()
    await user.click(within(dialog).getByRole('button', { name: 'Create' }))

    await waitFor(() =>
      expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
        settings: {
          'security.content': {
            mode: 'observe',
            custom: [
              {
                name: 'Codename',
                pattern: 'Polaris',
                scope: ['request', 'response'],
                match: 'contains',
                case_sensitive: true,
                action: 'replace',
              },
            ],
          },
        },
      }),
    )
  })

  it('describes a tool-call check implemented in code and offers no copy of it', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Dangerous commands/ }))
    // What it is for is the rule's reason; what it matches, a built-in check.
    const why = "Sends a credential to a host that is neither local nor the credential's own provider"
    expect(await screen.findByText(why)).toBeInTheDocument()
    expect(screen.getAllByText('Built-in check').length).toBeGreaterThan(0)

    await user.click(screen.getByText('Send a credential to an unknown host'))
    const dialog = await screen.findByRole('dialog')
    expect(within(dialog).getByText(why)).toBeInTheDocument()
    expect(within(dialog).getByText('Built-in check')).toBeInTheDocument()
    expect(within(dialog).queryByRole('button', { name: 'Copy as a custom rule' })).not.toBeInTheDocument()
    await user.keyboard('{Escape}')

    await user.click(screen.getByText('Download and run'))
    expect(
      await within(await screen.findByRole('dialog')).findByRole('button', { name: 'Copy as a custom rule' }),
    ).toBeInTheDocument()
  })

  it('tries a redaction pattern under its placeholder name and saves one that only records', async () => {
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    await user.click(await screen.findByRole('button', { name: 'New rule' }))
    const dialog = await screen.findByRole('dialog')
    // Text as written, ignoring case, unless the rule says otherwise.
    expect(within(dialog).getByRole('radio', { name: 'Contains' })).toBeChecked()
    expect(within(dialog).queryByRole('radio', { name: 'Code points' })).not.toBeInTheDocument()
    expect(within(dialog).getByRole('switch', { name: 'Case-sensitive' })).not.toBeChecked()
    expect(within(dialog).getByRole('radio', { name: 'Replace with placeholder' })).toBeChecked()
    await user.type(within(dialog).getByLabelText('Text to find'), 'PRJ-')
    await user.type(within(dialog).getByLabelText('Test text'), 'see prj-12')
    await waitFor(() =>
      expect(api).toHaveBeenCalledWith(
        '/api/admin/security/redact/test',
        expect.objectContaining({
          body: {
            sample: 'see prj-12',
            pattern: 'PRJ-',
            match: 'contains',
            case_sensitive: false,
            label: 'SECRET',
            action: 'placeholder',
          },
        }),
      ),
    )

    await user.click(within(dialog).getByRole('radio', { name: 'Regex' }))
    await user.click(within(dialog).getByRole('switch', { name: 'Case-sensitive' }))
    await user.type(within(dialog).getByLabelText('Regular expression'), '\\d+')
    await user.clear(within(dialog).getByLabelText('Placeholder name'))
    await user.type(within(dialog).getByLabelText('Placeholder name'), 'PROJECT')

    await waitFor(() =>
      expect(api).toHaveBeenCalledWith(
        '/api/admin/security/redact/test',
        expect.objectContaining({
          body: {
            sample: 'see prj-12',
            pattern: 'PRJ-\\d+',
            match: 'regex',
            case_sensitive: true,
            label: 'PROJECT',
            action: 'placeholder',
          },
        }),
      ),
    )

    await user.type(within(dialog).getByLabelText('Name'), 'Project')
    await user.click(within(dialog).getByRole('radio', { name: 'Record only' }))
    await user.click(within(dialog).getByRole('button', { name: 'Create' }))
    await waitFor(() =>
      expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
        settings: {
          'security.redact': {
            mode: 'observe',
            custom: [
              {
                name: 'Project',
                pattern: 'PRJ-\\d+',
                match: 'regex',
                case_sensitive: true,
                label: 'PROJECT',
                action: 'record',
              },
            ],
          },
        },
      }),
    )
  })

  it('keeps a refused rule in its dialog with the reason', async () => {
    const user = userEvent.setup()
    vi.mocked(apiPatch).mockRejectedValueOnce(new Error('regex parse error: unclosed group'))
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    await user.click(await screen.findByRole('button', { name: 'New rule' }))
    const dialog = await screen.findByRole('dialog')
    await user.type(within(dialog).getByLabelText('Name'), 'Broken')
    await user.click(within(dialog).getByRole('radio', { name: 'Regex' }))
    await user.type(within(dialog).getByLabelText('Regular expression'), 'PRJ-(')
    await user.click(within(dialog).getByRole('button', { name: 'Create' }))

    expect(await within(dialog).findByText(/unclosed group/)).toBeInTheDocument()
    expect(apiPatch).toHaveBeenCalledWith('/api/admin/settings', {
      settings: {
        'security.redact': {
          mode: 'observe',
          custom: [{ name: 'Broken', pattern: 'PRJ-(', match: 'regex', label: 'SECRET' }],
        },
      },
    })
  })

  it('offers no writes without the write permission', async () => {
    vi.mocked(hasPermission).mockImplementation((perm: string) => !perm.endsWith(':write'))
    const user = userEvent.setup()
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    expect(await screen.findByRole('switch', { name: 'Turn on "Email address"' })).toBeDisabled()
    expect(screen.getByRole('button', { name: 'New rule' })).toBeDisabled()
    expect(screen.getByRole('radio', { name: 'On' })).toBeDisabled()
    expect(screen.getByRole('checkbox', { name: 'Turn every rule in "AI services" on or off' })).toBeDisabled()
    // The action is said, not offered.
    expect(screen.queryByRole('combobox')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Test…' })).toBeEnabled()
  })
  it('lists the exception values under the redaction rules, and adds and removes one', async () => {
    const user = userEvent.setup()
    vi.mocked(apiPost).mockResolvedValue({ ...exception, sha256: 'b'.repeat(64), masked: 'ghp_B…BBBB' })
    vi.mocked(apiDelete).mockResolvedValue(exception)
    exceptions = { items: [exception], max: 500 }
    renderWithQueryClient(<GatewaySecurityPage />)

    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    expect(await screen.findByText('Exceptions · 1')).toBeInTheDocument()
    expect(screen.getByText('sk-an…AAAA')).toBeInTheDocument()
    // The rule is named from the rule view.
    expect(screen.getAllByText('Anthropic API key').length).toBeGreaterThan(1)

    await user.click(screen.getByRole('button', { name: 'Add exception' }))
    const dialog = await screen.findByRole('dialog')
    expect(within(dialog).getByText(/will no longer be replaced, nor recorded/)).toBeInTheDocument()
    await user.type(within(dialog).getByLabelText('Value'), 'ghp_test')
    await user.click(within(dialog).getByRole('button', { name: 'Add exception' }))
    await waitFor(() =>
      expect(apiPost).toHaveBeenCalledWith('/api/admin/security/redact/exceptions', { value: 'ghp_test' }),
    )

    await user.click(screen.getByRole('button', { name: 'Remove' }))
    const confirm = await screen.findByRole('dialog')
    await user.click(within(confirm).getByRole('button', { name: 'Remove' }))
    await waitFor(() =>
      expect(apiDelete).toHaveBeenCalledWith(`/api/admin/security/redact/exceptions/${'a'.repeat(64)}`),
    )
  })

  it('offers no adding at the cap, and one line when there are none', async () => {
    const user = userEvent.setup()
    exceptions = { items: [exception], max: 1 }
    const { unmount } = renderWithQueryClient(<GatewaySecurityPage />)
    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    expect(await screen.findByText(/There are 1 exceptions, the most there may be/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Add exception' })).toBeDisabled()
    unmount()

    exceptions = { items: [], max: 500 }
    renderWithQueryClient(<GatewaySecurityPage />)
    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    expect(await screen.findByText(/^No exceptions\./)).toBeInTheDocument()
    expect(screen.queryByText(/^Exceptions ·/)).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'Add exception' })).toBeEnabled()
  })

  it('says in the trial that an exception value is left as is', async () => {
    const user = userEvent.setup()
    vi.mocked(api).mockImplementation(async (url: string) => {
      if (url === '/api/admin/security') return detail()
      if (url === '/api/admin/security/redact/exceptions') return exceptions
      if (url.startsWith('/api/audit/logs?')) return events
      if (url.endsWith('/test'))
        return {
          hits: [{ rule: 'anthropic-api-key', custom: false, start: 4, end: 14, excerpt: 'sk-an…AAAA', excepted: true }],
          output: null,
          refused: false,
        }
      throw new Error(`unexpected request: ${url}`)
    })
    renderWithQueryClient(<GatewaySecurityPage />)
    await user.click(await screen.findByRole('tab', { name: /Secrets & privacy/ }))
    await user.click(await screen.findByRole('button', { name: 'Test…' }))
    const dialog = await screen.findByRole('dialog')
    await user.type(within(dialog).getByLabelText('Test text'), 'key sk-ant-xyz')
    expect(await within(dialog).findByText('Exception, left as is')).toBeInTheDocument()
    expect(within(dialog).getByText('Exceptions left as is: 1')).toBeInTheDocument()
    expect(within(dialog).getByText('No match')).toBeInTheDocument()
  })
})
