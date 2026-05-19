import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Plus, Trash2 } from 'lucide-react';
import {
  DEFAULT_MAX_CHARS,
  MAX_CHARS_CEILING,
  type OutputGuardrail,
} from './types';

/// Per-model output guardrails sub-form rendered inside the model
/// edit drawer. Lists current rules with a remove button each, and
/// exposes an inline "+ Add max-length guardrail" affordance. Today
/// only `max_length` is wired — other variants stay TODO in the
/// gateway crate's roadmap docstring.
export function OutputGuardrailsCard({
  rules,
  onChange,
}: {
  rules: OutputGuardrail[];
  onChange: (next: OutputGuardrail[]) => void;
}) {
  const { t } = useTranslation();
  const [adding, setAdding] = useState(false);
  const [draftMaxChars, setDraftMaxChars] = useState<string>(String(DEFAULT_MAX_CHARS));
  const [draftError, setDraftError] = useState('');

  const removeAt = (i: number) => {
    const next = rules.slice();
    next.splice(i, 1);
    onChange(next);
  };

  const startAdd = () => {
    setDraftMaxChars(String(DEFAULT_MAX_CHARS));
    setDraftError('');
    setAdding(true);
  };

  const cancelAdd = () => {
    setAdding(false);
    setDraftError('');
  };

  const commitAdd = () => {
    const n = Number(draftMaxChars);
    if (!Number.isInteger(n) || n < 1 || n > MAX_CHARS_CEILING) {
      setDraftError(t('models.outputGuardrails.maxLengthRange', { max: MAX_CHARS_CEILING }));
      return;
    }
    onChange([...rules, { type: 'max_length', max_chars: n }]);
    setAdding(false);
    setDraftError('');
  };

  return (
    <div className="space-y-2 border-t pt-4">
      <Label className="text-sm font-medium">{t('models.outputGuardrails.title')}</Label>
      <p className="text-xs text-muted-foreground">
        {t('models.outputGuardrails.description')}
      </p>
      {rules.length === 0 && !adding && (
        <p className="text-xs italic text-muted-foreground">
          {t('models.outputGuardrails.noRules')}
        </p>
      )}
      {rules.length > 0 && (
        <ul className="space-y-1">
          {rules.map((rule, i) => (
            <li
              key={i}
              className="flex items-center justify-between gap-2 rounded border px-2 py-1 text-xs"
            >
              <span className="font-mono">
                {t('models.outputGuardrails.maxLengthLabel', { count: rule.max_chars })}
              </span>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                onClick={() => removeAt(i)}
                aria-label={t('common.remove')}
              >
                <Trash2 className="h-3.5 w-3.5 text-destructive" />
              </Button>
            </li>
          ))}
        </ul>
      )}
      {adding ? (
        <div className="space-y-2 rounded border p-2">
          <Label htmlFor="guardrail_max_chars" className="text-xs">
            {t('models.outputGuardrails.maxLengthLabelShort')}
          </Label>
          <Input
            id="guardrail_max_chars"
            value={draftMaxChars}
            onChange={(e) => setDraftMaxChars(e.target.value)}
            inputMode="numeric"
            min={1}
            max={MAX_CHARS_CEILING}
            type="number"
          />
          {draftError && (
            <p className="text-xs text-destructive">{draftError}</p>
          )}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" size="sm" onClick={cancelAdd}>
              {t('common.cancel')}
            </Button>
            <Button type="button" size="sm" onClick={commitAdd}>
              {t('common.add')}
            </Button>
          </div>
        </div>
      ) : (
        <Button type="button" variant="outline" size="sm" onClick={startAdd}>
          <Plus className="mr-1 h-3.5 w-3.5" />
          {t('models.outputGuardrails.addMaxLength')}
        </Button>
      )}
    </div>
  );
}
