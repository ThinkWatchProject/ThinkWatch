import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { apiPost } from '@/lib/api';
import { GatewayField, GatewaySelect } from './gateway-form';

type Action = 'search' | 'open_page' | 'find_in_page';

interface DiagnosticResult {
  sources: { url: string; title: string; text: string; engine: string | null }[];
  elapsed_ms: number;
  cached: boolean;
  truncated: boolean;
  warnings: { text: string }[];
}

const engineNames: Record<string, string> = { google_docs: 'Google Docs', duckduckgo: 'DuckDuckGo', bing: 'Bing' };

function publicLink(value: string) {
  try {
    const url = new URL(value);
    return url.protocol === 'https:' || url.protocol === 'http:' ? url.href : undefined;
  } catch {
    return undefined;
  }
}

export function HostedWebDiagnostic({ disabled }: { disabled: boolean }) {
  const { t } = useTranslation();
  const [action, setAction] = useState<Action>('search');
  const [input, setInput] = useState('');
  const [pattern, setPattern] = useState('');
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<DiagnosticResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (disabled || busy || !input.trim() || (action === 'find_in_page' && !pattern.trim())) return;
    const request = action === 'search' ? { action, query: input.trim() }
      : action === 'open_page' ? { action, url: input.trim() }
      : { action, url: input.trim(), pattern: pattern.trim() };
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      setResult(await apiPost<DiagnosticResult>('/api/admin/settings/web-tools/test', request));
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : t('settings.hostedWeb.testFailed'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(event) => void submit(event)} className="mt-5 max-w-2xl space-y-3 border-t pt-4">
      <h3 className="text-sm font-medium">{t('settings.hostedWeb.diagnostic')}</h3>
      {disabled && <p className="text-sm text-muted-foreground">{t('settings.hostedWeb.saveToTest')}</p>}
      <fieldset disabled={disabled || busy} className="space-y-3">
        <div className="grid gap-3 sm:grid-cols-2">
          <GatewaySelect label={t('settings.hostedWeb.testAction')} value={action} disabled={disabled || busy} options={[
            { value: 'search', label: t('settings.hostedWeb.testSearch') },
            { value: 'open_page', label: t('settings.hostedWeb.testOpen') },
            { value: 'find_in_page', label: t('settings.hostedWeb.testFind') },
          ]} onChange={(value) => {
            setAction(value as Action);
            setInput('');
            setResult(null);
            setError(null);
          }} />
          <GatewayField label={action === 'search' ? t('settings.hostedWeb.testQuery') : t('settings.hostedWeb.testUrl')}>
            {(id) => <Input id={id} type={action === 'search' ? 'search' : 'url'} value={input}
              onChange={(event) => setInput(event.target.value)} required />}
          </GatewayField>
          {action === 'find_in_page' && <GatewayField label={t('settings.hostedWeb.findText')}>
            {(id) => <Input id={id} value={pattern} onChange={(event) => setPattern(event.target.value)} required />}
          </GatewayField>}
        </div>
        <Button type="submit" variant="outline" disabled={!input.trim() || (action === 'find_in_page' && !pattern.trim())}>
          {busy ? t('common.loading') : t('settings.hostedWeb.runTest')}
        </Button>
      </fieldset>
      {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      {result && !disabled && <div className="space-y-2">
        <p role="status" className="text-sm text-muted-foreground">
          {t('settings.hostedWeb.testResult', { count: result.sources.length, ms: result.elapsed_ms })}
          {result.cached ? ' · ' + t('settings.hostedWeb.testCached') : ''}
          {result.truncated ? ' · ' + t('settings.hostedWeb.testTruncated') : ''}
        </p>
        {result.warnings.map((warning, index) => <p key={index} className="text-sm text-muted-foreground">{warning.text}</p>)}
        <ul className="max-h-96 space-y-2 overflow-y-auto">
          {result.sources.map((source, index) => {
            const url = publicLink(source.url);
            return (
              <li key={source.url + ':' + index} className="space-y-1 rounded-md border p-3 text-sm">
                {url ? <a href={url} target="_blank" rel="noreferrer" className="break-all font-medium underline">{source.title || source.url}</a>
                  : <span className="font-medium">{source.title || source.url}</span>}
                {source.engine && <p className="text-xs text-muted-foreground">{engineNames[source.engine] ?? source.engine}</p>}
                <p className="max-h-48 overflow-y-auto whitespace-pre-wrap break-words">{source.text}</p>
              </li>
            );
          })}
        </ul>
      </div>}
    </form>
  );
}
