import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';

/// Small wrapper around `<Input type="number">` with a label and
/// optional hint. Used everywhere on the Settings page for TTL /
/// timeout / retention inputs.
export function NumberField({
  label,
  value,
  onChange,
  min = 0,
  max,
  hint,
  readOnly,
}: {
  label: string;
  value: number;
  onChange: (v: number) => void;
  min?: number;
  max?: number;
  hint?: string;
  readOnly?: boolean;
}) {
  return (
    <div className="space-y-1">
      <Label className="text-sm">{label}</Label>
      <Input
        type="number"
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        min={min}
        max={max}
        readOnly={readOnly}
        className={readOnly ? 'bg-muted' : ''}
      />
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  );
}
