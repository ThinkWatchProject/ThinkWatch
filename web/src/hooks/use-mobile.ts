import * as React from "react"

const MOBILE_BREAKPOINT = 768

export function useIsMobile() {
  // Seeded in the initialiser rather than an effect. The original started
  // `undefined` and corrected itself after mount, so the first paint was
  // always the desktop layout — a visible snap on phones.
  const [isMobile, setIsMobile] = React.useState(
    () => window.innerWidth < MOBILE_BREAKPOINT,
  )

  React.useEffect(() => {
    const mql = window.matchMedia(`(max-width: ${MOBILE_BREAKPOINT - 1}px)`)
    const onChange = () => {
      setIsMobile(window.innerWidth < MOBILE_BREAKPOINT)
    }
    mql.addEventListener("change", onChange)
    return () => mql.removeEventListener("change", onChange)
  }, [])

  return isMobile
}
