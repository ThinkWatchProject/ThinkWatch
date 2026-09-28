import { describe, it, expect, vi, beforeAll, beforeEach } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent, { type UserEvent } from '@testing-library/user-event'
import { CreateProviderDialog, EditProviderDialog } from './provider-dialogs'
import type { Provider } from './provider-types'

vi.mock('@/lib/api', () => ({
  apiPost: vi.fn(),
  apiPatch: vi.fn(),
}))

import { apiPost, apiPatch } from '@/lib/api'

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

beforeEach(() => {
  vi.clearAllMocks()
})

async function pick(user: UserEvent, label: string, option: string) {
  await user.click(screen.getByLabelText(label))
  await user.click(await screen.findByRole('option', { name: option }))
}

function renderCreate() {
  render(<CreateProviderDialog open onOpenChange={vi.fn()} onSuccess={vi.fn(async () => {})} />)
}

function bedrock(headers: { key: string; value: string; encrypted?: boolean }[]): Provider {
  return {
    id: 'prov-1',
    name: 'bedrock',
    display_name: 'Bedrock',
    provider_type: 'bedrock',
    base_url: 'us-east-1',
    is_active: true,
    config_json: { headers },
    created_at: '2026-09-28T00:00:00Z',
  }
}

function renderEdit(provider: Provider) {
  render(
    <EditProviderDialog open onOpenChange={vi.fn()} provider={provider} onSuccess={vi.fn(async () => {})} />,
  )
}

describe('CreateProviderDialog', () => {
  it('creates a Bedrock provider that authenticates with an API key', async () => {
    const user = userEvent.setup()
    renderCreate()

    await user.type(screen.getByLabelText('Name'), 'bedrock-prod')
    await pick(user, 'Provider Type', 'AWS Bedrock')
    // No key, no way to authenticate the test: signing isn't set up yet.
    expect(screen.queryByRole('button', { name: 'Test Connection' })).not.toBeInTheDocument()
    await pick(user, 'Authentication Mode', 'Bedrock API Key')
    await user.type(screen.getByLabelText('API Key'), 'ABSK-test')
    expect(screen.getByRole('button', { name: 'Test Connection' })).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: 'Create Provider' }))

    // The key is a bearer token in the headers, and there are no AWS keys.
    expect(apiPost).toHaveBeenCalledWith('/api/admin/providers', {
      name: 'bedrock-prod',
      display_name: '',
      provider_type: 'bedrock',
      base_url: 'us-east-1',
      headers: [{ key: 'Authorization', value: 'Bearer ABSK-test' }],
    })
  })

  // A Bedrock provider that sends `Authorization` is never signed, so a
  // key left behind would quietly override the access keys.
  it('drops the key when the provider switches to access keys', async () => {
    const user = userEvent.setup()
    renderCreate()

    await user.type(screen.getByLabelText('Name'), 'bedrock-prod')
    await pick(user, 'Provider Type', 'AWS Bedrock')
    await pick(user, 'Authentication Mode', 'Bedrock API Key')
    await user.type(screen.getByLabelText('API Key'), 'ABSK-test')
    await pick(user, 'Authentication Mode', 'Access Key / Secret Key')
    await user.type(screen.getByLabelText('Access Key ID'), 'AKIA-test')
    await user.type(screen.getByLabelText('Secret Access Key'), 'secret')
    await user.click(screen.getByRole('button', { name: 'Create Provider' }))

    expect(apiPost).toHaveBeenCalledWith(
      '/api/admin/providers',
      expect.objectContaining({
        headers: [],
        config: { aws_access_key_id: 'AKIA-test', aws_secret_access_key: 'secret' },
      }),
    )
  })
})

describe('EditProviderDialog', () => {
  it('replaces a saved Bedrock API key', async () => {
    const user = userEvent.setup()
    renderEdit(bedrock([{ key: 'Authorization', value: '', encrypted: true }]))

    expect(screen.getByLabelText('AWS Region')).toHaveValue('us-east-1')
    expect(screen.getByRole('button', { name: 'Test Connection' })).toBeInTheDocument()
    const key = screen.getByLabelText('API Key')
    expect(key).toHaveAttribute('placeholder', 'Saved — leave blank to keep')
    await user.type(key, 'ABSK-new')
    await user.click(screen.getByRole('button', { name: 'Save' }))

    expect(apiPatch).toHaveBeenCalledWith('/api/admin/providers/prov-1', {
      display_name: 'Bedrock',
      base_url: 'us-east-1',
      headers: [{ key: 'Authorization', value: 'Bearer ABSK-new' }],
    })
  })

  // A blank value is what keeps the saved key; a bare "Bearer " would
  // replace it with nothing.
  it('keeps the saved key when the field is typed in and cleared again', async () => {
    const user = userEvent.setup()
    renderEdit(bedrock([{ key: 'Authorization', value: '', encrypted: true }]))

    const key = screen.getByLabelText('API Key')
    await user.type(key, 'x')
    await user.clear(key)
    await user.click(screen.getByRole('button', { name: 'Save' }))

    expect(apiPatch).toHaveBeenCalledWith(
      '/api/admin/providers/prov-1',
      expect.objectContaining({ headers: [{ key: 'Authorization', value: '' }] }),
    )
  })

  it('offers no key field or test for a Bedrock provider that signs its requests', () => {
    renderEdit(bedrock([]))

    expect(screen.queryByLabelText('API Key')).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Test Connection' })).not.toBeInTheDocument()
  })
})
