/**
 * ThinkWatch wordmark icon — three flows entering a shielded eye whose pupil
 * is an AI data packet (rotated square), one trusted flow exiting on the
 * right. Uses `currentColor` so it adapts to whatever container color it's
 * placed in (sidebar header, login screen, setup wizard, etc.).
 */
export function ThinkWatchMark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" fill="none" className={className} aria-hidden="true">
      {/* three inflow stripes (left side) — kept inside viewBox so the
          rounded line caps don't get clipped at the edge */}
      <line x1="1.5" y1="11" x2="6" y2="11"
            stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" />
      <line x1="1.5" y1="16" x2="6" y2="16"
            stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" />
      <line x1="1.5" y1="21" x2="6" y2="21"
            stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" />

      {/* single trusted outflow (right side) */}
      <line x1="26" y1="16" x2="30.5" y2="16"
            stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" />

      {/* shield outline — drawn on top so the inflow lines tuck behind it */}
      <rect x="6" y="6" width="20" height="20" rx="5"
            fill="none" stroke="currentColor" strokeWidth="2.4" />

      {/* AI packet pupil — rotated square at the center */}
      <rect x="11.5" y="11.5" width="9" height="9" rx="1"
            fill="currentColor" transform="rotate(45 16 16)" />
    </svg>
  );
}
