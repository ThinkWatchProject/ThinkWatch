import { useEffect, useRef, useState, type KeyboardEvent } from 'react';

interface Props {
  /// Stored weight integer. Display = `weight / sum(enabled weights) * 100`.
  weight: number;
  /// Pre-computed share percentage (caller has the full route list to sum).
  pct: number;
  /// Whether this route is enabled. Disabled routes show "—" and don't accept edits.
  enabled: boolean;
  /// Greys out the input — used when the active routing strategy ignores
  /// weights (e.g. `latency`, where ratios are auto-tuned).
  disabled?: boolean;
  /// Fired on Enter or blur with a non-zero diff. Caller persists.
  onCommit: (newWeight: number) => void;
}

/// Click-to-edit weight cell. Renders the percentage in a friendly
/// "67%" format; on click reveals a numeric input for the raw weight,
/// committing on Enter / blur. Esc cancels.
///
/// Why two values? The wizard's mental model is "ratio = % of traffic".
/// The DB stores weight as an integer ratio numerator. We compute %
/// from `weight / sum`, but admins type the raw weight directly so a
/// "60 / 30 / 10" input gives them an exactly 60/30/10 split — typing
/// percentages would have to round to keep the sum at 100, which is
/// surprising.
export function InlinePercentInput({
  weight,
  pct,
  enabled,
  disabled,
  onCommit,
}: Props) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(String(weight));
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (editing) {
      inputRef.current?.focus();
      inputRef.current?.select();
    }
  }, [editing]);

  useEffect(() => {
    if (!editing) setDraft(String(weight));
  }, [weight, editing]);

  if (!enabled) {
    return <span className="text-[11px] text-muted-foreground">—</span>;
  }

  const commit = () => {
    const n = Number(draft);
    if (Number.isFinite(n) && n >= 0 && n !== weight) {
      onCommit(Math.floor(n));
    }
    setEditing(false);
  };

  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      commit();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      setDraft(String(weight));
      setEditing(false);
    }
  };

  if (editing) {
    return (
      <input
        ref={inputRef}
        type="number"
        min={0}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={onKey}
        className="w-16 h-6 text-[11px] text-right tabular-nums px-1 rounded border bg-background"
      />
    );
  }

  return (
    <button
      type="button"
      onClick={() => !disabled && setEditing(true)}
      disabled={disabled}
      className={`inline-flex items-baseline gap-0.5 text-[11px] tabular-nums hover:bg-muted/50 rounded px-1 ${
        disabled ? 'opacity-50 cursor-not-allowed' : 'cursor-pointer'
      }`}
      title={`weight=${weight}`}
    >
      <span>{pct.toFixed(0)}</span>
      <span className="text-muted-foreground">%</span>
    </button>
  );
}
