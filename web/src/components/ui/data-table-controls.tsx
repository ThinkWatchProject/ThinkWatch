import type { ReactNode } from 'react';
import { Search } from 'lucide-react';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';
import { useTranslation } from 'react-i18next';
import { DateTimeRangePicker } from '@/components/ui/datetime-picker';

/**
 * Shared row of table controls — search input, optional time-range
 * picker, optional category select, and a search button. Extracted
 * from `routes/logs.tsx` (the most feature-complete table) so
 * api-keys / users / mcp-servers can converge on the same shape
 * instead of each route inventing its own grid.
 *
 * Usage stays declarative:
 *
 *   <DataTableControls
 *     searchPlaceholder={…}
 *     searchValue={input}
 *     onSearchChange={setInput}
 *     onSearchSubmit={handleSearch}
 *     leading={<CategorySelect …/>}      // optional, e.g. logs.tsx
 *     timeRange={{ from, to, onFromChange, onToChange }} // optional
 *     trailing={<RefreshButton …/>}       // optional
 *   />
 *
 * Each slot is opt-in — pages that don't need a time picker just
 * omit `timeRange`.
 */
export function DataTableControls({
  searchValue,
  onSearchChange,
  onSearchSubmit,
  searchPlaceholder,
  leading,
  trailing,
  timeRange,
  searchLabel,
}: {
  searchValue: string;
  onSearchChange: (v: string) => void;
  onSearchSubmit?: () => void;
  searchPlaceholder?: string;
  leading?: ReactNode;
  trailing?: ReactNode;
  timeRange?: {
    from: string;
    to: string;
    onFromChange: (value: string) => void;
    onToChange: (value: string) => void;
  };
  /** Override the default "Search" button label. */
  searchLabel?: string;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-wrap items-center gap-2 mb-3">
      {leading}
      <Input
        placeholder={searchPlaceholder}
        value={searchValue}
        onChange={(e) => onSearchChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && onSearchSubmit) onSearchSubmit();
        }}
        className="flex-1 font-mono text-sm"
      />
      {timeRange && (
        <DateTimeRangePicker
          className="shrink-0"
          from={timeRange.from}
          to={timeRange.to}
          onFromChange={timeRange.onFromChange}
          onToChange={timeRange.onToChange}
        />
      )}
      {onSearchSubmit && (
        <Button onClick={onSearchSubmit} className="shrink-0">
          <Search className="h-4 w-4 mr-1" />
          {searchLabel ?? t('common.search')}
        </Button>
      )}
      {trailing}
    </div>
  );
}
