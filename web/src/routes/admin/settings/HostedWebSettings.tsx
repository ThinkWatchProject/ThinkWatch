import { useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { ArrowDown, ArrowUp, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import { fetchAllPaginated } from '@/lib/paginated-fetch';
import { GatewayField, GatewaySave, GatewaySelect } from './gateway-form';
import { useGatewayForm } from './useGatewayForm';
import { HostedWebDiagnostic } from './HostedWebDiagnostic';

const engines = ['google_docs', 'duckduckgo', 'bing'] as const;
type Engine = typeof engines[number];
const engineNames: Record<Engine, string> = {
  google_docs: 'Google Docs',
  duckduckgo: 'DuckDuckGo',
  bing: 'Bing',
};

const limits = [
  { key: 'max_results', label: 'maxResults', min: 1, max: 20, default: 8 },
  { key: 'read_pages', label: 'readPages', min: 0, max: 5, default: 2 },
  { key: 'max_page_chars', label: 'pageChars', min: 1000, max: 50000, default: 12000 },
  { key: 'timeout_secs', label: 'timeout', min: 1, max: 120, default: 30 },
  { key: 'max_concurrent', label: 'concurrency', min: 1, max: 16, default: 4 },
  { key: 'interval_ms', label: 'interval', min: 250, max: 60000, default: 1000 },
  { key: 'cache_secs', label: 'cache', min: 0, max: 86400, default: 600 },
  { key: 'max_tool_calls', label: 'maxCalls', min: 1, max: 32, default: 8 },
] as const;

export interface HostedWebConfig {
  enabled_lineages?: string[];
  execution?: {
    mode?: 'upstream' | 'core';
    engines?: Engine[];
    proxy?: string;
    language?: string;
    region?: string;
    allowed_domains?: string[];
    blocked_domains?: string[];
    fallback_to_upstream?: boolean;
  } & Partial<Record<typeof limits[number]['key'], number>>;
}

interface GatewayKey {
  lineage_id: string;
  key_prefix: string;
  name: string;
  surfaces: string[];
  is_active: boolean;
  expires_at?: string | null;
  grace_period_ends_at?: string | null;
}

function keyLabel(key: GatewayKey) {
  return key.name + ' (' + key.key_prefix + ')';
}

function KeyPicker({
  selected, onChange,
}: {
  selected: string[];
  onChange: (keys: string[]) => void;
}) {
  const { t } = useTranslation();
  const [search, setSearch] = useState('');
  const keys = useQuery({
    queryKey: ['gateway-web-tool-keys'],
    queryFn: async () => {
      const rows = await fetchAllPaginated<GatewayKey>('/api/keys', 100, 'data');
      const now = Date.now();
      const eligible = new Map<string, GatewayKey>();
      for (const key of rows) {
        if (key.lineage_id && key.is_active && key.surfaces.includes('ai_gateway')
          && (!key.expires_at || Date.parse(key.expires_at) > now)
          && (!key.grace_period_ends_at || Date.parse(key.grace_period_ends_at) > now)
          && !eligible.has(key.lineage_id)) {
          eligible.set(key.lineage_id, key);
        }
      }
      return eligible;
    },
  });
  const available = keys.data;
  const matching = [...(available?.values() ?? [])]
    .filter((key) => !selected.includes(key.lineage_id))
    .filter((key) => keyLabel(key).toLowerCase().includes(search.trim().toLowerCase()));

  return (
    <div className="space-y-2">
      <GatewayField label={t('settings.hostedWeb.keys')}>
        {(id) => <Input id={id} placeholder={t('settings.hostedWeb.searchKeys')} value={search} onChange={(e) => setSearch(e.target.value)} />}
      </GatewayField>
      {selected.length > 0 && (
        <ul className="flex flex-wrap gap-2">
          {selected.map((id) => {
            const key = available?.get(id);
            return (
              <li key={id} className="flex items-center gap-1 rounded-md border px-2 text-sm">
                <span>{key ? keyLabel(key) : t('settings.hostedWeb.unavailableKey', { id: id.slice(0, 8) })}</span>
                <Button type="button" variant="ghost" size="icon-xs"
                  aria-label={t('settings.hostedWeb.removeKey', { name: key?.name ?? id.slice(0, 8) })}
                  onClick={() => onChange(selected.filter((key) => key !== id))}><X /></Button>
              </li>
            );
          })}
        </ul>
      )}
      {keys.isPending && <p role="status" className="text-sm text-muted-foreground">{t('common.loading')}</p>}
      {keys.isError && (
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <p role="alert" className="text-destructive">{t('settings.hostedWeb.keysFailed')}</p>
          <Button type="button" variant="outline" size="sm" onClick={() => void keys.refetch()}>{t('common.retry')}</Button>
        </div>
      )}
      {keys.isSuccess && matching.length === 0 && <p className="text-sm text-muted-foreground">{t('settings.hostedWeb.noKeys')}</p>}
      {matching.length > 0 && (
        <ul className="max-h-44 divide-y overflow-y-auto rounded-md border">
          {matching.map((key) => (
            <li key={key.lineage_id} className="flex items-center justify-between gap-2 px-3 py-1.5">
              <span className="break-all text-sm">{keyLabel(key)}</span>
              <Button type="button" variant="outline" size="sm" disabled={selected.length >= 256}
                aria-label={t('settings.hostedWeb.addKey', { name: keyLabel(key) })}
                onClick={() => onChange([...selected, key.lineage_id])}>{t('common.add')}</Button>
            </li>
          ))}
        </ul>
      )}
      {selected.length === 0 && <p className="text-sm text-muted-foreground">{t('settings.hostedWeb.requireKey')}</p>}
    </div>
  );
}

function domains(text: string) {
  return [...new Set(text.split(/[,\s]+/).filter(Boolean))];
}

function draftFrom(settings: HostedWebConfig) {
  return {
    enabled_lineages: settings.enabled_lineages ?? [],
    execution: settings.execution ?? {},
    allow: (settings.execution?.allowed_domains ?? []).join('\n'),
    block: (settings.execution?.blocked_domains ?? []).join('\n'),
  };
}

export function HostedWebSettings({
  initial, persist, loadLatest,
}: {
  initial: HostedWebConfig;
  persist: (value: HostedWebConfig) => Promise<void>;
  loadLatest?: () => Promise<HostedWebConfig>;
}) {
  const { t } = useTranslation();
  const form = useGatewayForm(draftFrom(initial), ({ enabled_lineages, execution, allow, block }) => persist({
    enabled_lineages,
    execution: { ...execution, allowed_domains: domains(allow), blocked_domains: domains(block) },
  }), loadLatest && (async () => draftFrom(await loadLatest())));
  const { execution, enabled_lineages } = form.draft;
  const enabled = execution.mode === 'core';
  const order = execution.engines ?? [];
  const automatic = order.length === 0;
  const displayed = automatic ? engines : [...order, ...engines.filter((engine) => !order.includes(engine))];
  const invalidLimits = limits.some(({ key, min, max, default: fallback }) => {
    const value = execution[key] ?? fallback;
    return !Number.isInteger(value) || value < min || value > max;
  });

  function update(patch: HostedWebConfig['execution']) {
    form.update({ ...form.draft, execution: { ...execution, ...patch } });
  }

  function move(engine: Engine, offset: number) {
    const next = [...order];
    const from = next.indexOf(engine);
    next.splice(from, 1);
    next.splice(from + offset, 0, engine);
    update({ engines: next });
  }

  return (
    <Card>
      <CardHeader><CardTitle className="text-base">{t('settings.hostedWeb.title')}</CardTitle></CardHeader>
      <CardContent>
        <fieldset disabled={form.busy} className="max-w-2xl space-y-5">
          <div className="flex items-center gap-2">
            <Switch id="hosted-web-enabled" checked={enabled} onCheckedChange={(checked) => update({ mode: checked ? 'core' : 'upstream' })} />
            <Label htmlFor="hosted-web-enabled">{t('settings.hostedWeb.enable')}</Label>
          </div>
          {enabled && <>
            <div className="space-y-2">
              <h3 className="text-sm font-medium">{t('settings.hostedWeb.sources')}</h3>
              <div className="flex items-center gap-2">
                <Switch id="hosted-web-auto" checked={automatic} onCheckedChange={(checked) => update({ engines: checked ? [] : [...engines] })} />
                <Label htmlFor="hosted-web-auto">{t('settings.hostedWeb.autoSources')}</Label>
              </div>
              <ol className="divide-y rounded-md border">
                {displayed.map((engine, i) => {
                  const checked = automatic || order.includes(engine);
                  return (
                    <li key={engine} className="flex flex-wrap items-center justify-between gap-2 px-3 py-2">
                      <div className="flex items-center gap-2">
                        {automatic ? <span className="min-w-4 text-xs text-muted-foreground">{i + 1}.</span> : (
                          <Checkbox id={'hosted-engine-' + engine} checked={checked} disabled={checked && order.length === 1}
                            onCheckedChange={(use) => update({ engines: use ? [...order, engine] : order.filter((id) => id !== engine) })} />
                        )}
                        {automatic ? <span className="text-sm">{engineNames[engine]}</span> : (
                          <Label htmlFor={'hosted-engine-' + engine}>{engineNames[engine]}</Label>
                        )}
                      </div>
                      {!automatic && checked && (
                        <div className="flex items-center gap-1">
                          <Button type="button" variant="ghost" size="icon-sm" disabled={i === 0}
                            aria-label={t('settings.hostedWeb.moveUp', { engine: engineNames[engine] })} onClick={() => move(engine, -1)}><ArrowUp /></Button>
                          <Button type="button" variant="ghost" size="icon-sm" disabled={i === order.length - 1}
                            aria-label={t('settings.hostedWeb.moveDown', { engine: engineNames[engine] })} onClick={() => move(engine, 1)}><ArrowDown /></Button>
                        </div>
                      )}
                    </li>
                  );
                })}
              </ol>
            </div>
            <KeyPicker selected={enabled_lineages} onChange={(keys) => form.update({ ...form.draft, enabled_lineages: keys })} />
            <details className="space-y-4">
              <summary className="cursor-pointer text-sm font-medium">{t('settings.hostedWeb.advanced')}</summary>
              <div className="grid gap-4 sm:grid-cols-2">
                <GatewayField label={t('settings.hostedWeb.allow')}>
                  {(id) => <Textarea id={id} rows={2} placeholder="example.org" value={form.draft.allow} onChange={(e) => form.update({ ...form.draft, allow: e.target.value })} />}
                </GatewayField>
                <GatewayField label={t('settings.hostedWeb.block')}>
                  {(id) => <Textarea id={id} rows={2} placeholder="example.org" value={form.draft.block} onChange={(e) => form.update({ ...form.draft, block: e.target.value })} />}
                </GatewayField>
                <GatewaySelect label={t('settings.hostedWeb.proxy')} value={execution.proxy ?? 'system'} disabled={form.busy}
                  options={[{ value: 'system', label: t('settings.hostedWeb.systemProxy') }, { value: 'direct', label: t('settings.hostedWeb.direct') }]}
                  onChange={(proxy) => update({ proxy })} />
                <GatewayField label={t('settings.hostedWeb.language')}>
                  {(id) => <Input id={id} placeholder="zh-Hans" value={execution.language ?? ''} onChange={(e) => update({ language: e.target.value })} />}
                </GatewayField>
                <GatewayField label={t('settings.hostedWeb.region')}>
                  {(id) => <Input id={id} placeholder="CN" maxLength={2} value={execution.region ?? ''} onChange={(e) => update({ region: e.target.value })} />}
                </GatewayField>
                {limits.map(({ key, label, min, max, default: fallback }) => (
                  <GatewayField key={key} label={t(`settings.hostedWeb.${label}`)}>
                    {(id) => <Input id={id} type="number" min={min} max={max}
                      value={Number.isNaN(execution[key]) ? '' : execution[key] ?? fallback}
                      onChange={(e) => update({ [key]: e.target.valueAsNumber })} />}
                  </GatewayField>
                ))}
              </div>
              <div className="flex items-center gap-2">
                <Switch id="hosted-web-fallback" checked={execution.fallback_to_upstream ?? false}
                  onCheckedChange={(checked) => update({ fallback_to_upstream: checked })} />
                <Label htmlFor="hosted-web-fallback">{t('settings.hostedWeb.fallback')}</Label>
              </div>
              {invalidLimits && <p role="alert" className="text-sm text-destructive">{t('settings.hostedWeb.invalidLimits')}</p>}
            </details>
          </>}
          <GatewaySave state={form.state} error={form.error}
            disabled={!form.dirty || form.busy || form.conflict || invalidLimits || (enabled && enabled_lineages.length === 0)}
            onSave={() => void form.save()} onReload={form.conflict && loadLatest ? () => void form.reload() : undefined} />
        </fieldset>
        {enabled && <HostedWebDiagnostic key={JSON.stringify(initial)} disabled={form.dirty || form.busy || form.conflict} />}
      </CardContent>
    </Card>
  );
}
