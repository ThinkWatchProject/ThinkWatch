import { useId, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { SaveIndicator } from './SaveIndicator';
import type { FieldSaveState } from './useFieldAutosave';

export function GatewaySave({
  state, error, disabled, onSave, onReload,
}: {
  state: FieldSaveState;
  error: string | null;
  disabled: boolean;
  onSave: () => void;
  onReload?: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Button type="button" disabled={disabled} onClick={onSave}>{t('common.save')}</Button>
      {state !== 'error' && <SaveIndicator state={state} error={null} />}
      {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      {onReload && <Button type="button" variant="outline" disabled={state === 'saving'} onClick={onReload}>{t('settings.gatewayDiscardReload')}</Button>}
    </div>
  );
}

export function GatewaySelect({
  label, value, options, onChange, disabled, placeholder,
}: {
  label: string;
  value: string;
  options: { value: string; label: string }[];
  onChange: (value: string) => void;
  disabled?: boolean;
  placeholder?: string;
}) {
  return (
    <GatewayField label={label}>
      {(id) => (
        <Select value={value} onValueChange={onChange} disabled={disabled}>
          <SelectTrigger id={id}><SelectValue placeholder={placeholder} /></SelectTrigger>
          <SelectContent>
            {options.map((option) => <SelectItem key={option.value} value={option.value}>{option.label}</SelectItem>)}
          </SelectContent>
        </Select>
      )}
    </GatewayField>
  );
}

export function GatewayField({ label, children }: { label: string; children: (id: string) => ReactNode }) {
  const id = useId();
  return (
    <div className="min-w-0 space-y-1.5">
      <Label htmlFor={id}>{label}</Label>
      {children(id)}
    </div>
  );
}
