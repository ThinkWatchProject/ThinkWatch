import { describe, expect, it } from 'vitest'
import { render } from '@testing-library/react'
import type { Matcher } from '@/lib/security-types'
import { MatcherText } from './matcher-text'

const text = (m: Matcher) => render(<MatcherText m={m} />).container.textContent

describe('MatcherText', () => {
  it('groups prefixes by what must follow them', () => {
    expect(
      text({
        kind: 'prefixes',
        prefixes: [
          { prefix: 'sk-svcacct-', min_tail: 58, tail: 'any' },
          { prefix: 'sk-admin-', min_tail: 58, tail: 'any' },
        ],
      }),
    ).toBe('Starts with sk-svcacct- or sk-admin-, followed by at least 58 characters')
    expect(
      text({
        kind: 'prefixes',
        prefixes: [
          { prefix: 'sntrys_', min_tail: 40, tail: 'any' },
          { prefix: 'sntryu_', min_tail: 64, tail: 'hex' },
        ],
      }),
    ).toBe(
      'Starts with sntrys_, followed by at least 40 characters; or sntryu_, followed by at least 64 hex digits',
    )
  })

  it('describes assignments, field names, saved keys and checks in code', () => {
    expect(text({ kind: 'assignment', suffixes: ['PASSWORD', 'TOKEN'] })).toBe(
      'The literal value assigned to a name ending in PASSWORD or TOKEN; only the value is replaced',
    )
    expect(text({ kind: 'labeled', names: ['real_name', '姓名'] })).toBe('The value of a field named real_name or 姓名')
    expect(text({ kind: 'saved' })).toBe('The provider API keys saved in ThinkWatch, by value, whatever their format')
    expect(text({ kind: 'builtin', check: 'crypto-extended-key' })).toBe(
      'Starts with xprv, tprv, yprv or zprv and passes Base58Check',
    )
  })
})
