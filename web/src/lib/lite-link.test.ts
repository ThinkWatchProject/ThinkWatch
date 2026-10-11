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

describe('liteDownloadUrl', () => {
  it('follows the console language', () => {
    expect(liteDownloadUrl('zh')).toBe('https://thinkwat.ch/zh-CN/lite')
    expect(liteDownloadUrl('en')).toBe('https://thinkwat.ch/lite')
  })
})
