import { describe, expect, it } from 'vitest'
import type { GuardDetail, SecurityRuleView } from '@/lib/security-types'
import { kindName, ruleAbout, ruleName, ruleWhy, viewName } from './names'

const rule = (over: Partial<SecurityRuleView>): SecurityRuleView => ({
  id: 'groq-key',
  custom: false,
  name: 'Groq API key',
  about: 'Starts with `gsk_`',
  kind: 'ai-services',
  matcher: { kind: 'prefix', prefix: 'gsk_', min_tail: 20 },
  enabled: true,
  on_by_default: true,
  ...over,
})

const detail: GuardDetail = {
  mode: 'observe',
  rules: [rule({})],
  kinds: [{ slug: 'ai-services', name: 'AI services' }],
}

describe('client-owned rule translations', () => {
  it('is shown in the console language, English where no Chinese is given', () => {
    expect(viewName('zh', 'redact', rule({}))).toBe('Groq API 密钥')
    expect(viewName('en', 'redact', rule({}))).toBe('Groq API key')
    expect(viewName('zh', 'redact', rule({ id: 'new-key' }))).toBe('Groq API key')
    expect(ruleAbout('zh', 'redact', rule({}))).toBe('`gsk_` 开头，其后至少 20 个字符')
    expect(ruleWhy('zh', 'content', rule({ id: 'bidi-controls', why: 'Because' }))).toBe('可使屏幕上的显示顺序与实际字符顺序不一致。')
    expect(kindName('zh', 'redact', detail.kinds, 'ai-services')).toBe('AI 服务')
    expect(kindName('en', 'redact', detail.kinds, 'unknown')).toBe('unknown')
  })

  it('names a custom rule by its own name, and a log row by id', () => {
    expect(viewName('zh', 'redact', rule({ id: 'Codename', custom: true, name: 'Codename' }))).toBe('Codename')
    expect(ruleName('zh', 'redact', detail, 'groq-key', false)).toBe('Groq API 密钥')
    expect(ruleName('zh', 'redact', detail, 'gone-rule', false, 'Gone rule')).toBe('Gone rule')
    expect(ruleName('zh', 'redact', undefined, 'gone-rule', false)).toBe('gone-rule')
  })

  it('resolves a shared ID separately for each guard', () => {
    const entry = rule({ id: 'you-are-now' })
    expect(viewName('zh', 'inspect_tools', entry)).toBe('重新设定模型身份')
    expect(viewName('zh', 'content', entry)).toBe('重新设定身份')
  })
})
