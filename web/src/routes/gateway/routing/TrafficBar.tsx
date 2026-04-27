import { useCallback, useEffect, useRef, useState } from 'react';

interface Segment {
  id: string;
  /// Display label (provider + label or upstream model). Tooltip + visible.
  label: string;
  /// Stored weight (integer). Caller pre-filters to enabled-only routes.
  weight: number;
  /// Tailwind background color classes — caller assigns deterministically
  /// so the same route always gets the same hue.
  colorClass: string;
}

interface Props {
  segments: Segment[];
  /// Disable drag handles when admin lacks write permission or the
  /// model is in auto mode.
  disabled?: boolean;
  /// Fired on pointer release with the new weight set. Caller persists
  /// via `PATCH /api/admin/model-routes/batch-weights`.
  onCommit: (updates: { id: string; weight: number }[]) => void;
}

/// Horizontal bar split into segments by weight. Each segment-segment
/// boundary is a draggable handle: dragging right grows the left
/// segment's weight at the right segment's expense (zero-sum within
/// the pair). On pointer release we commit the final weights.
///
/// Why pair-only? Multi-segment redistribute is conceptually nice but
/// hard to make intuitive: dragging segment 2's right edge "should"
/// affect segment 3, not segment 4. Pair-only matches the visual
/// "I'm dragging this boundary" mental model and is what the AWS / GCP
/// load-balancer admin UIs do.
export function TrafficBar({ segments, disabled, onCommit }: Props) {
  // Local copy so dragging is smooth (we re-render at 60fps without
  // round-tripping through the parent). Synced from props on each
  // non-dragging update.
  const [draftWeights, setDraftWeights] = useState<number[]>(
    segments.map((s) => s.weight),
  );
  const draggingRef = useRef(false);

  useEffect(() => {
    if (!draggingRef.current) {
      setDraftWeights(segments.map((s) => s.weight));
    }
  }, [segments]);

  const containerRef = useRef<HTMLDivElement>(null);
  const total = draftWeights.reduce((a, b) => a + b, 0) || 1;

  const startDrag = useCallback(
    (boundaryIndex: number) => (e: React.PointerEvent) => {
      if (disabled) return;
      e.preventDefault();
      const container = containerRef.current;
      if (!container) return;
      draggingRef.current = true;
      // Capture pointer so we keep getting move events even if the
      // cursor leaves the bar (browser default).
      (e.target as HTMLElement).setPointerCapture(e.pointerId);

      const rect = container.getBoundingClientRect();
      const initial = [...draftWeights];
      const pairTotal = initial[boundaryIndex] + initial[boundaryIndex + 1];

      const onMove = (ev: PointerEvent) => {
        const x = ev.clientX - rect.left;
        const beforePx = initial
          .slice(0, boundaryIndex)
          .reduce((sum, w) => sum + (w / total) * rect.width, 0);
        const pairPx = (pairTotal / total) * rect.width;
        if (pairPx <= 0) return;
        // Drag distance into the pair, normalized 0..1.
        const t = Math.max(0, Math.min(1, (x - beforePx) / pairPx));
        const newLeft = Math.round(pairTotal * t);
        const newRight = pairTotal - newLeft;
        const next = [...initial];
        next[boundaryIndex] = newLeft;
        next[boundaryIndex + 1] = newRight;
        setDraftWeights(next);
      };

      const onUp = () => {
        window.removeEventListener('pointermove', onMove);
        window.removeEventListener('pointerup', onUp);
        window.removeEventListener('pointercancel', onUp);
        draggingRef.current = false;
        // Read fresh state via the setter form so we commit the latest
        // weights even if React batched a final move event.
        setDraftWeights((latest) => {
          // Diff against props so we only PATCH segments that moved.
          const updates = segments
            .map((s, i) => ({ id: s.id, weight: latest[i] }))
            .filter((u, i) => u.weight !== segments[i].weight);
          if (updates.length > 0) onCommit(updates);
          return latest;
        });
      };

      window.addEventListener('pointermove', onMove);
      window.addEventListener('pointerup', onUp);
      window.addEventListener('pointercancel', onUp);
    },
    [disabled, draftWeights, segments, total, onCommit],
  );

  if (segments.length === 0) return null;
  if (segments.length === 1) {
    return (
      <div className="h-7 rounded-md border overflow-hidden flex">
        <div
          className={`${segments[0].colorClass} text-[11px] text-white flex items-center justify-center font-medium`}
          style={{ width: '100%' }}
          title={`${segments[0].label} (100%)`}
        >
          <span className="truncate px-2">{segments[0].label} 100%</span>
        </div>
      </div>
    );
  }

  return (
    <div
      ref={containerRef}
      className={`relative h-7 rounded-md border overflow-hidden flex select-none ${
        disabled ? 'opacity-60' : ''
      }`}
    >
      {segments.map((s, i) => {
        const w = draftWeights[i] ?? s.weight;
        const pct = (w / total) * 100;
        return (
          <div
            key={s.id}
            className={`${s.colorClass} text-[11px] text-white flex items-center justify-center font-medium relative`}
            style={{ width: `${pct}%` }}
            title={`${s.label} (${pct.toFixed(0)}%)`}
          >
            <span className="truncate px-2">
              {pct >= 12 ? `${s.label} ${pct.toFixed(0)}%` : `${pct.toFixed(0)}%`}
            </span>
            {i < segments.length - 1 && !disabled && (
              <div
                onPointerDown={startDrag(i)}
                className="absolute right-0 top-0 h-full w-2 -mr-1 cursor-col-resize z-10 hover:bg-white/30"
                role="separator"
                aria-label="Resize"
              />
            )}
          </div>
        );
      })}
    </div>
  );
}

/// Stable color picker for routes. Same route_id always gets the same
/// Tailwind class so the bar visualization stays familiar across reloads.
const PALETTE = [
  'bg-emerald-600',
  'bg-sky-600',
  'bg-violet-600',
  'bg-amber-600',
  'bg-rose-600',
  'bg-teal-600',
  'bg-indigo-600',
  'bg-orange-600',
];

export function colorClassForRoute(routeId: string): string {
  // Tiny FNV-1a-ish hash, deterministic, no deps.
  let h = 2166136261;
  for (let i = 0; i < routeId.length; i++) {
    h ^= routeId.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return PALETTE[Math.abs(h) % PALETTE.length];
}
