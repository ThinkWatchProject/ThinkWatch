import { afterEach, describe, expect, it } from 'vitest'
import { openAppLink } from './open-app-link'

const frames = () => Array.from(document.querySelectorAll('iframe'))

afterEach(() => {
  for (const f of frames()) f.remove()
})

describe('openAppLink', () => {
  it('loads the link in a hidden iframe, not in the page', () => {
    const before = window.location.href
    openAppLink('thinkwatch://import?service=thinkwatch&key=tw-a')

    expect(window.location.href).toBe(before)
    const [frame] = frames()
    expect(frame).toBeDefined()
    expect(frame.getAttribute('src')).toBe('thinkwatch://import?service=thinkwatch&key=tw-a')
    expect(frame.hidden).toBe(true)
    expect(frame).toHaveAttribute('aria-hidden', 'true')
    expect(frame.tabIndex).toBe(-1)
  })

  it('replaces the iframe of an earlier call', () => {
    openAppLink('thinkwatch://import?key=first')
    openAppLink('thinkwatch://import?key=second')

    expect(frames().map((f) => f.getAttribute('src'))).toEqual(['thinkwatch://import?key=second'])
  })
})
