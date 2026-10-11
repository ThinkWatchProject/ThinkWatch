import { describe, expect, it } from 'vitest'
import { liteDownloadUrl, liteImportLink } from './lite-link'

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

  it('takes plain http only on this machine', () => {
    for (const local of ['http://localhost:3000', 'http://127.0.0.1:3000', 'http://[::1]:3000']) {
      expect(liteImportLink(local, KEY)).toHaveProperty('link')
    }
    expect(liteImportLink('http://10.0.0.5:3000', KEY)).toEqual({ problem: 'insecure-url' })
    expect(liteImportLink('http://gateway.internal', KEY)).toEqual({ problem: 'insecure-url' })
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

describe('liteDownloadUrl', () => {
  it('follows the console language', () => {
    expect(liteDownloadUrl('zh')).toBe('https://thinkwat.ch/zh-CN/lite')
    expect(liteDownloadUrl('en')).toBe('https://thinkwat.ch/lite')
  })
})
