import { describe, expect, it } from 'vitest'
import { liteDownloadUrl, liteImportLink, liteLimits, type KeyRateRule } from './lite-link'

const KEY = 'tw-0123456789abcdef'

describe('liteImportLink', () => {
  it('builds the enterprise link with each value encoded', () => {
    expect(liteImportLink('https://gateway.example.com', KEY)).toEqual({
      link:
        'thinkwatch://import?service=thinkwatch&name=thinkwatch' +
        '&url=https%3A%2F%2Fgateway.example.com&key=tw-0123456789abcdef',
    })
    const withPort = liteImportLink('https://gateway.example.com:8443', 'tw-a+b/c=')
    expect(withPort).toEqual({
      link:
        'thinkwatch://import?service=thinkwatch&name=thinkwatch' +
        '&url=https%3A%2F%2Fgateway.example.com%3A8443&key=tw-a%2Bb%2Fc%3D',
    })
  })

  it('takes plain http on this machine and on private networks only', () => {
    const allowed = [
      // This machine.
      'http://localhost:3000',
      'http://127.0.0.1:3000',
      'http://[::1]:3000',
      // Private IPv4 ranges, at their edges.
      'http://10.0.0.5:3000',
      'http://10.255.255.255',
      'http://172.16.0.1:3000',
      'http://172.31.255.254',
      'http://192.168.1.10',
      'http://100.64.0.1',
      'http://100.127.255.254',
      'http://169.254.10.20',
      // Private IPv6 ranges.
      'http://[fd00::5]:3000',
      'http://[fc00::1]',
      'http://[fe80::1]',
      'http://[febf::1]',
      // Intranet-style names.
      'http://corp',
      'http://gateway:3000',
      'http://example.corp',
      'http://gw.example.local',
      'http://gw.lan',
      'http://gw.internal:8080',
      'http://gw.intranet',
      'http://gw.home.arpa',
      'http://gw.localhost',
    ]
    for (const url of allowed) {
      expect(liteImportLink(url, KEY), url).toHaveProperty('link')
    }
    const refused = [
      'http://172.15.255.254',
      'http://172.32.0.1',
      'http://100.63.255.254',
      'http://100.128.0.1',
      'http://192.169.0.1',
      'http://169.253.0.1',
      'http://11.0.0.1',
      'http://127.0.0.2',
      'http://8.8.8.8',
      'http://[2001:db8::1]',
      'http://[fec0::1]',
      'http://example.com',
      'http://gw.example.com:3000',
      'http://corp.example.com',
      'http://gw.locals',
    ]
    for (const url of refused) {
      expect(liteImportLink(url, KEY), url).toEqual({ problem: 'insecure-url' })
    }
    // https is taken whatever the host.
    expect(liteImportLink('https://example.com', KEY)).toHaveProperty('link')
    expect(liteImportLink('https://8.8.8.8', KEY)).toHaveProperty('link')
  })

  it('refuses anything but a bare scheme://host[:port]', () => {
    for (const url of [
      'https://gateway.example.com/',
      'https://gateway.example.com/v1',
      'https://gateway.example.com?x=1',
      'https://gateway.example.com#top',
      'https://user:pass@gateway.example.com',
      'https://GATEWAY.example.com',
      'ftp://gateway.example.com',
      'gateway.example.com',
      '',
    ]) {
      expect(liteImportLink(url, KEY), url).toEqual({ problem: 'bad-url' })
    }
  })

  it('refuses a key with characters Lite does not take', () => {
    expect(liteImportLink('https://gateway.example.com', 'tw-${HOME}')).toEqual({
      problem: 'bad-key',
    })
    expect(liteImportLink('https://gateway.example.com', '')).toEqual({ problem: 'bad-key' })
    expect(liteImportLink('https://gateway.example.com', 'x'.repeat(513))).toEqual({
      problem: 'bad-key',
    })
  })
})

describe('liteImportLink with limits', () => {
  it('adds limits as one encoded value, `|` as %7C and `;` as %3B', () => {
    expect(
      liteImportLink('https://gateway.example.com', KEY, [
        'minute|requests|60',
        'hour|requests|500|5',
      ]),
    ).toEqual({
      link:
        'thinkwatch://import?service=thinkwatch&name=thinkwatch' +
        '&url=https%3A%2F%2Fgateway.example.com&key=tw-0123456789abcdef' +
        '&limits=minute%7Crequests%7C60%3Bhour%7Crequests%7C500%7C5',
    })
  })

  it('leaves the parameter out when there are no limits', () => {
    expect(liteImportLink('https://gateway.example.com', KEY, [])).toEqual(
      liteImportLink('https://gateway.example.com', KEY),
    )
    expect(liteImportLink('https://gateway.example.com', KEY)).not.toHaveProperty(
      'link',
      expect.stringContaining('limits='),
    )
  })
})

const NOW = Date.parse('2026-10-20T12:00:00Z')

const rule = (overrides: Partial<KeyRateRule> = {}): KeyRateRule => ({
  surface: 'ai_gateway',
  metric: 'requests',
  window_secs: 60,
  max_count: 100,
  enabled: true,
  expires_at: null,
  ...overrides,
})

describe('liteLimits', () => {
  it("maps each of the console's windows that Lite has", () => {
    const cases: [number, string][] = [
      [60, 'minute|requests|7'],
      [300, 'minute|requests|7|5'],
      [3600, 'hour|requests|7'],
      [18000, 'hour|requests|7|5'],
      [86400, 'hour|requests|7|24'],
    ]
    for (const [window_secs, entry] of cases) {
      expect(liteLimits([rule({ window_secs, max_count: 7 })], NOW), String(window_secs)).toEqual([
        entry,
      ])
    }
  })

  it('maps any other window of whole minutes up to 60 or whole hours up to 24', () => {
    expect(liteLimits([rule({ window_secs: 120 })], NOW)).toEqual(['minute|requests|100|2'])
    expect(liteLimits([rule({ window_secs: 1800 })], NOW)).toEqual(['minute|requests|100|30'])
    expect(liteLimits([rule({ window_secs: 7200 })], NOW)).toEqual(['hour|requests|100|2'])
    expect(liteLimits([rule({ window_secs: 43200 })], NOW)).toEqual(['hour|requests|100|12'])
  })

  it('leaves out windows Lite has no rolling counterpart for', () => {
    for (const window_secs of [604800, 30, 90, 5400, 90000, 172800, 0, -60, 60.5]) {
      expect(liteLimits([rule({ window_secs })], NOW), String(window_secs)).toEqual([])
    }
  })

  it('leaves out token rules, which count weighted tokens', () => {
    expect(liteLimits([rule({ metric: 'tokens', max_count: 100000 })], NOW)).toEqual([])
  })

  it('takes only enabled, unexpired AI-gateway rules', () => {
    expect(
      liteLimits(
        [
          rule({ enabled: false, max_count: 1 }),
          rule({ surface: 'mcp_gateway', max_count: 2 }),
          rule({ expires_at: '2026-10-20T11:59:59Z', max_count: 3 }),
          rule({ expires_at: '2026-10-20T12:00:00Z', max_count: 4 }),
          rule({ window_secs: 3600, expires_at: '2026-10-21T00:00:00Z', max_count: 5 }),
        ],
        NOW,
      ),
    ).toEqual(['hour|requests|5'])
  })

  it('leaves out counts Lite does not take', () => {
    expect(liteLimits([rule({ max_count: 0 })], NOW)).toEqual([])
    expect(liteLimits([rule({ max_count: 1.5 })], NOW)).toEqual([])
    expect(liteLimits([rule({ max_count: 1_000_000_000_000_000 })], NOW)).toEqual([])
    expect(liteLimits([rule({ max_count: 999_999_999_999_999 })], NOW)).toEqual([
      'minute|requests|999999999999999',
    ])
  })

  it('keeps the smaller count of two rules on one window', () => {
    expect(
      liteLimits([rule({ max_count: 50 }), rule({ max_count: 20 }), rule({ max_count: 80 })], NOW),
    ).toEqual(['minute|requests|20'])
  })

  it('orders entries by window, shortest first', () => {
    expect(
      liteLimits(
        [
          rule({ window_secs: 86400, max_count: 5000 }),
          rule({ window_secs: 60, max_count: 10 }),
          rule({ window_secs: 3600, max_count: 300 }),
        ],
        NOW,
      ),
    ).toEqual(['minute|requests|10', 'hour|requests|300', 'hour|requests|5000|24'])
  })

  it('keeps the 16 windows with the lowest rate when there are more', () => {
    // 20 windows of 1–20 minutes; the n-minute one allows 10 + n requests,
    // so the rate falls as n grows and the longest 16 are the tightest.
    const rules = Array.from({ length: 20 }, (_, i) =>
      rule({ window_secs: 60 * (i + 1), max_count: 11 + i }),
    )
    const out = liteLimits(rules, NOW)
    expect(out).toHaveLength(16)
    expect(out[0]).toBe('minute|requests|15|5')
    expect(out[15]).toBe('minute|requests|30|20')
  })

  it('gives nothing when no rule maps', () => {
    expect(liteLimits([], NOW)).toEqual([])
  })
})

describe('liteDownloadUrl', () => {
  it('follows the console language', () => {
    expect(liteDownloadUrl('zh')).toBe('https://thinkwat.ch/zh-CN/lite')
    expect(liteDownloadUrl('en')).toBe('https://thinkwat.ch/lite')
  })
})
