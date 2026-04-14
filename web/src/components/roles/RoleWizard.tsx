import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/tabs';
import { cn } from '@/lib/utils';
import { CheckCircle2 } from 'lucide-react';

export interface WizardStep {
  id: string;
  label: string;
  /** Shown in the left rail under the label. */
  hint?: string;
  /** The step's body — any JSX. */
  content: ReactNode;
  /** Return a user-facing error message to block Next; return null when OK. */
  validate?: () => string | null;
}

interface RoleWizardProps {
  steps: WizardStep[];
  submitting?: boolean;
  submitLabel: string;
  onSubmit: () => void;
  /** Optional: extra footer content shown between Prev and Next (e.g. "Reset to defaults" for system roles). */
  footerExtras?: ReactNode;
}

/**
 * 3-pane wizard shell for the Add/Edit Role dialog. Two-column layout:
 * a vertical step rail on the left and the active step's body on the
 * right. Built on shadcn Tabs with `orientation="vertical"` so keyboard
 * navigation and Radix a11y come for free.
 *
 * Step-validation is opt-in per step: `validate()` returning a string
 * blocks the Next button and surfaces the message inline.
 */
export function RoleWizard({
  steps,
  submitting,
  submitLabel,
  onSubmit,
  footerExtras,
}: RoleWizardProps) {
  const { t } = useTranslation();
  const [currentId, setCurrentId] = useState(steps[0]?.id);
  const [errors, setErrors] = useState<Record<string, string | null>>({});

  const currentIdx = steps.findIndex((s) => s.id === currentId);
  const isLast = currentIdx === steps.length - 1;
  const isFirst = currentIdx === 0;
  const currentErr = currentId ? errors[currentId] : null;

  const goNext = () => {
    const step = steps[currentIdx];
    const err = step?.validate?.() ?? null;
    if (err) {
      setErrors((s) => ({ ...s, [step.id]: err }));
      return;
    }
    setErrors((s) => ({ ...s, [step.id]: null }));
    const next = steps[currentIdx + 1];
    if (next) setCurrentId(next.id);
  };

  const goPrev = () => {
    const prev = steps[currentIdx - 1];
    if (prev) setCurrentId(prev.id);
  };

  const handleSubmit = () => {
    // Validate all prior steps + current one.
    for (let i = 0; i <= currentIdx; i++) {
      const err = steps[i].validate?.() ?? null;
      if (err) {
        setErrors((s) => ({ ...s, [steps[i].id]: err }));
        setCurrentId(steps[i].id);
        return;
      }
    }
    onSubmit();
  };

  return (
    <Tabs
      orientation="vertical"
      value={currentId}
      onValueChange={setCurrentId}
      className="flex flex-1 flex-row gap-6 min-h-[30rem]"
    >
      <TabsList className="flex h-auto flex-col items-stretch gap-1 bg-transparent p-0 w-52 shrink-0">
        {steps.map((s, i) => (
          <TabsTrigger
            key={s.id}
            value={s.id}
            className={cn(
              'justify-start gap-3 rounded-md px-3 py-2 text-left',
              'data-[state=active]:bg-accent data-[state=active]:text-accent-foreground',
            )}
          >
            <span
              className={cn(
                'flex h-6 w-6 shrink-0 items-center justify-center rounded-full border text-xs font-medium',
                i < currentIdx
                  ? 'border-primary bg-primary text-primary-foreground'
                  : i === currentIdx
                    ? 'border-primary text-primary'
                    : 'border-muted-foreground/30 text-muted-foreground',
              )}
            >
              {i < currentIdx ? <CheckCircle2 className="h-3.5 w-3.5" /> : i + 1}
            </span>
            <span className="flex min-w-0 flex-1 flex-col">
              <span className="truncate text-sm">{s.label}</span>
              {s.hint && (
                <span className="truncate text-[10px] text-muted-foreground">{s.hint}</span>
              )}
            </span>
          </TabsTrigger>
        ))}
      </TabsList>

      <div className="flex min-w-0 flex-1 flex-col">
        <div className="flex-1 min-h-0">
          {steps.map((s) => (
            <TabsContent key={s.id} value={s.id} className="mt-0 space-y-4">
              {s.content}
            </TabsContent>
          ))}
        </div>

        {currentErr && (
          <p className="mt-3 text-xs text-destructive">{currentErr}</p>
        )}

        <div className="mt-4 flex items-center gap-2 border-t pt-4">
          {footerExtras}
          <div className="ml-auto flex items-center gap-2">
            {!isFirst && (
              <Button type="button" variant="outline" onClick={goPrev} disabled={submitting}>
                {t('common.previous', 'Previous')}
              </Button>
            )}
            {!isLast ? (
              <Button type="button" onClick={goNext} disabled={submitting}>
                {t('common.next', 'Next')}
              </Button>
            ) : (
              <Button type="button" onClick={handleSubmit} disabled={submitting}>
                {submitting ? t('common.loading') : submitLabel}
              </Button>
            )}
          </div>
        </div>
      </div>
    </Tabs>
  );
}
