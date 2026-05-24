import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { LoginPage } from './login'

// Mock fetch for SSO status. The login page now reads
// `allow_registration` from the same payload to decide whether to
// show the "Don't have an account? Register" link, so the mock must
// include that flag for the register-link test to pass.
beforeEach(() => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue({
    json: () => Promise.resolve({ enabled: false, allow_registration: true }),
  }))
})

// Stub the PoW hook out so tests don't need a real Web Worker (Vitest's
// jsdom doesn't ship one) and don't try to hit /api/auth/pow-challenge.
// Returning `status: 'ready'` immediately means the submit button stays
// "Sign in" without waiting for a grind.
vi.mock('@/hooks/use-pow-challenge', () => ({
  usePowChallenge: () => ({
    status: 'ready',
    solution: { challenge_id: 'test-challenge', nonce: '0' },
    tried: 0,
    difficulty: 19,
    error: null,
    // Far-future expiry so submit-time staleness check passes.
    expiresAt: Date.now() + 60_000,
    refresh: vi.fn(),
  }),
}))

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

  it('hides SSO button when SSO is disabled', () => {
    render(<LoginPage onLogin={vi.fn()} />)

    expect(screen.queryByRole('button', { name: /sso/i })).not.toBeInTheDocument()
  })

  it('renders register link', () => {
    render(<LoginPage onLogin={vi.fn()} />)

    expect(screen.getByText(/don't have an account/i)).toBeInTheDocument()
  })

  it('calls onLogin with email, password, totp, and pow on submit', async () => {
    const user = userEvent.setup()
    const onLogin = vi.fn().mockResolvedValue({})
    render(<LoginPage onLogin={onLogin} />)

    await user.type(screen.getByLabelText(/email/i), 'test@example.com')
    await user.type(screen.getByLabelText(/password/i), 'secretpass')
    await user.click(screen.getByRole('button', { name: /sign in$/i }))

    expect(onLogin).toHaveBeenCalledWith(
      'test@example.com',
      'secretpass',
      undefined,
      { challenge_id: 'test-challenge', nonce: '0' },
    )
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

})
