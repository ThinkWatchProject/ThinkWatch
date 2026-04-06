import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { SetupPage } from './setup'

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
    expect(screen.getByLabelText(/^password$/i)).toBeInTheDocument()
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
    await user.type(screen.getByLabelText(/^password$/i), 'short')
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
    await user.type(screen.getByLabelText(/^password$/i), 'password123')
    await user.type(screen.getByLabelText(/confirm password/i), 'password456')

    await user.click(screen.getByRole('button', { name: /next/i }))

    expect(screen.getByText('Passwords do not match')).toBeInTheDocument()
  })

  it('can skip provider step', async () => {
    const user = userEvent.setup()

    const mockFetch = vi.fn().mockResolvedValue({
      ok: true,
      json: () => Promise.resolve({
        admin_id: '123',
        admin_email: 'admin@test.com',
        api_key: 'tw-test-key-123',
        message: 'Setup complete',
      }),
    })
    vi.stubGlobal('fetch', mockFetch)

    render(<SetupPage />)

    // Welcome -> Admin
    await user.click(screen.getByRole('button', { name: /get started/i }))

    // Fill admin form
    await user.type(screen.getByLabelText(/email/i), 'admin@test.com')
    await user.type(screen.getByLabelText(/display name/i), 'Admin')
    await user.type(screen.getByLabelText(/^password$/i), 'Password123')
    await user.type(screen.getByLabelText(/confirm password/i), 'Password123')
    await user.click(screen.getByRole('button', { name: /next/i }))

    // Settings step -> Next
    expect(screen.getByText('Site Settings')).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /next/i }))

    // Provider step -> Skip
    expect(screen.getByText('Add First Provider')).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /skip for now/i }))

    // Should move to complete step
    await waitFor(() => {
      expect(screen.getByText('Setup Complete!')).toBeInTheDocument()
    })
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

    // Navigate through all steps
    await user.click(screen.getByRole('button', { name: /get started/i }))

    await user.type(screen.getByLabelText(/email/i), 'admin@test.com')
    await user.type(screen.getByLabelText(/display name/i), 'Admin')
    await user.type(screen.getByLabelText(/^password$/i), 'Password123')
    await user.type(screen.getByLabelText(/confirm password/i), 'Password123')
    await user.click(screen.getByRole('button', { name: /next/i }))

    // Settings
    await user.click(screen.getByRole('button', { name: /next/i }))

    // Provider -> Skip
    await user.click(screen.getByRole('button', { name: /skip for now/i }))

    // Verify complete step with API key displayed
    await waitFor(() => {
      expect(screen.getByText('Setup Complete!')).toBeInTheDocument()
    })
    expect(screen.getByDisplayValue('tw-test-key-xyz789')).toBeInTheDocument()
  })
})
