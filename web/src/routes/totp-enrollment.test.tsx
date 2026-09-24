import { describe, it, expect, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { TotpEnrollmentPage } from './totp-enrollment'

describe('TotpEnrollmentPage', () => {
  it('offers enrollment and signing out, and nothing else', async () => {
    const onLogout = vi.fn()
    render(<TotpEnrollmentPage email="person@example.com" onEnrolled={vi.fn()} onLogout={onLogout} />)

    expect(screen.getByText(/person@example\.com/)).toBeInTheDocument()
    const buttons = screen.getAllByRole('button').map((b) => b.textContent)
    expect(buttons).toHaveLength(2)
    expect(screen.getByRole('button', { name: /enable 2fa/i })).toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: /logout/i }))
    expect(onLogout).toHaveBeenCalledTimes(1)
  })
})
