import { useTranslation } from 'react-i18next';
import { Plus, Trash2 } from 'lucide-react';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import type {
  ToolAction,
  ToolInspectionConfig,
  ToolInspectionMode,
  ToolRule,
} from '../admin/settings/types';

interface Props {
  config: ToolInspectionConfig;
  rules: ToolRule[];
  onChange: (next: ToolInspectionConfig) => void;
  canWrite: boolean;
}

/**
 * Tool-call inspection: which tool calls an upstream returns get recorded
 * or cut. Built-in rules are listed from the server and can be switched off
 * or re-graded; custom rules are added on top. Saved with the page.
 */
export function ToolInspectionCard({ config, rules, onChange, canWrite }: Props) {
  const { t } = useTranslation();

  // A new built-in rule shipped by the server has no translation yet;
  // its English name and reason stand in.
  const ruleName = (r: ToolRule) =>
    t(`settings.toolInspection.rules.${r.id}.name`, { defaultValue: r.name });
  const ruleWhy = (r: ToolRule) =>
    t(`settings.toolInspection.rules.${r.id}.why`, { defaultValue: r.why });

  const setMode = (mode: ToolInspectionMode) => onChange({ ...config, mode });

  const setEnabled = (id: string, on: boolean) =>
    onChange({
      ...config,
      disabled: on ? config.disabled.filter((d) => d !== id) : [...config.disabled, id],
    });

  // Only an action that differs from the factory one is stored.
  const setAction = (r: ToolRule, action: ToolAction) => {
    const actions = { ...config.actions };
    if (action === r.default_action) delete actions[r.id];
    else actions[r.id] = action;
    onChange({ ...config, actions });
  };

  const addCustom = () =>
    onChange({ ...config, custom: [...config.custom, { name: '', pattern: '', action: 'record' }] });

  const updateCustom = (i: number, patch: Partial<ToolInspectionConfig['custom'][number]>) =>
    onChange({
      ...config,
      custom: config.custom.map((c, idx) => (idx === i ? { ...c, ...patch } : c)),
    });

  const removeCustom = (i: number) =>
    onChange({ ...config, custom: config.custom.filter((_, idx) => idx !== i) });

  const actionSelect = (value: ToolAction, onValue: (a: ToolAction) => void, disabled: boolean) => (
    <Select
      value={value}
      onValueChange={(v) => v && onValue(v as ToolAction)}
      disabled={disabled || !canWrite}
    >
      <SelectTrigger className="h-8">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="cut">
          <span className="text-destructive">{t('settings.toolInspection.actionCut')}</span>
        </SelectItem>
        <SelectItem value="record">
          <span className="text-muted-foreground">{t('settings.toolInspection.actionRecord')}</span>
        </SelectItem>
      </SelectContent>
    </Select>
  );

  return (
    <Card>
      <CardHeader>
        <div className="flex items-start justify-between gap-4">
          <div className="space-y-1">
            <CardTitle className="text-base">{t('settings.toolInspection.title')}</CardTitle>
            <p className="text-xs text-muted-foreground max-w-2xl">
              {t('settings.toolInspection.intro')}
            </p>
          </div>
          <div className="flex items-center gap-2 shrink-0">
            <span className="text-sm text-muted-foreground">{t('settings.toolInspection.mode')}</span>
            <Select
              value={config.mode}
              onValueChange={(v) => v && setMode(v as ToolInspectionMode)}
              disabled={!canWrite}
            >
              <SelectTrigger className="h-8 w-[120px]">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="off">{t('settings.toolInspection.modeOff')}</SelectItem>
                <SelectItem value="observe">{t('settings.toolInspection.modeObserve')}</SelectItem>
                <SelectItem value="enforce">{t('settings.toolInspection.modeEnforce')}</SelectItem>
              </SelectContent>
            </Select>
          </div>
        </div>
      </CardHeader>
      <CardContent className="space-y-6">
        <div className="space-y-2">
          <h4 className="text-sm font-medium">{t('settings.toolInspection.builtin')}</h4>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t('settings.toolInspection.rule')}</TableHead>
                <TableHead className="w-[90px]">{t('settings.toolInspection.enabled')}</TableHead>
                <TableHead className="w-[140px]">{t('settings.toolInspection.inEnforce')}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rules.map((r) => {
                const on = !config.disabled.includes(r.id);
                return (
                  <TableRow key={r.id}>
                    <TableCell>
                      <div className={on ? '' : 'text-muted-foreground'}>
                        <p className="text-sm">{ruleName(r)}</p>
                        <p className="text-xs text-muted-foreground">{ruleWhy(r)}</p>
                      </div>
                    </TableCell>
                    <TableCell>
                      <Switch
                        checked={on}
                        onCheckedChange={(v) => setEnabled(r.id, v)}
                        disabled={!canWrite}
                        aria-label={ruleName(r)}
                      />
                    </TableCell>
                    <TableCell>
                      {actionSelect(
                        config.actions[r.id] ?? r.default_action,
                        (a) => setAction(r, a),
                        !on,
                      )}
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </div>

        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <h4 className="text-sm font-medium">{t('settings.toolInspection.custom')}</h4>
            <Button variant="outline" size="sm" onClick={addCustom} disabled={!canWrite}>
              <Plus className="h-4 w-4" />
              {t('settings.toolInspection.addRule')}
            </Button>
          </div>
          {config.custom.length === 0 ? (
            <p className="text-sm text-muted-foreground py-4 text-center">
              {t('settings.toolInspection.customEmpty')}
            </p>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-[180px]">{t('settings.toolInspection.name')}</TableHead>
                  <TableHead>{t('settings.toolInspection.pattern')}</TableHead>
                  <TableHead className="w-[140px]">{t('settings.toolInspection.inEnforce')}</TableHead>
                  <TableHead className="w-10" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {config.custom.map((c, i) => (
                  <TableRow key={i}>
                    <TableCell>
                      <Input
                        value={c.name}
                        onChange={(e) => updateCustom(i, { name: e.target.value })}
                        placeholder={t('settings.toolInspection.namePlaceholder')}
                        className="h-8"
                        disabled={!canWrite}
                      />
                    </TableCell>
                    <TableCell>
                      <Input
                        value={c.pattern}
                        onChange={(e) => updateCustom(i, { pattern: e.target.value })}
                        placeholder="kubectl\s+delete"
                        className="h-8 font-mono text-xs"
                        disabled={!canWrite}
                      />
                    </TableCell>
                    <TableCell>
                      {actionSelect(c.action, (a) => updateCustom(i, { action: a }), false)}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        onClick={() => removeCustom(i)}
                        disabled={!canWrite}
                        aria-label={t('common.delete')}
                        title={t('common.delete')}
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </div>

        <p className="text-xs text-muted-foreground">{t('settings.toolInspection.behavior')}</p>
      </CardContent>
    </Card>
  );
}
