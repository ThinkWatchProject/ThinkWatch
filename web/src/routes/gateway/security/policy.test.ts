import { describe, expect, it } from 'vitest'
import type { GuardDetail, SecurityRuleView } from '@/lib/security-types'
import {
  checkCodepoints,
  customRuleView,
  isLabel,
  placeholder,
  policyOf,
  withAction,
  withCustom,
  withEnabled,
  withMode,
  withoutCustom,
} from './policy'

const builtin = (over: Partial<SecurityRuleView> & Pick<SecurityRuleView, 'id'>): SecurityRuleView => ({
  custom: false,
  name: over.id,
  kind: 'injection',
  matcher: { kind: 'contains', text: over.id },
  enabled: true,
  on_by_default: true,
  ...over,
})

const content: GuardDetail = {
  mode: 'observe',
  rules: [
    builtin({
      id: 'unicode-tags',
      kind: 'invisible',
      matcher: { kind: 'codepoints', ranges: ['U+E0000–U+E007F'] },
      action: 'strip',
      default_action: 'strip',
    }),
    builtin({ id: 'zero-width', kind: 'invisible', on_by_default: false, action: 'strip', default_action: 'strip' }),
    builtin({ id: 'jailbreak', on_by_default: false, enabled: false, action: 'block', default_action: 'block' }),
    builtin({ id: 'ignore-previous-instructions', enabled: false, action: 'block', default_action: 'block' }),
    builtin({ id: 'act-as', kind: 'persona', on_by_default: false, enabled: false, action: 'record', default_action: 'block' }),
    customRuleView({
      name: 'Project X',
      pattern: 'project-x',
      match: 'contains',
      action: 'strip',
      label: null,
      enabled: true,
    }),
    customRuleView({
      name: 'Tags',
      pattern: 'U+E0000–U+E007F, U+200B',
      match: 'codepoints',
      action: 'block',
      label: null,
      enabled: false,
    }),
  ],
}

describe('policyOf', () => {
  it('writes only what differs from the factory, and every custom rule', () => {
    expect(policyOf('content', content)).toEqual({
      mode: 'observe',
      enable: ['zero-width'],
      disable: ['ignore-previous-instructions'],
      // A rule that is off still keeps the action it was given.
      actions: { 'act-as': 'record' },
      custom: [
        { name: 'Project X', pattern: 'project-x', match: 'contains', action: 'strip' },
        {
          name: 'Tags',
          pattern: 'U+E0000–U+E007F, U+200B',
          match: 'codepoints',
          action: 'block',
          disabled: true,
        },
      ],
    })
  })

  it('leaves out empty lists', () => {
    const d: GuardDetail = { mode: 'enforce', rules: [builtin({ id: 'jailbreak', action: 'block', default_action: 'block' })] }
    expect(policyOf('content', d)).toEqual({ mode: 'enforce' })
  })

  it('gives every custom redaction rule its placeholder label', () => {
    const d: GuardDetail = {
      mode: 'enforce',
      rules: [
        builtin({ id: 'internal-ip', kind: 'internal', matcher: { kind: 'private-ip' }, on_by_default: false, label: 'SECRET' }),
        builtin({ id: 'jwt', kind: 'jwt', matcher: { kind: 'jwt' }, enabled: false, label: 'SECRET' }),
        customRuleView({ name: 'Project', pattern: 'PRJ-\\d{6}', match: 'regex', action: null, label: 'PROJECT', enabled: true }),
        { ...customRuleView({ name: 'Ticket', pattern: 'T-\\d+', match: 'regex', action: null, label: null, enabled: true }), label: null },
      ],
    }
    expect(policyOf('redact', d)).toEqual({
      mode: 'enforce',
      enable: ['internal-ip'],
      disable: ['jwt'],
      custom: [
        { name: 'Project', pattern: 'PRJ-\\d{6}', label: 'PROJECT' },
        { name: 'Ticket', pattern: 'T-\\d+', label: 'SECRET' },
      ],
    })
  })

  it('writes tool-call actions and custom rules', () => {
    const d: GuardDetail = {
      mode: 'observe',
      rules: [
        builtin({ id: 'rm-rf-root', kind: 'command', matcher: { kind: 'regex', pattern: 'rm' }, action: 'record', default_action: 'cut' }),
        builtin({ id: 'chmod-777', kind: 'command', matcher: { kind: 'regex', pattern: 'chmod' }, action: 'cut', default_action: 'cut' }),
        customRuleView({ name: 'kubectl', pattern: 'kubectl\\s+delete', match: 'regex', action: 'cut', label: null, enabled: true }),
      ],
    }
    expect(policyOf('inspect_tools', d)).toEqual({
      mode: 'observe',
      actions: { 'rm-rf-root': 'record' },
      custom: [{ name: 'kubectl', pattern: 'kubectl\\s+delete', action: 'cut' }],
    })
  })
})

describe('edits', () => {
  it('switch, act and set the mode on the view', () => {
    let d = withMode(content, 'enforce')
    d = withEnabled(d, { id: 'jailbreak', custom: false }, true)
    d = withAction(d, 'unicode-tags', 'block')
    expect(policyOf('content', d)).toMatchObject({
      mode: 'enforce',
      enable: ['zero-width', 'jailbreak'],
      actions: { 'act-as': 'record', 'unicode-tags': 'block' },
    })
  })

  it('switches a custom rule without touching a built-in one of the same name', () => {
    const d: GuardDetail = {
      mode: 'observe',
      rules: [
        builtin({ id: 'dan', action: 'block', default_action: 'block' }),
        customRuleView({ name: 'dan', pattern: 'dan', match: 'contains', action: 'record', label: null, enabled: true }),
      ],
    }
    const next = withEnabled(d, { id: 'dan', custom: true }, false)
    expect(next.rules.map((r) => r.enabled)).toEqual([true, false])
  })

  it('renames a custom rule in place and adds new ones last', () => {
    const renamed = withCustom(
      content,
      'Project X',
      customRuleView({ name: 'Project Y', pattern: 'project-y', match: 'regex', action: 'block', label: null, enabled: true }),
    )
    expect(renamed.rules.filter((r) => r.custom).map((r) => r.id)).toEqual(['Project Y', 'Tags'])

    const added = withCustom(
      content,
      null,
      customRuleView({ name: 'New', pattern: 'x', match: 'contains', action: 'record', label: null, enabled: true }),
    )
    expect(added.rules.at(-1)?.id).toBe('New')
    expect(withoutCustom(added, 'New').rules).toHaveLength(content.rules.length)
  })
})

describe('checkCodepoints', () => {
  it('takes single code points and ranges, in any of the separators', () => {
    expect(checkCodepoints('U+200B')).toBeNull()
    expect(checkCodepoints('u+200b')).toBeNull()
    expect(checkCodepoints('U+E0000–U+E007F')).toBeNull()
    expect(checkCodepoints('U+0041-U+005A, U+200B、U+FEFF\nU+2060，U+10FFFF')).toBeNull()
  })

  it('names the first item that is wrong', () => {
    expect(checkCodepoints('U+200B, 200C')).toEqual({ kind: 'bad', item: '200C' })
    expect(checkCodepoints('U+D800')).toEqual({ kind: 'bad', item: 'U+D800' })
    expect(checkCodepoints('U+110000')).toEqual({ kind: 'bad', item: 'U+110000' })
    expect(checkCodepoints('U+1234567')).toEqual({ kind: 'bad', item: 'U+1234567' })
    expect(checkCodepoints('U+0042-U+0041')).toEqual({ kind: 'bad', item: 'U+0042-U+0041' })
  })

  it('wants at least one and at most 32 items', () => {
    expect(checkCodepoints(' , ')).toEqual({ kind: 'empty' })
    const many = Array.from({ length: 33 }, (_, i) => `U+${(0x41 + i).toString(16)}`).join(',')
    expect(checkCodepoints(many)).toEqual({ kind: 'tooMany' })
  })
})

describe('placeholder labels', () => {
  it('are capitals, digits and underscores, starting with a letter, 24 at most', () => {
    expect(isLabel('SECRET')).toBe(true)
    expect(isLabel('PROJECT_1')).toBe(true)
    expect(isLabel('A'.repeat(24))).toBe(true)
    expect(isLabel('A'.repeat(25))).toBe(false)
    expect(isLabel('1ABC')).toBe(false)
    expect(isLabel('project')).toBe(false)
    expect(isLabel('')).toBe(false)
  })

  it('show what a hit becomes', () => {
    expect(placeholder('EMAIL')).toBe('<<TW_EMAIL_1>>')
    expect(placeholder(null)).toBe('<<TW_SECRET_1>>')
  })
})
