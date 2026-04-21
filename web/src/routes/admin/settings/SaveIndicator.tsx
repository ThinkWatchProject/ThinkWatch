import { Check, AlertCircle, Loader2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import type { FieldSaveState } from './useFieldAutosave';

interface SaveIndicatorProps {
  state: FieldSaveState;
  error: string | null;
}

/// Compact inline-save status marker. Rendered to the right of the input
/// so the user always sees the save outcome where the change happened —
/// no more hunting for a page-level banner after each edit.
///
/// Pair with `useFieldAutosave` so every settings page reports save
/// state through the same primitive — admins learn one visual once
/// instead of guessing per page whether their edit actually landed.
export function SaveIndicator({ state, error }: SaveIndicatorProps) {
  const { t } = useTranslation();
  if (state === 'idle') {
    return <span className="inline-block h-3.5 w-3.5 shrink-0" aria-hidden="true" />;
  }
  if (state === 'saving') {
    return (
      <Loader2
        className="h-3.5 w-3.5 shrink-0 animate-spin text-muted-foreground"
        aria-label={t('admin.settings.saving')}
      />
    );
  }
  if (state === 'saved') {
    return (
      <Check
        className="h-3.5 w-3.5 shrink-0 text-emerald-500"
        aria-label={t('admin.settings.saved')}
      />
    );
  }
  const label = error ?? t('admin.settings.saveFailed');
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <AlertCircle
          className="h-3.5 w-3.5 shrink-0 cursor-help text-destructive"
          aria-label={label}
        />
      </TooltipTrigger>
      <TooltipContent side="top">{label}</TooltipContent>
    </Tooltip>
  );
}
