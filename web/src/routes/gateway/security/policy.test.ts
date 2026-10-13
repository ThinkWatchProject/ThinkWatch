import { describe, expect, it } from 'vitest'
import type { GuardDetail, SecurityRuleView } from '@/lib/security-types'
import {
  actionAllowed,
  checkCodepoints,
  choiceOfScope,
  customRuleView,
  groupState,
  isLabel,
  matchAllowed,
  matchesSearch,
  patternOf,
  placeholder,
  policyOf,
  ruleScope,
  scopeOfChoice,
  withAction,
  withCustom,
  withEnabled,
  withEnabledMany,
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
      caseSensitive: false,
      action: 'strip',
      label: null,
      enabled: true,
    }),
    customRuleView({
      name: 'Tags',
      pattern: 'U+E0000–U+E007F, U+200B',
      match: 'codepoints',
      caseSensitive: false,
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
        customRuleView({ name: 'Project', pattern: 'PRJ-\\d{6}', match: 'regex', caseSensitive: true, action: 'placeholder', label: 'PROJECT', enabled: true }),
        { ...customRuleView({ name: 'Ticket', pattern: 'T-\\d+', match: 'regex', caseSensitive: false, action: 'record', label: null, enabled: true }), label: null },
        customRuleView({ name: 'Codename', pattern: 'blue.fox', match: 'contains', caseSensitive: false, action: 'placeholder', label: null, enabled: false }),
      ],
    }
    expect(policyOf('redact', d)).toEqual({
      mode: 'enforce',
      enable: ['internal-ip'],
      disable: ['jwt'],
      custom: [
        { name: 'Project', pattern: 'PRJ-\\d{6}', match: 'regex', case_sensitive: true, label: 'PROJECT' },
        // Only a rule that records says so: replacing is the default.
        { name: 'Ticket', pattern: 'T-\\d+', match: 'regex', label: 'SECRET', action: 'record' },
        { name: 'Codename', pattern: 'blue.fox', match: 'contains', label: 'SECRET', disabled: true },
      ],
    })
  })

  it('reads how a custom redaction rule matches back from the view', () => {
    const view = (matcher: SecurityRuleView['matcher']): SecurityRuleView => ({
      ...customRuleView({ name: 'r', pattern: '', match: 'contains', caseSensitive: false, action: 'placeholder', label: null, enabled: true }),
      matcher,
    })
    expect(patternOf(view({ kind: 'contains', text: 'a.b' }))).toEqual({ pattern: 'a.b', match: 'contains', caseSensitive: false })
    expect(patternOf(view({ kind: 'regex', pattern: 'a+', case_sensitive: true }))).toEqual({
      pattern: 'a+',
      match: 'regex',
      caseSensitive: true,
    })
    expect(customRuleView({ name: 'r', pattern: 'x', match: 'regex', caseSensitive: true, action: 'placeholder', label: null, enabled: true }).matcher).toEqual({
      kind: 'regex',
      pattern: 'x',
      case_sensitive: true,
    })
  })

  it('writes tool-call actions and custom rules', () => {
    const d: GuardDetail = {
      mode: 'observe',
      rules: [
        builtin({ id: 'rm-rf-root', kind: 'command', matcher: { kind: 'regex', pattern: 'rm' }, action: 'record', default_action: 'cut' }),
        builtin({ id: 'chmod-777', kind: 'command', matcher: { kind: 'regex', pattern: 'chmod' }, action: 'cut', default_action: 'cut' }),
        customRuleView({ name: 'kubectl', pattern: 'kubectl\\s+delete', match: 'regex', caseSensitive: false, action: 'cut', label: null, enabled: true }),
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
    d = withAction(d, { id: 'unicode-tags', custom: false }, 'block')
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
        customRuleView({ name: 'dan', pattern: 'dan', match: 'contains', caseSensitive: false, action: 'record', label: null, enabled: true }),
      ],
    }
    const next = withEnabled(d, { id: 'dan', custom: true }, false)
    expect(next.rules.map((r) => r.enabled)).toEqual([true, false])
  })

  it('renames a custom rule in place and adds new ones last', () => {
    const renamed = withCustom(
      content,
      'Project X',
      customRuleView({ name: 'Project Y', pattern: 'project-y', match: 'regex', caseSensitive: false, action: 'block', label: null, enabled: true }),
    )
    expect(renamed.rules.filter((r) => r.custom).map((r) => r.id)).toEqual(['Project Y', 'Tags'])

    const added = withCustom(
      content,
      null,
      customRuleView({ name: 'New', pattern: 'x', match: 'contains', caseSensitive: false, action: 'record', label: null, enabled: true }),
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

describe('the restructured guards', () => {
  it('reads a stored policy without the new fields as before: requests only, ignoring case, placeholders', () => {
    const old = customRuleView({
      name: 'Old',
      pattern: 'x',
      match: 'contains',
      caseSensitive: false,
      action: 'strip',
      label: null,
      enabled: true,
    })
    expect(ruleScope({})).toEqual(['request'])
    expect(ruleScope(old)).toEqual(['request'])
    expect(policyOf('content', { mode: 'observe', rules: [old] })).toEqual({
      mode: 'observe',
      custom: [{ name: 'Old', pattern: 'x', match: 'contains', action: 'strip' }],
    })
  })

  it('writes what a content rule checks, its case rule and its replacement, defaults left out', () => {
    const rule = (over: Partial<Parameters<typeof customRuleView>[0]>) =>
      customRuleView({
        name: 'Codename',
        pattern: 'Polaris',
        match: 'contains',
        caseSensitive: false,
        action: 'replace',
        label: null,
        scope: ['request', 'response'],
        replacement: '***',
        enabled: true,
        ...over,
      })
    expect(policyOf('content', { mode: 'enforce', rules: [rule({})] })).toEqual({
      mode: 'enforce',
      custom: [{ name: 'Codename', pattern: 'Polaris', scope: ['request', 'response'], match: 'contains', action: 'replace' }],
    })
    expect(
      policyOf('content', {
        mode: 'enforce',
        rules: [rule({ scope: ['response'], caseSensitive: true, replacement: '[hidden]' })],
      }).custom,
    ).toEqual([
      {
        name: 'Codename',
        pattern: 'Polaris',
        scope: ['response'],
        match: 'contains',
        case_sensitive: true,
        action: 'replace',
        replacement: '[hidden]',
      },
    ])
    // A replacement only goes with the replace action.
    expect(
      policyOf('content', { mode: 'enforce', rules: [rule({ action: 'strip', replacement: '[hidden]' })] }).custom?.[0],
    ).not.toHaveProperty('replacement')
  })

  it('writes the redaction actions that differ from the factory', () => {
    const d: GuardDetail = {
      mode: 'enforce',
      rules: [
        builtin({ id: 'email', kind: 'personal', matcher: { kind: 'email' }, action: 'record', default_action: 'placeholder' }),
        builtin({ id: 'jwt', kind: 'jwt', matcher: { kind: 'jwt' }, action: 'placeholder', default_action: 'placeholder' }),
      ],
    }
    expect(policyOf('redact', d)).toEqual({ mode: 'enforce', actions: { email: 'record' } })
    expect(policyOf('redact', withAction(d, { id: 'email', custom: false }, 'placeholder'))).toEqual({ mode: 'enforce' })
  })

  it('switches a group at once and says when it is partly on', () => {
    const invisible = content.rules.filter((r) => r.kind === 'invisible')
    expect(groupState(invisible)).toBe(true)
    const off = withEnabledMany(content, invisible, false)
    expect(groupState(off.rules.filter((r) => r.kind === 'invisible'))).toBe(false)
    // zero-width ships off and was on: now it is simply not enabled.
    expect(policyOf('content', off)).toMatchObject({ disable: ['unicode-tags', 'ignore-previous-instructions'] })
    expect(policyOf('content', off).enable).toBeUndefined()
    expect(groupState(content.rules.filter((r) => r.kind === 'injection'))).toBe(false)
    expect(groupState(content.rules.filter((r) => r.custom))).toBe('indeterminate')
  })

  it('gives a custom rule switched to replace the default text', () => {
    const d = withAction(content, { id: 'Project X', custom: true }, 'replace')
    expect(d.rules.find((r) => r.id === 'Project X')).toMatchObject({ action: 'replace', replacement: '***' })
  })

  it('rules out a refusal and code points for answers', () => {
    expect(actionAllowed('block', ['request'])).toBe(true)
    expect(actionAllowed('block', ['response'])).toBe(false)
    expect(actionAllowed('block', ['request', 'response'])).toBe(false)
    expect(actionAllowed('replace', ['response'])).toBe(true)
    expect(matchAllowed('codepoints', ['request'])).toBe(true)
    expect(matchAllowed('codepoints', ['request', 'response'])).toBe(false)
    expect(matchAllowed('regex', ['response'])).toBe(true)
    expect(scopeOfChoice('both')).toEqual(['request', 'response'])
    expect(choiceOfScope(['response', 'request'])).toBe('both')
    expect(choiceOfScope(['response'])).toBe('response')
  })

  it('searches by name and by what a rule matches', () => {
    const tags = content.rules[0]
    expect(matchesSearch(tags, 'Unicode tag characters', 'tag')).toBe(true)
    expect(matchesSearch(tags, 'Unicode 标签字符', '标签')).toBe(true)
    const custom = content.rules.find((r) => r.id === 'Project X')!
    expect(matchesSearch(custom, 'Project X', 'PROJECT-x')).toBe(true)
    expect(matchesSearch(custom, 'Project X', 'nothing')).toBe(false)
    expect(matchesSearch(custom, 'Project X', '  ')).toBe(true)
  })
})
