import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';

export interface AuthHeaderFields {
  headerName: string;
  valueTemplate: string;
}

/**
 * Common upstream-credential header shapes. Picking a preset writes
 * both fields at once. The template MUST contain exactly one
 * `{{token}}` — backend rejects anything else (including the
 * `{{user_id}}` / `{{user_email}}` placeholders that are valid in
 * `custom_headers` — those are deliberately separate machinery).
 */
const PRESETS: { id: string; label: string; fields: AuthHeaderFields }[] = [
  {
    id: 'bearer',
    label: 'Bearer',
    fields: { headerName: 'Authorization', valueTemplate: 'Bearer {{token}}' },
  },
  {
    id: 'x-api-key',
    label: 'X-API-Key',
    fields: { headerName: 'X-API-Key', valueTemplate: '{{token}}' },
  },
  {
    id: 'azure',
    label: 'api-key',
    fields: { headerName: 'api-key', valueTemplate: '{{token}}' },
  },
  {
    id: 'github-legacy',
    label: 'token',
    fields: { headerName: 'Authorization', valueTemplate: 'token {{token}}' },
  },
];

interface Props {
  value: AuthHeaderFields;
  onChange: (next: AuthHeaderFields) => void;
  /** Preview token used in the live preview. Defaults to a redacted sample. */
  previewToken?: string;
}

/**
 * Editor for `auth_header_name` + `auth_value_template`. Used by both
 * the create wizard and the edit form.
 *
 * Renders a presets row + the two raw inputs + a live preview of the
 * exact header line the gateway will send. Preview uses a redacted
 * sample token so users immediately see "where this token goes" —
 * the most common confusion in the per-user PAT flow.
 */
export function AuthHeaderFieldset({ value, onChange, previewToken }: Props) {
  const { t } = useTranslation();
  const sample = previewToken && previewToken.length > 0
    ? `${previewToken.slice(0, 6)}…`
    : '••••••••';
  // Defensive: callers should always pass a non-empty template, but
  // if a stale API response sneaks through with an empty string the
  // preview should still render rather than crash on .replaceAll().
  const template = value.valueTemplate || 'Bearer {{token}}';
  const renderedValue = template.replaceAll('{{token}}', sample);

  return (
    <div className="space-y-3">
      <div>
        <Label className="text-sm font-medium">
          {t('mcpServers.authHeader.title')}
        </Label>
        <p className="text-xs text-muted-foreground">
          {t('mcpServers.authHeader.hint')}
        </p>
      </div>

      <div className="flex flex-wrap gap-2">
        {PRESETS.map((p) => {
          const active =
            p.fields.headerName === value.headerName &&
            p.fields.valueTemplate === value.valueTemplate;
          return (
            <Button
              key={p.id}
              type="button"
              size="sm"
              variant={active ? 'default' : 'outline'}
              onClick={() => onChange(p.fields)}
            >
              {p.label}
            </Button>
          );
        })}
      </div>

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        <div className="space-y-1.5">
          <Label htmlFor="auth-header-name" className="text-xs">
            {t('mcpServers.authHeader.headerName')}
          </Label>
          <Input
            id="auth-header-name"
            value={value.headerName}
            onChange={(e) =>
              onChange({ ...value, headerName: e.target.value })
            }
            placeholder="Authorization"
          />
        </div>
        <div className="space-y-1.5">
          <Label htmlFor="auth-value-template" className="text-xs">
            {t('mcpServers.authHeader.valueTemplate')}
          </Label>
          <Input
            id="auth-value-template"
            value={value.valueTemplate}
            onChange={(e) =>
              onChange({ ...value, valueTemplate: e.target.value })
            }
            placeholder="Bearer {{token}}"
          />
        </div>
      </div>

      <div className="rounded border bg-muted/30 px-2 py-1.5 text-xs">
        <span className="text-muted-foreground">
          {t('mcpServers.authHeader.previewLabel')}:
        </span>{' '}
        <code className="font-mono">
          {value.headerName}: {renderedValue}
        </code>
      </div>
    </div>
  );
}
