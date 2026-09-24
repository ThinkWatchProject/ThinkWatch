import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import type { HiddenTextAction } from '../admin/settings/types';

interface Props {
  action: HiddenTextAction;
  onChange: (next: HiddenTextAction) => void;
  canWrite: boolean;
}

/**
 * What a request carrying hidden characters gets — Unicode tag characters
 * and bidi overrides, in what the caller typed or in a tool result. Saved
 * with the page.
 */
export function HiddenTextCard({ action, onChange, canWrite }: Props) {
  const { t } = useTranslation();
  return (
    <Card>
      <CardHeader>
        <div className="flex items-start justify-between gap-4">
          <div className="space-y-1">
            <CardTitle className="text-base">{t('settings.hiddenText.title')}</CardTitle>
            <p className="text-xs text-muted-foreground max-w-2xl">
              {t('settings.hiddenText.intro')}
            </p>
          </div>
          <div className="flex items-center gap-2 shrink-0">
            <span className="text-sm text-muted-foreground">{t('settings.hiddenText.action')}</span>
            <Select
              value={action}
              onValueChange={(v) => v && onChange(v as HiddenTextAction)}
              disabled={!canWrite}
            >
              <SelectTrigger className="h-8 w-[120px]">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="off">{t('settings.hiddenText.off')}</SelectItem>
                <SelectItem value="log">{t('settings.contentFilter.actionLog')}</SelectItem>
                <SelectItem value="warn">{t('settings.contentFilter.actionWarn')}</SelectItem>
                <SelectItem value="block">
                  <span className="text-destructive">{t('settings.contentFilter.actionBlock')}</span>
                </SelectItem>
              </SelectContent>
            </Select>
          </div>
        </div>
      </CardHeader>
      <CardContent>
        <p className="text-xs text-muted-foreground">{t('settings.hiddenText.behavior')}</p>
      </CardContent>
    </Card>
  );
}
