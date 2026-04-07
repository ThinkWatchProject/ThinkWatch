import { useState, useCallback, useMemo } from 'react';
import { format, parse, isValid, subHours, subDays } from 'date-fns';
import type { Locale } from 'date-fns';
import { zhCN } from 'date-fns/locale/zh-CN';
import { enUS } from 'date-fns/locale/en-US';
import { CalendarIcon, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { DateRange } from 'react-day-picker';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Calendar } from '@/components/ui/calendar';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { cn } from '@/lib/utils';

const LOCALE_MAP: Record<string, Locale> = { zh: zhCN, 'zh-CN': zhCN, en: enUS };

interface DateTimeRangePickerProps {
  from: string;
  to: string;
  onFromChange: (value: string) => void;
  onToChange: (value: string) => void;
  className?: string;
}

function parseValue(v: string): Date | undefined {
  if (!v) return undefined;
  const d = parse(v, "yyyy-MM-dd'T'HH:mm", new Date());
  return isValid(d) ? d : undefined;
}

function toTimeStr(d: Date | undefined, fallback: string): string {
  return d ? format(d, 'HH:mm') : fallback;
}

function combine(date: Date, time: string): string {
  return `${format(date, 'yyyy-MM-dd')}T${time}`;
}

function formatDt(d: Date): string {
  return format(d, "yyyy-MM-dd'T'HH:mm");
}

type PresetKey = 'last1h' | 'last6h' | 'last24h' | 'last3d' | 'last7d' | 'last30d';

const PRESETS: { key: PresetKey; getRange: () => [Date, Date] }[] = [
  { key: 'last1h', getRange: () => [subHours(new Date(), 1), new Date()] },
  { key: 'last6h', getRange: () => [subHours(new Date(), 6), new Date()] },
  { key: 'last24h', getRange: () => [subHours(new Date(), 24), new Date()] },
  { key: 'last3d', getRange: () => [subDays(new Date(), 3), new Date()] },
  { key: 'last7d', getRange: () => [subDays(new Date(), 7), new Date()] },
  { key: 'last30d', getRange: () => [subDays(new Date(), 30), new Date()] },
];

export function DateTimeRangePicker({ from, to, onFromChange, onToChange, className }: DateTimeRangePickerProps) {
  const { t, i18n } = useTranslation();
  const [open, setOpen] = useState(false);
  const locale = useMemo(() => LOCALE_MAP[i18n.language] ?? enUS, [i18n.language]);

  const fromDate = parseValue(from);
  const toDate = parseValue(to);
  const fromTime = toTimeStr(fromDate, '00:00');
  const toTime = toTimeStr(toDate, '23:59');

  const handleRangeSelect = useCallback((range: DateRange | undefined) => {
    if (!range) return;
    if (range.from) {
      onFromChange(combine(range.from, fromTime));
    }
    if (range.to) {
      onToChange(combine(range.to, toTime));
    } else if (range.from && !range.to) {
      onToChange('');
    }
  }, [onFromChange, onToChange, fromTime, toTime]);

  const handleFromTime = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    const time = e.target.value;
    if (fromDate) {
      onFromChange(combine(fromDate, time));
    }
  }, [onFromChange, fromDate]);

  const handleToTime = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    const time = e.target.value;
    if (toDate) {
      onToChange(combine(toDate, time));
    }
  }, [onToChange, toDate]);

  const handleClear = useCallback(() => {
    onFromChange('');
    onToChange('');
  }, [onFromChange, onToChange]);

  const applyPreset = useCallback((getRange: () => [Date, Date]) => {
    const [s, e] = getRange();
    onFromChange(formatDt(s));
    onToChange(formatDt(e));
    setOpen(false);
  }, [onFromChange, onToChange]);

  const hasValue = fromDate || toDate;

  /**
   * Smart-compact the displayed range:
   *   - same calendar day → "2026-04-07 08:55 → 23:55"
   *   - same year but different day → "04-07 08:55 → 04-08 10:00"
   *   - different year → "2025-12-30 08:55 → 2026-01-02 10:00"
   *   - only one side set → that side rendered in full
   * Never truncates — the trigger grows to fit whatever text is produced.
   */
  const displayText = (() => {
    if (!hasValue) return null;
    if (!fromDate) return `… → ${format(toDate!, 'yyyy-MM-dd HH:mm')}`;
    if (!toDate) return `${format(fromDate, 'yyyy-MM-dd HH:mm')} → …`;
    const sameDay =
      fromDate.getFullYear() === toDate.getFullYear() &&
      fromDate.getMonth() === toDate.getMonth() &&
      fromDate.getDate() === toDate.getDate();
    const sameYear = fromDate.getFullYear() === toDate.getFullYear();
    if (sameDay) {
      return `${format(fromDate, 'yyyy-MM-dd HH:mm')} → ${format(toDate, 'HH:mm')}`;
    }
    if (sameYear) {
      return `${format(fromDate, 'MM-dd HH:mm')} → ${format(toDate, 'MM-dd HH:mm')}`;
    }
    return `${format(fromDate, 'yyyy-MM-dd HH:mm')} → ${format(toDate, 'yyyy-MM-dd HH:mm')}`;
  })();

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          className={cn(
            'justify-start text-left font-normal h-9 gap-2',
            !displayText && 'text-muted-foreground',
            className,
          )}
        >
          <CalendarIcon className="h-4 w-4 shrink-0" />
          <span className="whitespace-nowrap">
            {displayText ?? t('logs.pickDateRange', 'Select date range')}
          </span>
          {hasValue && (
            <X
              className="ml-1 h-3.5 w-3.5 shrink-0 opacity-50 hover:opacity-100"
              onClick={(e) => { e.stopPropagation(); handleClear(); }}
            />
          )}
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-auto p-0" align="start">
        <div className="flex">
          <div className="border-r px-2 py-3 flex flex-col gap-1 min-w-[7rem]">
            {PRESETS.map(({ key, getRange }) => (
              <Button
                key={key}
                variant="ghost"
                size="sm"
                className="justify-start text-xs h-7 px-2"
                onClick={() => applyPreset(getRange)}
              >
                {t(`logs.preset.${key}`, key)}
              </Button>
            ))}
          </div>
          <div className="relative">
            <Calendar
              mode="range"
              selected={{ from: fromDate, to: toDate }}
              onSelect={handleRangeSelect}
              numberOfMonths={2}
              locale={locale}
              initialFocus
            />
            <div className="border-t px-3 py-2 flex items-center gap-4">
              <div className="flex items-center gap-1.5">
                <Label className="text-xs text-muted-foreground whitespace-nowrap">{t('logs.dateFrom', 'From')}</Label>
                <Input type="time" value={fromTime} onChange={handleFromTime} className="h-7 w-[6.5rem] text-xs" disabled={!fromDate} />
              </div>
              <span className="text-muted-foreground text-xs">→</span>
              <div className="flex items-center gap-1.5">
                <Label className="text-xs text-muted-foreground whitespace-nowrap">{t('logs.dateTo', 'To')}</Label>
                <Input type="time" value={toTime} onChange={handleToTime} className="h-7 w-[6.5rem] text-xs" disabled={!toDate} />
              </div>
            </div>
          </div>
        </div>
      </PopoverContent>
    </Popover>
  );
}
