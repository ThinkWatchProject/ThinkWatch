import { useEffect, useState } from "react"
import { Toaster as Sonner, type ToasterProps } from "sonner"
import { CircleCheckIcon, InfoIcon, TriangleAlertIcon, OctagonXIcon, Loader2Icon } from "lucide-react"

/**
 * Sonner doesn't observe our app's theme system on its own — we
 * apply `class="dark"` to <html> via `useTheme`, but sonner only
 * inspects `prefers-color-scheme`. Without a subscription the
 * toaster (and its action buttons) keep rendering in light mode,
 * which collides with success/info/warning colored backgrounds in
 * dark mode (the action pill ends up cream/white against a dark
 * green success bg).
 *
 * Subscribe to the html class via MutationObserver and pass the
 * resolved theme into sonner so the action button + base surface
 * stay consistent with the rest of the UI.
 */
function useResolvedTheme(): "light" | "dark" {
  const [resolved, setResolved] = useState<"light" | "dark">(() =>
    typeof document !== "undefined" &&
    document.documentElement.classList.contains("dark")
      ? "dark"
      : "light",
  )
  useEffect(() => {
    if (typeof document === "undefined") return
    const html = document.documentElement
    const update = () =>
      setResolved(html.classList.contains("dark") ? "dark" : "light")
    update()
    const obs = new MutationObserver(update)
    obs.observe(html, { attributes: true, attributeFilter: ["class"] })
    return () => obs.disconnect()
  }, [])
  return resolved
}

const Toaster = ({ ...props }: ToasterProps) => {
  const theme = useResolvedTheme()
  return (
    <Sonner
      theme={theme}
      className="toaster group"
      icons={{
        success: <CircleCheckIcon className="size-4" />,
        info: <InfoIcon className="size-4" />,
        warning: <TriangleAlertIcon className="size-4" />,
        error: <OctagonXIcon className="size-4" />,
        loading: <Loader2Icon className="size-4 animate-spin" />,
      }}
      style={
        {
          "--normal-bg": "var(--popover)",
          "--normal-text": "var(--popover-foreground)",
          "--normal-border": "var(--border)",
          "--border-radius": "var(--radius)",
        } as React.CSSProperties
      }
      toastOptions={{
        classNames: {
          toast: "cn-toast",
        },
      }}
      {...props}
    />
  )
}

export { Toaster }
