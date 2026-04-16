import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Plus, X } from 'lucide-react';

export interface HeaderEditorProps {
  headers: [string, string][];
  onChange: (headers: [string, string][]) => void;
  /** Placeholder for the header name input (default: "Header-Name") */
  keyPlaceholder?: string;
  /** Placeholder for the header value input */
  valuePlaceholder?: string;
  /** Preset header buttons rendered after the "Add Header" button */
  presets?: { label: string; header: [string, string] }[];
}

export function HeaderEditor({
  headers,
  onChange,
  keyPlaceholder = 'Header-Name',
  valuePlaceholder,
  presets,
}: HeaderEditorProps) {
  const { t } = useTranslation();

  const updateKey = (i: number, key: string) => {
    const next = [...headers];
    next[i] = [key, headers[i][1]];
    onChange(next);
  };

  const updateValue = (i: number, value: string) => {
    const next = [...headers];
    next[i] = [headers[i][0], value];
    onChange(next);
  };

  const remove = (i: number) => {
    onChange(headers.filter((_, j) => j !== i));
  };

  const add = () => {
    onChange([...headers, ['', '']]);
  };

  return (
    <>
      {headers.map(([k, v], i) => (
        <div key={i} className="flex gap-2 items-center">
          <Input
            className="flex-1"
            placeholder={keyPlaceholder}
            value={k}
            onChange={(e) => updateKey(i, e.target.value)}
          />
          <Input
            className="flex-1"
            placeholder={valuePlaceholder ?? t('mcpServers.headerValuePlaceholder')}
            value={v}
            onChange={(e) => updateValue(i, e.target.value)}
          />
          <Button type="button" variant="ghost" size="icon-sm" onClick={() => remove(i)}>
            <X className="h-3 w-3" />
          </Button>
        </div>
      ))}
      <div className="flex flex-wrap gap-2">
        <Button type="button" variant="outline" size="sm" onClick={add}>
          <Plus className="mr-1 h-3 w-3" />{t('providers.addHeader')}
        </Button>
        {presets?.map((preset) => (
          <Button
            key={preset.label}
            type="button"
            variant="ghost"
            size="sm"
            className="text-xs text-muted-foreground"
            onClick={() => onChange([...headers, preset.header])}
          >
            + {preset.label}
          </Button>
        ))}
      </div>
    </>
  );
}
