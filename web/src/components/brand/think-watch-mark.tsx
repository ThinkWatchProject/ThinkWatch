/**
 * ThinkWatch wordmark icon — an almond-shaped eye whose pupil is the
 * lowercase letter `w`. Reads as "watchful brand mark":
 *   - Top + bottom arcs form the eyelid contour (Watch)
 *   - The central `w` glyph is the brand initial (Think_W_atch)
 *
 * Uses `currentColor` so it inherits its container color.
 */
export function ThinkWatchMark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" fill="none" className={className} aria-hidden="true">
      {/* Eye outline — two symmetric arcs forming an almond */}
      <path
        d="M3 16 Q 16 5, 29 16 Q 16 27, 3 16 Z"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinejoin="round"
      />

      {/* Lowercase `w` as the pupil */}
      <polyline
        points="9,13 12,20 16,15 20,20 23,13"
        fill="none"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
