import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { LoginPage } from './login'

// Mock fetch for SSO status
beforeEach(() => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
    json: () => Promise.resolve({ enabled: false }),
  }))
})

describe('LoginPage', () => {
  it('renders email and password inputs', () => {
    render(<LoginPage onLogin={vi.fn()} />)

    expect(screen.getByLabelText(/email/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/password/i)).toBeInTheDocument()
  })

  it('renders sign in button', () => {
    render(<LoginPage onLogin={vi.fn()} />)

    expect(screen.getByRole('button', { name: /sign in$/i })).toBeInTheDocument()
  })

  it('renders SSO button', () => {
    render(<LoginPage onLogin={vi.fn()} />)

    expect(screen.getByRole('button', { name: /sso/i })).toBeInTheDocument()
  })

  it('renders register link', () => {
    render(<LoginPage onLogin={vi.fn()} />)

    expect(screen.getByText(/don't have an account/i)).toBeInTheDocument()
  })

  it('calls onLogin with email and password on submit', async () => {
    const user = userEvent.setup()
    const onLogin = vi.fn().mockResolvedValue({})
    render(<LoginPage onLogin={onLogin} />)

    await user.type(screen.getByLabelText(/email/i), 'test@example.com')
    await user.type(screen.getByLabelText(/password/i), 'secretpass')
    await user.click(screen.getByRole('button', { name: /sign in$/i }))

    expect(onLogin).toHaveBeenCalledWith('test@example.com', 'secretpass', undefined)
  })

  it('displays error when login fails', async () => {
    const user = userEvent.setup()
    const onLogin = vi.fn().mockRejectedValue(new Error('Invalid credentials'))
    render(<LoginPage onLogin={onLogin} />)

    await user.type(screen.getByLabelText(/email/i), 'test@example.com')
    await user.type(screen.getByLabelText(/password/i), 'wrongpass')
    await user.click(screen.getByRole('button', { name: /sign in$/i }))

    expect(await screen.findByText('Invalid credentials')).toBeInTheDocument()
  })

  it('disables submit button while loading', async () => {
    const user = userEvent.setup()
    // Never-resolving promise to keep loading state
    const onLogin = vi.fn().mockReturnValue(new Promise(() => {}))
    render(<LoginPage onLogin={onLogin} />)

    await user.type(screen.getByLabelText(/email/i), 'test@example.com')
    await user.type(screen.getByLabelText(/password/i), 'pass1234')
    await user.click(screen.getByRole('button', { name: /sign in$/i }))

    expect(screen.getByRole('button', { name: /signing in/i })).toBeDisabled()
  })

  it('SSO button is disabled when SSO is not enabled', () => {
    render(<LoginPage onLogin={vi.fn()} />)

    expect(screen.getByRole('button', { name: /sso/i })).toBeDisabled()
  })
})
