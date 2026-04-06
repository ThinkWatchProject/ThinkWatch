/**
 * ThinkWatch wordmark icon — three flows entering a shielded eye whose pupil
 * is an AI data packet (rotated square), one trusted flow exiting on the
 * right. Uses `currentColor` so it adapts to whatever container color it's
 * placed in (sidebar header, login screen, setup wizard, etc.).
 *
 * Designed on a 32×32 grid that the content actually fills edge-to-edge,
 * so the icon scales up cleanly to fit any square container.
 */
export function ThinkWatchMark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" fill="none" className={className} aria-hidden="true">
      {/* shield outline — fills most of the canvas */}
      <rect
        x="6"
        y="6"
        width="20"
        height="20"
        rx="5"
        fill="none"
        stroke="currentColor"
        strokeWidth="2.4"
      />
      {/* three inflow stripes (left edge) */}
      <line x1="0" y1="11" x2="6" y2="11" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      <line x1="0" y1="16" x2="6" y2="16" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      <line x1="0" y1="21" x2="6" y2="21" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      {/* single trusted outflow (right edge) */}
      <line x1="26" y1="16" x2="32" y2="16" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      {/* AI packet pupil — rotated square at the center */}
      <rect
        x="11.5"
        y="11.5"
        width="9"
        height="9"
        rx="1"
        fill="currentColor"
        transform="rotate(45 16 16)"
      />
    </svg>
  );
}
