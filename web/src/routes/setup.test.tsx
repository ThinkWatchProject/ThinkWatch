import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { SetupPage } from './setup'

// SetupPage dynamically imports @/lib/api once init succeeds to register
// the freshly-generated ECDSA key pair. Both crypto.subtle.generateKey
// and indexedDB are unavailable / unstable in jsdom, so the original
// registerKeyPair() rejects and the page never advances to the complete
// step. Stubbing it (and invalidateSetupStatusCache, which the same
// import block touches) keeps the happy-path tests focused on the UI
// flow, not the auth-handshake plumbing.
vi.mock('@/lib/api', () => ({
  registerKeyPair: vi.fn().mockResolvedValue(undefined),
  invalidateSetupStatusCache: vi.fn(),
  API_BASE: '',
}))

beforeEach(() => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
    ok: true,
    json: () => Promise.resolve({ initialized: false, needs_setup: true }),
  }))
})

describe('SetupPage', () => {
  it('renders welcome step on mount', () => {
    render(<SetupPage />)

    expect(screen.getByText('Welcome to ThinkWatch')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /get started/i })).toBeInTheDocument()
  })

  it('navigates to admin step after clicking Get Started', async () => {
    const user = userEvent.setup()
    render(<SetupPage />)

    await user.click(screen.getByRole('button', { name: /get started/i }))

    expect(screen.getByText('Create Admin Account')).toBeInTheDocument()
    expect(screen.getByLabelText(/email/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/display name/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/^password\b/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/confirm password/i)).toBeInTheDocument()
  })

  it('validates password length', async () => {
    const user = userEvent.setup()
    render(<SetupPage />)

    // Navigate to admin step
    await user.click(screen.getByRole('button', { name: /get started/i }))

    // Fill in required fields with short password
    await user.type(screen.getByLabelText(/email/i), 'admin@test.com')
    await user.type(screen.getByLabelText(/display name/i), 'Admin')
    await user.type(screen.getByLabelText(/^password\b/i), 'short')
    await user.type(screen.getByLabelText(/confirm password/i), 'short')

    // Click Next
    await user.click(screen.getByRole('button', { name: /next/i }))

    expect(screen.getByText('Password must be at least 8 characters')).toBeInTheDocument()
  })

  it('validates password match', async () => {
    const user = userEvent.setup()
    render(<SetupPage />)

    await user.click(screen.getByRole('button', { name: /get started/i }))

    await user.type(screen.getByLabelText(/email/i), 'admin@test.com')
    await user.type(screen.getByLabelText(/display name/i), 'Admin')
    await user.type(screen.getByLabelText(/^password\b/i), 'password123')
    await user.type(screen.getByLabelText(/confirm password/i), 'password456')

    await user.click(screen.getByRole('button', { name: /next/i }))

    expect(screen.getByText('Passwords do not match')).toBeInTheDocument()
  })

  it('renders complete step with API key after successful setup', async () => {
    const user = userEvent.setup()

    const mockFetch = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({
        admin_id: '123',
        admin_email: 'admin@test.com',
        api_key: 'tw-test-key-xyz789',
        message: 'Setup complete',
      }),
    })
    vi.stubGlobal('fetch', mockFetch)

    render(<SetupPage />)

    // Welcome -> Admin
    await user.click(screen.getByRole('button', { name: /get started/i }))

    // Fill admin form -> Next
    await user.type(screen.getByLabelText(/email/i), 'admin@test.com')
    await user.type(screen.getByLabelText(/display name/i), 'Admin')
    await user.type(screen.getByLabelText(/^password\b/i), 'Password123')
    await user.type(screen.getByLabelText(/confirm password/i), 'Password123')
    await user.click(screen.getByRole('button', { name: /next/i }))

    // Settings step is the final input step — submit completes setup.
    expect(screen.getByText('Site Settings')).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /finish setup/i }))

    await waitFor(() => {
      expect(screen.getByText('Setup Complete!')).toBeInTheDocument()
    })
    expect(screen.getByDisplayValue('tw-test-key-xyz789')).toBeInTheDocument()
  })
})
