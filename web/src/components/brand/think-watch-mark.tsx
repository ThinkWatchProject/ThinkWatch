/**
 * ThinkWatch wordmark icon — three flows entering a shielded eye whose pupil
 * is an AI data packet (rotated square), one trusted flow exiting on the
 * right. Uses `currentColor` so it adapts to whatever container color it's
 * placed in (sidebar header, login screen, setup wizard, etc.).
 */
export function ThinkWatchMark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" fill="none" className={className} aria-hidden="true">
      {/* shield outline */}
      <rect
        x="6"
        y="9"
        width="20"
        height="14"
        rx="4"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
      />
      {/* three inflow stripes (left) */}
      <line x1="2" y1="13" x2="6" y2="13" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      <line x1="2" y1="16" x2="6" y2="16" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      <line x1="2" y1="19" x2="6" y2="19" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      {/* single trusted outflow (right) */}
      <line x1="26" y1="16" x2="30" y2="16" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      {/* AI packet pupil */}
      <rect
        x="13"
        y="13"
        width="6"
        height="6"
        rx="0.6"
        fill="currentColor"
        transform="rotate(45 16 16)"
      />
    </svg>
  );
}
