import { describe, expect, it } from 'vitest'
import { gatewayBaseUrl, type GatewayInfo } from './use-gateway-url'

const unset: GatewayInfo = {
  public_protocol: null,
  public_host: null,
  public_port: null,
  gateway_port: 3000,
}
const page = (url: string) => new URL(url)

describe('gatewayBaseUrl', () => {
  it('uses what is configured, leaving a default port out', () => {
    const at = page('http://localhost:5173/guide')
    expect(
      gatewayBaseUrl(
        { public_protocol: 'https', public_host: 'gw.example.com', public_port: 443, gateway_port: 3000 },
        at,
      ),
    ).toBe('https://gw.example.com')
    expect(
      gatewayBaseUrl(
        { public_protocol: 'https', public_host: 'gw.example.com', public_port: 8443, gateway_port: 3000 },
        at,
      ),
    ).toBe('https://gw.example.com:8443')
    expect(
      gatewayBaseUrl({ ...unset, public_protocol: 'http', public_host: '10.0.0.5' }, at),
    ).toBe('http://10.0.0.5:3000')
    expect(
      gatewayBaseUrl({ ...unset, public_protocol: 'http', public_host: 'gw.internal', public_port: 80 }, at),
    ).toBe('http://gw.internal')
  })

  it('takes what is not set from the address the console is open at', () => {
    expect(gatewayBaseUrl(unset, page('https://tw.example.com/guide'))).toBe(
      'https://tw.example.com:3000',
    )
    expect(gatewayBaseUrl({ ...unset, public_port: 443 }, page('https://tw.example.com/'))).toBe(
      'https://tw.example.com',
    )
    // The gateway's own port, not the console's.
    expect(gatewayBaseUrl({ ...unset, gateway_port: 3100 }, page('http://192.168.1.4:3001/'))).toBe(
      'http://192.168.1.4:3100',
    )
    expect(gatewayBaseUrl(unset, page('http://[::1]:3001/'))).toBe('http://[::1]:3000')
    // A configured host with the browser's scheme.
    expect(gatewayBaseUrl({ ...unset, public_host: 'gw.example.com' }, page('https://c.example.com/'))).toBe(
      'https://gw.example.com:3000',
    )
  })

  it('is made from the browser alone until the configuration is known', () => {
    expect(gatewayBaseUrl(undefined, page('https://tw.example.com/guide'))).toBe(
      'https://tw.example.com:3000',
    )
    expect(gatewayBaseUrl(undefined, page('http://localhost:5173/'))).toBe('http://localhost:3000')
    expect(gatewayBaseUrl(undefined, { protocol: 'file:', hostname: '' })).toBe('http://localhost:3000')
  })
})
