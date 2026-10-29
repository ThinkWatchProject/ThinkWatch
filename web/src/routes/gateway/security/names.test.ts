import { describe, expect, it } from 'vitest'
import type { GuardDetail, SecurityRuleView } from '@/lib/security-types'
import { kindName, ruleAbout, ruleName, ruleWhy, viewName } from './names'

const rule = (over: Partial<SecurityRuleView>): SecurityRuleView => ({
  id: 'groq-key',
  custom: false,
  name: 'Groq API key',
  name_zh: 'Groq API 密钥',
  about: 'Starts with `gsk_`',
  about_zh: '`gsk_` 开头',
  kind: 'ai-services',
  matcher: { kind: 'builtin', check: 'groq-key' },
  enabled: true,
  on_by_default: true,
  ...over,
})

const detail: GuardDetail = {
  mode: 'observe',
  rules: [rule({})],
  kinds: [{ slug: 'ai-services', name: 'AI services', name_zh: 'AI 服务' }],
}

describe('rule text from the view', () => {
  it('is shown in the console language, English where no Chinese is given', () => {
    expect(viewName('zh', rule({}))).toBe('Groq API 密钥')
    expect(viewName('en', rule({}))).toBe('Groq API key')
    expect(viewName('zh', rule({ name_zh: undefined }))).toBe('Groq API key')
    expect(ruleAbout('zh', rule({}))).toBe('`gsk_` 开头')
    expect(ruleWhy('zh', rule({ why: 'Because', why_zh: '因为' }))).toBe('因为')
    expect(kindName('zh', detail.kinds, 'ai-services')).toBe('AI 服务')
    expect(kindName('en', detail.kinds, 'unknown')).toBe('unknown')
  })

  it('names a custom rule by its own name, and a log row by id', () => {
    expect(viewName('zh', rule({ id: 'Codename', custom: true, name: 'Codename' }))).toBe('Codename')
    expect(ruleName('zh', detail, 'groq-key', false)).toBe('Groq API 密钥')
    expect(ruleName('zh', detail, 'gone-rule', false, 'Gone rule')).toBe('Gone rule')
    expect(ruleName('zh', undefined, 'gone-rule', false)).toBe('gone-rule')
  })
})
