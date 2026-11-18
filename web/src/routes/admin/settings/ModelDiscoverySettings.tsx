import { useId, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Plus, X } from 'lucide-react';
import { ComboboxInput } from '@/components/combobox-input';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { api } from '@/lib/api';
import { GatewayField, GatewaySave, GatewaySelect } from './gateway-form';
import { useGatewayForm } from './useGatewayForm';

const levels = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'] as const;
type Level = typeof levels[number];
type History = 'auto' | 'reasoning-content' | 'reasoning' | 'content-blocks' | 'disabled';

interface ModelSpec {
  context_window?: number;
  max_output_tokens?: number;
  reasoning?: boolean;
  image_input?: boolean;
  reasoning_history?: History;
  reasoning_effort?: { supported_levels: Level[]; default_level?: Level };
}

interface Alias {
  target: string;
  hide_original?: boolean;
}

export interface ModelDiscoveryConfig {
  aliases?: Record<string, Alias>;
  profiles?: Record<string, Record<string, ModelSpec>>;
}

interface AliasRow extends Alias {
  id: string;
  name: string;
}

interface ProfileRow {
  id: string;
  provider: string;
  model: string;
  spec: ModelSpec;
}

interface Provider { id: string; name: string }

function initialRows(config: ModelDiscoveryConfig) {
  return {
    aliases: Object.entries(config.aliases ?? {}).map(([name, alias]): AliasRow => ({ id: 'saved-' + name, name, ...alias })),
    profiles: Object.entries(config.profiles ?? {}).flatMap(([provider, models]) =>
      Object.entries(models).map(([model, spec]): ProfileRow => ({ id: provider + ':' + model, provider, model, spec: definedSpec(spec) }))),
  };
}

function definedSpec(spec: ModelSpec): ModelSpec {
  return Object.fromEntries(Object.entries(spec).filter(([, value]) => value != null));
}

function serialize({ aliases, profiles }: ReturnType<typeof initialRows>): ModelDiscoveryConfig {
  const byProvider = new Map<string, [string, ModelSpec][]>();
  for (const { provider, model, spec } of profiles) {
    const models = byProvider.get(provider) ?? [];
    models.push([model.trim(), definedSpec(spec)]);
    byProvider.set(provider, models);
  }
  return {
    aliases: Object.fromEntries(aliases.map(({ name, target, hide_original }) => [name.trim(), { target: target.trim(), hide_original }])),
    profiles: Object.fromEntries([...byProvider].map(([provider, models]) => [provider, Object.fromEntries(models)])),
  };
}

function invalidRows({ aliases, profiles }: ReturnType<typeof initialRows>) {
  const names = new Set(aliases.map((row) => row.name.trim()));
  const exact = (name: string) => name.trim().length > 0 && !/[\s*?]/.test(name.trim());
  return names.size !== aliases.length ||
    aliases.some((row) => !exact(row.name) || !exact(row.target) || names.has(row.target.trim())) ||
    new Set(profiles.map((p) => p.provider + ':' + p.model.trim())).size !== profiles.length ||
    profiles.some(({ provider, model, spec }) =>
      !provider || !exact(model) || Object.keys(definedSpec(spec)).length === 0 ||
      [spec.context_window, spec.max_output_tokens].some((value) =>
        value !== undefined && (!Number.isInteger(value) || value < 1 || value > 4294967295)));
}

function AliasFields({ row, targets, loading, onChange, onRemove }: {
  row: AliasRow;
  targets: string[];
  loading: boolean;
  onChange: (row: AliasRow) => void;
  onRemove: () => void;
}) {
  const { t } = useTranslation();
  const checkboxId = useId();
  return (
    <div className="space-y-3 rounded-md border p-3">
      <div className="grid gap-3 sm:grid-cols-2">
        <GatewayField label={t('settings.modelDiscovery.aliasName')}>
          {(id) => <Input id={id} maxLength={256} value={row.name} onChange={(e) => onChange({ ...row, name: e.target.value })} />}
        </GatewayField>
        <GatewayField label={t('settings.modelDiscovery.target')}>
          {(id) => <ComboboxInput id={id} value={row.target} options={targets} loading={loading}
            loadingText={t('common.loading')} onChange={(target) => onChange({ ...row, target })} />}
        </GatewayField>
      </div>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <Checkbox id={checkboxId} checked={row.hide_original ?? false}
            onCheckedChange={(checked) => onChange({ ...row, hide_original: checked === true })} />
          <Label htmlFor={checkboxId}>{t('settings.modelDiscovery.hideOriginal')}</Label>
        </div>
        <Button type="button" variant="ghost" size="sm" onClick={onRemove}><X />{t('common.remove')}</Button>
      </div>
    </div>
  );
}

function ProfileFields({ row, providers, busy, onChange, onRemove }: {
  row: ProfileRow;
  providers: Provider[];
  busy: boolean;
  onChange: (row: ProfileRow) => void;
  onRemove: () => void;
}) {
  const { t } = useTranslation();
  const checkboxId = useId();
  const { spec } = row;
  const [browseUpstream, setBrowseUpstream] = useState(false);
  const remote = useQuery({
    queryKey: ['gateway-discovery-remote-models', row.provider],
    enabled: browseUpstream && providers.some((provider) => provider.id === row.provider),
    queryFn: ({ signal }) => api<{ id: string }[]>('/api/admin/providers/' + encodeURIComponent(row.provider) + '/remote-models', { signal }),
  });
  const providerOptions = providers.map(({ id, name }) => ({ value: id, label: name }));
  if (row.provider && !providerOptions.some((option) => option.value === row.provider)) {
    providerOptions.push({ value: row.provider, label: t('settings.modelDiscovery.unavailableProvider', { id: row.provider.slice(0, 8) }) });
  }
  const automatic = { value: 'auto', label: t('settings.auto') };
  const booleanOptions = [
    automatic,
    { value: 'yes', label: t('settings.modelDiscovery.yes') },
    { value: 'no', label: t('settings.modelDiscovery.no') },
  ];
  const booleanValue = (value?: boolean) => value === undefined ? 'auto' : value ? 'yes' : 'no';
  const parseBoolean = (value: string) => value === 'auto' ? undefined : value === 'yes';
  const effortMode = !spec.reasoning_effort ? 'auto' : spec.reasoning_effort.supported_levels.length === 0 ? 'fixed' : 'manual';
  const supported = spec.reasoning_effort?.supported_levels ?? [];

  function patch(patch: ModelSpec) {
    onChange({ ...row, spec: { ...spec, ...patch } });
  }

  function setLevel(level: Level, checked: boolean) {
    const next = new Set(supported);
    if (checked) next.add(level);
    else next.delete(level);
    const currentDefault = spec.reasoning_effort?.default_level;
    patch({ reasoning_effort: {
      supported_levels: levels.filter((item) => next.has(item)),
      default_level: currentDefault && next.has(currentDefault) ? currentDefault : undefined,
    } });
  }

  return (
    <div className="space-y-4 rounded-md border p-3">
      <div className="grid gap-3 sm:grid-cols-2">
        <GatewaySelect label={t('settings.modelDiscovery.provider')} value={row.provider} disabled={busy}
          placeholder={t('settings.modelDiscovery.selectProvider')} options={providerOptions}
          onChange={(provider) => {
            setBrowseUpstream(true);
            onChange({ ...row, provider, model: '' });
          }} />
        <div onFocus={() => setBrowseUpstream(true)}>
          <GatewayField label={t('settings.modelDiscovery.upstreamModel')}>
            {(id) => <ComboboxInput id={id} value={row.model} options={remote.data?.map((model) => model.id) ?? []}
              loading={remote.isLoading} loadingText={t('common.loading')} onChange={(model) => onChange({ ...row, model })} />}
          </GatewayField>
        </div>
        <GatewayField label={t('settings.modelDiscovery.contextWindow')}>
          {(id) => <Input id={id} type="number" min={1} placeholder={t('settings.auto')} value={spec.context_window ?? ''}
            onChange={(e) => patch({ context_window: e.target.value === '' ? undefined : e.target.valueAsNumber })} />}
        </GatewayField>
        <GatewayField label={t('settings.modelDiscovery.maxOutput')}>
          {(id) => <Input id={id} type="number" min={1} placeholder={t('settings.auto')} value={spec.max_output_tokens ?? ''}
            onChange={(e) => patch({ max_output_tokens: e.target.value === '' ? undefined : e.target.valueAsNumber })} />}
        </GatewayField>
        <GatewaySelect label={t('settings.modelDiscovery.images')} value={booleanValue(spec.image_input)} disabled={busy}
          options={booleanOptions} onChange={(value) => patch({ image_input: parseBoolean(value) })} />
        <GatewaySelect label={t('settings.modelDiscovery.reasoning')} value={booleanValue(spec.reasoning)} disabled={busy}
          options={booleanOptions} onChange={(value) => patch({
            reasoning: parseBoolean(value),
            reasoning_effort: value === 'no' && effortMode === 'manual' ? undefined : spec.reasoning_effort,
          })} />
        <GatewaySelect label={t('settings.modelDiscovery.history')} value={spec.reasoning_history ?? 'auto'} disabled={busy}
          options={[
            automatic,
            { value: 'reasoning-content', label: 'reasoning_content' },
            { value: 'reasoning', label: 'reasoning' },
            { value: 'content-blocks', label: t('settings.modelDiscovery.historyBlocks') },
            { value: 'disabled', label: t('settings.modelDiscovery.historyDisabled') },
          ]}
          onChange={(value) => patch({ reasoning_history: value === 'auto' ? undefined : value as History })} />
        <GatewaySelect label={t('settings.modelDiscovery.effort')} value={effortMode} disabled={busy}
          options={[
            automatic,
            { value: 'fixed', label: t('settings.modelDiscovery.effortFixed') },
            ...(spec.reasoning === false ? [] : [{ value: 'manual', label: t('settings.modelDiscovery.effortManual') }]),
          ]}
          onChange={(value) => patch({
            reasoning_effort: value === 'auto' ? undefined : { supported_levels: value === 'fixed' ? [] : ['medium'] },
          })} />
      </div>
      {effortMode === 'manual' && (
        <div className="space-y-3">
          <div className="flex flex-wrap gap-3">
            {levels.map((level) => (
              <div className="flex items-center gap-1.5" key={level}>
                <Checkbox id={checkboxId + '-' + level} checked={supported.includes(level)}
                  disabled={supported.length === 1 && supported.includes(level)}
                  onCheckedChange={(checked) => setLevel(level, checked === true)} />
                <Label htmlFor={checkboxId + '-' + level}>{level}</Label>
              </div>
            ))}
          </div>
          <GatewaySelect label={t('settings.modelDiscovery.effortDefault')} value={spec.reasoning_effort?.default_level ?? 'auto'} disabled={busy}
            options={[automatic, ...supported.map((level) => ({ value: level, label: level }))]}
            onChange={(value) => patch({ reasoning_effort: { supported_levels: supported, default_level: value === 'auto' ? undefined : value as Level } })} />
        </div>
      )}
      <Button type="button" variant="ghost" size="sm" onClick={onRemove}><X />{t('common.remove')}</Button>
    </div>
  );
}

export function ModelDiscoverySettings({ initial, persist, loadLatest }: {
  initial: ModelDiscoveryConfig;
  persist: (value: ModelDiscoveryConfig) => Promise<void>;
  loadLatest?: () => Promise<ModelDiscoveryConfig>;
}) {
  const { t } = useTranslation();
  const sequence = useRef(0);
  const newRowId = () => 'draft-' + ++sequence.current;
  const form = useGatewayForm(initialRows(initial), (rows) => persist(serialize(rows)),
    loadLatest && (async () => initialRows(await loadLatest())));
  const { aliases, profiles } = form.draft;
  const modelList = useQuery({
    queryKey: ['gateway-discovery-models'],
    enabled: aliases.length > 0,
    queryFn: ({ signal }) => api<{ model_id: string }[]>('/api/admin/models/ids', { signal }),
  });
  const providers = useQuery({
    queryKey: ['gateway-discovery-providers'],
    enabled: profiles.length > 0,
    queryFn: ({ signal }) => api<Provider[]>('/api/admin/providers', { signal }),
  });
  const targets = modelList.data?.map((model) => model.model_id) ?? [];
  const collisions = new Set(aliases.map((alias) => alias.name.trim()).filter((name) => targets.includes(name)));
  const invalid = invalidRows(form.draft) || collisions.size !== 0;

  return (
    <Card>
      <CardHeader><CardTitle className="text-base">{t('settings.modelDiscovery.title')}</CardTitle></CardHeader>
      <CardContent>
        <fieldset disabled={form.busy} className="max-w-2xl space-y-5">
          <section className="space-y-2">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="text-sm font-medium">{t('settings.modelDiscovery.aliases')}</h3>
              <Button type="button" size="sm" variant="outline" disabled={aliases.length >= 256}
                onClick={() => form.update({ ...form.draft, aliases: [...aliases, { id: newRowId(), name: '', target: '' }] })}>
                <Plus />{t('settings.modelDiscovery.addAlias')}
              </Button>
            </div>
            {aliases.map((row) => <AliasFields key={row.id} row={row} targets={targets}
              loading={modelList.isLoading}
              onChange={(next) => form.update({ ...form.draft, aliases: aliases.map((alias) => alias.id === next.id ? next : alias) })}
              onRemove={() => form.update({ ...form.draft, aliases: aliases.filter((alias) => alias.id !== row.id) })} />)}
            {collisions.size > 0 && <p role="alert" className="text-sm text-destructive">
              {t('settings.modelDiscovery.nameCollision', { models: [...collisions].join(', ') })}
            </p>}
          </section>
          <section className="space-y-2">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="text-sm font-medium">{t('settings.modelDiscovery.profiles')}</h3>
              <Button type="button" size="sm" variant="outline" disabled={profiles.length >= 1024}
                onClick={() => form.update({ ...form.draft, profiles: [...profiles, { id: newRowId(), provider: '', model: '', spec: {} }] })}>
                <Plus />{t('settings.modelDiscovery.addProfile')}
              </Button>
            </div>
            {providers.isError && (
              <div className="flex flex-wrap items-center gap-2">
                <p role="alert" className="text-sm text-destructive">{t('settings.modelDiscovery.providersFailed')}</p>
                <Button type="button" variant="outline" size="sm" onClick={() => void providers.refetch()}>{t('common.retry')}</Button>
              </div>
            )}
            {profiles.map((row) => <ProfileFields key={row.id} row={row} providers={providers.data ?? []} busy={form.busy}
              onChange={(next) => form.update({ ...form.draft, profiles: profiles.map((profile) => profile.id === next.id ? next : profile) })}
              onRemove={() => form.update({ ...form.draft, profiles: profiles.filter((profile) => profile.id !== row.id) })} />)}
          </section>
          {invalid && <p className="text-sm text-muted-foreground">{t('settings.modelDiscovery.incomplete')}</p>}
          <GatewaySave state={form.state} error={form.error}
            disabled={!form.dirty || form.busy || form.conflict || invalid}
            onSave={() => void form.save()} onReload={form.conflict && loadLatest ? () => void form.reload() : undefined} />
        </fieldset>
      </CardContent>
    </Card>
  );
}
