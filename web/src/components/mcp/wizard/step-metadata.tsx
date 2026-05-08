import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle, ArrowLeft, Info, Loader2 } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from '@/components/ui/collapsible';
import { ChevronDown } from 'lucide-react';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { HeaderEditor } from '@/components/header-editor';
import {
  resolveCollision,
  sanitizePrefixInput,
  slugifyPrefix,
} from '@/lib/prefix-utils';
import type { WizardState } from './types';

interface Props {
  state: WizardState;
  patch: (partial: Partial<WizardState>) => void;
  taken: { names: Set<string>; prefixes: Set<string> };
  onBack: () => void;
  onSubmit: () => Promise<void>;
  submitting: boolean;
  submitError: string;
}

/**
 * Step 4 — name, namespace prefix, description, custom_headers,
 * cache TTL, and the live test panel. Save button is enabled once
 * required fields are filled (name + endpoint URL — the latter
 * already validated in Step 1).
 *
 * Custom headers stay in the advanced collapsible because their
 * `{{user_id}}` / `{{user_email}}` template substitution is a
 * separate concern from the auth header — we already explicitly
 * told the admin in Step 2 that those don't carry credentials.
 */
export function StepMetadata({
  state,
  patch,
  taken,
  onBack,
  onSubmit,
  submitting,
  submitError,
}: Props) {
  const { t } = useTranslation();
  const [prefixManuallyEdited, setPrefixManuallyEdited] = useState(
    !!state.namespace_prefix,
  );

  // Auto-derive prefix from name unless the admin has typed one.
  const resolved = useMemo(() => {
    if (!state.name.trim()) return null;
    const base =
      prefixManuallyEdited && state.namespace_prefix
        ? state.namespace_prefix
        : slugifyPrefix(state.name);
    if (!base) return null;
    return resolveCollision(state.name.trim(), base, taken.names, taken.prefixes);
  }, [state.name, state.namespace_prefix, prefixManuallyEdited, taken]);

  // Mirror the auto-derived prefix into wizard state so the submit
  // payload uses the deconflicted value (resolved.name / .prefix).
  useEffect(() => {
    if (!resolved) return;
    if (!prefixManuallyEdited && state.namespace_prefix !== resolved.prefix) {
      patch({ namespace_prefix: resolved.prefix });
    }
  }, [resolved, prefixManuallyEdited, state.namespace_prefix, patch]);

  const canSubmit =
    !!state.name.trim() && !!state.endpoint_url.trim() && !submitting;

  // For anonymous probes the wizard skipped Step 2 + Step 3 entirely,
  // so the only place we get to surface "what we found and why we
  // skipped" is here. Step 2 has its own ProbeSummary; this one is
  // intentionally only shown for anonymous to avoid duplication.
  const showAnonymousSummary =
    state.probe?.anonymous_ok && state.auth_shape === 'anonymous';

  return (
    <div className="space-y-4">
      {submitError && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{submitError}</AlertDescription>
        </Alert>
      )}

      {showAnonymousSummary && state.probe && (
        <div className="rounded-md border border-emerald-500/30 bg-emerald-500/10 p-3 text-xs text-emerald-900 dark:text-emerald-200">
          <div className="flex items-start gap-2">
            <Info className="mt-0.5 h-4 w-4 shrink-0 text-emerald-600 dark:text-emerald-300" />
            <div className="flex-1 space-y-1">
              <p className="font-medium">
                {t('mcpServers.wizard.probeSummary.anonymousTitle')}
              </p>
              <p className="opacity-85">
                {t('mcpServers.wizard.probeSummary.probedUrl')}{' '}
                <code className="font-mono">{state.endpoint_url}</code>
              </p>
              <p className="opacity-85">
                {t('mcpServers.wizard.probeSummary.anonymousDetail', {
                  count: state.probe.tools.length,
                })}
              </p>
            </div>
          </div>
        </div>
      )}

      <div className="space-y-2">
        <Label htmlFor="wiz-name">{t('common.name')} *</Label>
        <Input
          id="wiz-name"
          value={state.name}
          onChange={(e) => patch({ name: e.target.value })}
          placeholder="my-mcp-server"
          required
          autoFocus
        />
      </div>

      <div className="space-y-2">
        <Label htmlFor="wiz-prefix">{t('mcpServers.namespacePrefix')}</Label>
        <Input
          id="wiz-prefix"
          value={
            prefixManuallyEdited
              ? state.namespace_prefix
              : (resolved?.prefix ?? slugifyPrefix(state.name))
          }
          onChange={(e) => {
            setPrefixManuallyEdited(true);
            patch({ namespace_prefix: sanitizePrefixInput(e.target.value) });
          }}
          placeholder={t('mcpServers.namespacePrefixPlaceholder')}
          pattern="[a-z0-9_]{1,32}"
          maxLength={32}
        />
        {resolved && (
          <p className="text-xs text-muted-foreground">
            {t('mcpServers.willBeStoredAs')}{' '}
            <code className="rounded bg-muted px-1 font-mono">{resolved.name}</code>
            {' / '}
            <code className="rounded bg-muted px-1 font-mono">{resolved.prefix}</code>
          </p>
        )}
        <p className="text-xs text-muted-foreground">
          {t('mcpServers.namespacePrefixHint')}
        </p>
      </div>

      <div className="space-y-2">
        <Label htmlFor="wiz-display">{t('mcpServers.displayLabel')}</Label>
        <Input
          id="wiz-display"
          value={state.display_label}
          onChange={(e) => patch({ display_label: e.target.value })}
          placeholder={t('mcpServers.displayLabelPlaceholder')}
          maxLength={120}
        />
        <p className="text-xs text-muted-foreground">{t('mcpServers.displayLabelHint')}</p>
      </div>

      <div className="space-y-2">
        <Label htmlFor="wiz-desc">{t('common.description')}</Label>
        <Input
          id="wiz-desc"
          value={state.description}
          onChange={(e) => patch({ description: e.target.value })}
        />
      </div>

      <Collapsible className="space-y-2">
        <CollapsibleTrigger className="group flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground">
          <ChevronDown className="h-3 w-3 transition-transform group-data-[state=open]:rotate-180" />
          {t('mcpServers.wizard.advancedSection')}
        </CollapsibleTrigger>
        <CollapsibleContent className="space-y-3 pt-2">
          <div className="space-y-2">
            <Label>{t('providers.customHeaders')}</Label>
            <p className="text-xs text-muted-foreground">
              {t('providers.customHeadersDesc')}
            </p>
            <HeaderEditor
              headers={state.custom_headers}
              onChange={(h) => patch({ custom_headers: h })}
              keyPlaceholder="X-Custom-Header"
              presets={[
                { label: t('mcpServers.presetUserId'), header: ['X-User-Id', '{{user_id}}'] },
                { label: t('mcpServers.presetUserEmail'), header: ['X-User-Email', '{{user_email}}'] },
              ]}
            />
          </div>
          <div className="space-y-2">
            <Label>{t('mcpServers.cacheTtlLabel')}</Label>
            <p className="text-xs text-muted-foreground">
              {t('mcpServers.cacheTtlHint')}
            </p>
            <Input
              type="number"
              min={0}
              step={60}
              placeholder={t('mcpServers.cacheTtlPlaceholder')}
              value={state.cache_ttl_secs}
              onChange={(e) => patch({ cache_ttl_secs: e.target.value })}
            />
          </div>
        </CollapsibleContent>
      </Collapsible>

      <div className="flex items-center justify-between gap-2 pt-2">
        <Button variant="outline" type="button" onClick={onBack}>
          <ArrowLeft className="h-4 w-4" />
          {t('common.back')}
        </Button>
        <Button type="button" onClick={onSubmit} disabled={!canSubmit}>
          {submitting && <Loader2 className="h-4 w-4 animate-spin" />}
          {submitting
            ? t('mcpServers.registering')
            : t('mcpServers.registerServer')}
        </Button>
      </div>
    </div>
  );
}
