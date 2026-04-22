import '@testing-library/jest-dom/vitest'
import '../i18n'

// jsdom doesn't ship ResizeObserver, but @radix-ui's Popover (used
// inside several scope-picker components) calls it during layout.
// A no-op stub keeps Popover-bearing components mountable in tests.
if (typeof globalThis.ResizeObserver === 'undefined') {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver
}
