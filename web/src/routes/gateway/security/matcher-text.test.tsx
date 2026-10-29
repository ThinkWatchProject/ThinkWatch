import { describe, expect, it } from 'vitest'
import { render } from '@testing-library/react'
import { About, MatcherText } from './matcher-text'

describe('About', () => {
  it('shows backticked text as code, without the backticks', () => {
    const { container } = render(<About text="Starts with `sk-ant-`, followed by at least 20 characters" />)
    expect(container.textContent).toBe('Starts with sk-ant-, followed by at least 20 characters')
    expect(container.querySelector('code')?.textContent).toBe('sk-ant-')
  })
})

describe('MatcherText', () => {
  it('describes a pattern, and a check in code as a built-in check', () => {
    const text = (m: Parameters<typeof MatcherText>[0]['m']) => render(<MatcherText m={m} />).container.textContent
    expect(text({ kind: 'regex', pattern: 'a+', case_sensitive: true })).toBe('Regex a+, case-sensitive')
    expect(text({ kind: 'builtin', check: 'credential-to-network' })).toBe('Built-in check')
  })
})
