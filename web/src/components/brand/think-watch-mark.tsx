/**
 * ThinkWatch wordmark icon — a TW monogram inside a rounded shield.
 *
 *   - Outer rounded square: the gateway / audit boundary ("Watch")
 *   - T glyph on top: horizontal bar + central stem
 *   - W glyph on bottom: two stacked V's that share the T's stem as their
 *     vertical axis
 *
 * Reads as a "TW" stamp at any size and as a stylized seal at distance.
 * Uses `currentColor` so it inherits its container color.
 */
export function ThinkWatchMark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" fill="none" className={className} aria-hidden="true">
      {/* Rounded shield outline — the audit boundary */}
      <rect
        x="3"
        y="3"
        width="26"
        height="26"
        rx="6"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
      />

      {/* T — horizontal bar */}
      <line
        x1="9"
        y1="10"
        x2="23"
        y2="10"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinecap="round"
      />
      {/* T — central stem (also serves as the W axis) */}
      <line
        x1="16"
        y1="10"
        x2="16"
        y2="16"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinecap="round"
      />

      {/* W — left V (top of W) */}
      <polyline
        points="9,16 12,22 16,16"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      {/* W — right V */}
      <polyline
        points="16,16 20,22 23,16"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
