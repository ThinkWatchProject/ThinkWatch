import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Slider as SliderPrimitive } from 'radix-ui';
import { Scale } from 'lucide-react';

interface Segment {
  id: string;
  /// Display label (provider + label or upstream model). Tooltip + visible.
  label: string;
  /// Stored weight (integer). Caller pre-filters to enabled-only routes.
  weight: number;
}

interface Props {
  segments: Segment[];
  /// Disable drag handles when admin lacks write permission.
  disabled?: boolean;
  /// Fired on pointer release with the new weight set. Caller persists
  /// via `PATCH /api/admin/model-routes/batch-weights`.
  onCommit: (updates: { id: string; weight: number }[]) => void;
}

/// One row per route, each row a Radix slider. Dragging one slider
/// grows (or shrinks) that route; the delta is redistributed across
/// **all other** routes proportionally to their weights at drag-start.
/// Snapshotting at drag-start means motion stays linear even when
/// neighbours approach zero.
///
/// Smooth at 60fps via local draft state; commits to the parent on
/// pointer release so we don't spam PATCH requests during drag.
export function TrafficBar({ segments, disabled, onCommit }: Props) {
  const { t } = useTranslation();

  const [draft, setDraft] = useState<number[]>(segments.map((s) => s.weight));
  const dragSnapshot = useRef<{ index: number; weights: number[] } | null>(null);

  useEffect(() => {
    if (!dragSnapshot.current) {
      setDraft(segments.map((s) => s.weight));
    }
  }, [segments]);

  const total = draft.reduce((a, b) => a + b, 0) || 1;

  const handleValueChange = (segmentIndex: number, [newPct]: number[]) => {
    if (
      !dragSnapshot.current ||
      dragSnapshot.current.index !== segmentIndex
    ) {
      dragSnapshot.current = { index: segmentIndex, weights: [...draft] };
    }
    const initial = dragSnapshot.current.weights;
    const initialTotal = initial.reduce((a, b) => a + b, 0) || 1;
    const newK = Math.max(0, Math.min(initialTotal, (newPct / 100) * initialTotal));
    const oldK = initial[segmentIndex];
    const delta = newK - oldK;

    const next = [...initial];
    next[segmentIndex] = newK;

    const sumOthers = initial
      .map((w, i) => (i === segmentIndex ? 0 : w))
      .reduce((a, b) => a + b, 0);
    for (let i = 0; i < initial.length; i++) {
      if (i === segmentIndex) continue;
      if (sumOthers > 0) {
        next[i] = initial[i] - delta * (initial[i] / sumOthers);
      } else {
        const denom = initial.length - 1 || 1;
        next[i] = -delta / denom;
      }
      if (next[i] < 0) next[i] = 0;
    }
    setDraft(next.map((w) => Math.round(w)));
  };

  const handleValueCommit = () => {
    dragSnapshot.current = null;
    setDraft((latest) => {
      const updates = segments
        .map((s, i) => ({ id: s.id, weight: latest[i] }))
        .filter((u, i) => u.weight !== segments[i].weight);
      if (updates.length > 0) onCommit(updates);
      return latest;
    });
  };

  const resetToEvenSplit = () => {
    if (disabled || segments.length < 2) return;
    const updates = segments
      .map((s) => ({ id: s.id, weight: 100 }))
      .filter((u, i) => u.weight !== segments[i].weight);
    if (updates.length > 0) onCommit(updates);
  };

  if (segments.length === 0) return null;

  return (
    <div className="space-y-2">
      {segments.map((s, i) => {
        const w = draft[i] ?? s.weight;
        const pct = (w / total) * 100;
        const single = segments.length === 1;
        return (
          <div key={s.id} className="flex items-center gap-3">
            <div
              className="text-[11px] font-mono truncate w-32 shrink-0"
              title={s.label}
            >
              {s.label}
            </div>
            <SliderPrimitive.Root
              value={[Math.round(pct)]}
              min={0}
              max={100}
              step={1}
              disabled={disabled || single}
              onValueChange={(v) => handleValueChange(i, v)}
              onValueCommit={handleValueCommit}
              className="relative flex flex-1 items-center select-none touch-none data-[disabled]:opacity-50"
              aria-label={s.label}
            >
              <SliderPrimitive.Track className="bg-muted relative grow overflow-hidden rounded-full h-1.5">
                <SliderPrimitive.Range className="bg-foreground absolute h-full" />
              </SliderPrimitive.Track>
              <SliderPrimitive.Thumb className="border-foreground/30 bg-background block size-4 shrink-0 rounded-full border shadow-sm hover:ring-2 hover:ring-foreground/20 focus-visible:ring-2 focus-visible:ring-foreground/40 focus-visible:outline-hidden disabled:pointer-events-none" />
            </SliderPrimitive.Root>
            <div className="text-xs font-mono tabular-nums w-10 text-right shrink-0">
              {pct.toFixed(0)}%
            </div>
          </div>
        );
      })}
      {!disabled && segments.length > 1 && (
        <div className="flex justify-end">
          <button
            type="button"
            onClick={resetToEvenSplit}
            className="text-muted-foreground hover:text-foreground text-[11px] inline-flex items-center gap-1"
            title={t('models.routing.evenSplitTooltip')}
          >
            <Scale className="h-3 w-3" />
            {t('models.routing.evenSplitTooltip')}
          </button>
        </div>
      )}
    </div>
  );
}
