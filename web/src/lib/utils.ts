import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/**
 * Return the input only if it parses to an `http:` or `https:` URL.
 * Use this at every `<a href={...}>` and `window.open(...)` site where
 * the URL came from admin/user input — `javascript:` and `data:`
 * URLs in an href execute in the user's session if clicked, and
 * `rel="noopener noreferrer"` does NOT block them. Returns `null` so
 * callers can conditionally render the link.
 *
 * The malformed-URL throw path also returns `null` (no link), which
 * is the right failure mode for any field where "no link" beats
 * "broken link or attack vector."
 */
export function safeExternalHref(raw: string | null | undefined): string | null {
  if (!raw) return null
  try {
    const parsed = new URL(raw)
    if (parsed.protocol === 'http:' || parsed.protocol === 'https:') {
      return parsed.toString()
    }
  } catch {
    // Falls through to null
  }
  return null
}
