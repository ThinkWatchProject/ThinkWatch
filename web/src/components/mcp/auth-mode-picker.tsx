import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { AUTH_MODES, authModeIcon, type AuthMode } from './auth-mode-utils';

export type { AuthMode } from './auth-mode-utils';

interface AuthModePickerProps {
  value: AuthMode | null;
  onChange: (mode: AuthMode) => void;
}

export function AuthModePicker({ value, onChange }: AuthModePickerProps) {
  const { t } = useTranslation();
  return (
    <div className="grid gap-2">
      {AUTH_MODES.map((mode) => {
        const Icon = authModeIcon[mode];
        const selected = value === mode;
        return (
          <button
            key={mode}
            type="button"
            onClick={() => onChange(mode)}
            className={cn(
              'flex items-start gap-3 rounded-md border p-3 text-left transition-colors',
              'hover:bg-accent/50',
              selected
                ? 'border-primary bg-accent ring-1 ring-primary'
                : 'border-input',
            )}
          >
            <Icon className={cn('h-5 w-5 mt-0.5 shrink-0', selected ? 'text-primary' : 'text-muted-foreground')} />
            <div className="min-w-0 flex-1">
              <div className="text-sm font-medium">{t(`mcpServers.wizard.modes.${mode}.title`)}</div>
              <div className="text-xs text-muted-foreground mt-0.5">
                {t(`mcpServers.wizard.modes.${mode}.description`)}
              </div>
            </div>
          </button>
        );
      })}
    </div>
  );
}
