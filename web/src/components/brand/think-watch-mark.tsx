/**
 * ThinkWatch wordmark icon — three flows entering a shielded eye whose pupil
 * is an AI data packet (rotated square), one trusted flow exiting on the
 * right. Uses `currentColor` so it adapts to whatever container color it's
 * placed in (sidebar header, login screen, setup wizard, etc.).
 *
 * The viewBox is tight (no padding) so the icon fills its container.
 */
export function ThinkWatchMark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 20" fill="none" className={className} aria-hidden="true">
      {/* shield outline */}
      <rect
        x="6"
        y="2"
        width="20"
        height="16"
        rx="5"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
      />
      {/* three inflow stripes (left) */}
      <line x1="0" y1="6"  x2="6" y2="6"  stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
      <line x1="0" y1="10" x2="6" y2="10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
      <line x1="0" y1="14" x2="6" y2="14" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
      {/* single trusted outflow (right) */}
      <line x1="26" y1="10" x2="32" y2="10" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
      {/* AI packet pupil */}
      <rect
        x="12.5"
        y="6.5"
        width="7"
        height="7"
        rx="0.8"
        fill="currentColor"
        transform="rotate(45 16 10)"
      />
    </svg>
  );
}
