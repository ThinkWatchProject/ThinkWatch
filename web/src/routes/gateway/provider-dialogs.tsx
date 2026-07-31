import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import i18n from '@/i18n';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog';
import { AlertCircle, Zap, Loader2, CheckCircle2, XCircle, Check, ChevronDown } from 'lucide-react';
import { HeaderEditor } from '@/components/header-editor';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { apiPost, apiPatch } from '@/lib/api';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { toast } from 'sonner';
import type { Provider, TestResult } from './provider-types';

const defaultHeadersForType = (type: string): [string, string][] => {
  switch (type) {
    case 'openai': return [['Authorization', 'Bearer ']];
    case 'anthropic': return [['x-api-key', ''], ['anthropic-version', '2023-06-01']];
    case 'google': return [['x-goog-api-key', '']];
    case 'azure_openai': return [['api-key', '']];
    case 'bedrock': return [];
    default: return [];
  }
};

const authHeaderKey: Record<string, string | null> = {
  openai: 'Authorization',
  anthropic: 'x-api-key',
  google: 'x-goog-api-key',
  azure_openai: 'api-key',
  bedrock: null,
  custom: null,
};

const wrapAuthValue = (type: string, token: string) =>
  type === 'openai' ? `Bearer ${token}` : token;
const unwrapAuthValue = (type: string, value: string) =>
  type === 'openai' ? value.replace(/^Bearer\s*/i, '') : value;

const defaultBaseUrl: Record<string, string> = {
  openai: 'https://api.openai.com',
  anthropic: 'https://api.anthropic.com',
  google: 'https://generativelanguage.googleapis.com',
  bedrock: 'us-east-1',
};

function TestResultPanel({ testResult }: { testResult: TestResult | null }) {
  const { t } = useTranslation();
  if (!testResult) return null;
  const modelCount = testResult.model_count ?? testResult.models?.length;
  const showStats = testResult.success && testResult.latency_ms != null && modelCount != null;
  return (
    <div className="space-y-2">
      <Alert variant={testResult.success ? 'default' : 'destructive'}>
        {testResult.success ? <CheckCircle2 className="h-4 w-4" /> : <XCircle className="h-4 w-4" />}
        <AlertDescription>
          <div>{testResult.message}</div>
          {showStats && (
            <div className="mt-1 flex items-center gap-1 text-xs text-muted-foreground">
              <Check className="h-3 w-3" aria-hidden="true" />
              <span>
                {t('providers.testResult.latencyAndModels', {
                  latency: testResult.latency_ms,
                  count: modelCount,
                })}
              </span>
            </div>
          )}
        </AlertDescription>
      </Alert>
      {testResult.models && testResult.models.length > 0 && (
        <Collapsible className="space-y-2">
          <CollapsibleTrigger className="group flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground">
            <ChevronDown className="h-3 w-3 transition-transform group-data-[state=open]:rotate-180" />
            {t('providers.testResult.showModels')}
          </CollapsibleTrigger>
          <CollapsibleContent>
            <ScrollArea className="h-32 rounded-md border p-2">
              <ul className="space-y-0.5 text-xs font-mono">
                {testResult.models.map((m) => <li key={m}>{m}</li>)}
              </ul>
            </ScrollArea>
          </CollapsibleContent>
        </Collapsible>
      )}
    </div>
  );
}

function TestConnectionButton({
  testing,
  disabled,
  onClick,
}: {
  testing: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  const { t } = useTranslation();
  return (
    <Button type="button" variant="outline" disabled={disabled} onClick={onClick}>
      {testing ? <Loader2 className="mr-1 h-4 w-4 animate-spin" /> : <Zap className="mr-1 h-4 w-4" />}
      {testing ? t('providers.testing') : t('providers.testConnection')}
    </Button>
  );
}

async function handleTestConnection(
  type: string,
  url: string,
  hdrs: [string, string][],
  setTesting: (v: boolean) => void,
  setTestResult: (v: TestResult | null) => void,
  /**
   * Set when testing an already-saved provider: the server fills any
   * blank header value from that provider's stored secrets, which the
   * edit dialog never sees (they come back redacted).
   */
  providerId?: string,
) {
  setTesting(true);
  setTestResult(null);
  try {
    const res = await apiPost<TestResult>(
      '/api/admin/providers/test',
      {
        provider_type: type,
        base_url: url,
        headers: hdrs.filter(([k]) => k.trim()).map(([k, v]) => ({ key: k, value: v })),
        provider_id: providerId,
      },
    );
    setTestResult(res);
  } catch (err) {
    setTestResult({ success: false, message: err instanceof Error ? err.message : i18n.t('common.error') });
  } finally {
    setTesting(false);
  }
}

const headerPresets = (t: (key: string) => string) => [
  { label: t('mcpServers.presetUserId'), header: ['X-User-Id', '{{user_id}}'] as [string, string] },
  { label: t('mcpServers.presetUserEmail'), header: ['X-User-Email', '{{user_email}}'] as [string, string] },
];

// ---------------------------------------------------------------------------
// Create Provider Dialog
// ---------------------------------------------------------------------------

interface CreateProviderDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSuccess: () => Promise<void>;
}

export function CreateProviderDialog({ open, onOpenChange, onSuccess }: CreateProviderDialogProps) {
  const { t } = useTranslation();

  const [name, setName] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [providerType, setProviderType] = useState('openai');
  const [baseUrl, setBaseUrl] = useState('');
  const [headers, setHeaders] = useState<[string, string][]>(defaultHeadersForType('openai'));
  const [bedrockAuthMode, setBedrockAuthMode] = useState<'aksk' | 'imdsv2'>('aksk');
  const [awsAccessKeyId, setAwsAccessKeyId] = useState('');
  const [awsSecretKey, setAwsSecretKey] = useState('');
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  const resetForm = () => {
    setName('');
    setDisplayName('');
    setProviderType('openai');
    setBaseUrl('');
    setHeaders(defaultHeadersForType('openai'));
    setBedrockAuthMode('aksk');
    setAwsAccessKeyId('');
    setAwsSecretKey('');
    setFormError('');
    setTestResult(null);
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    setSubmitting(true);
    try {
      await apiPost('/api/admin/providers', {
        name,
        display_name: displayName,
        provider_type: providerType,
        base_url: baseUrl || defaultBaseUrl[providerType] || '',
        headers: headers.filter(([k]) => k.trim()).map(([k, v]) => ({ key: k, value: v })),
        ...(providerType === 'bedrock' && bedrockAuthMode === 'aksk' ? {
          config: { aws_access_key_id: awsAccessKeyId, aws_secret_access_key: awsSecretKey },
        } : {}),
      });
      onOpenChange(false);
      resetForm();
      toast.success(t('providers.providerCreated'));
      await onSuccess();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setSubmitting(false);
    }
  };

  const handleOpenChange = (v: boolean) => {
    if (!v) resetForm();
    onOpenChange(v);
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t('providers.addProvider')}</DialogTitle>
          <DialogDescription>{t('providers.dialogDescription')}</DialogDescription>
        </DialogHeader>
        <form onSubmit={handleCreate} className="space-y-4">
          <div className="space-y-2">
            <Label htmlFor="prov-name">{t('common.name')}</Label>
            <Input id="prov-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="my-openai" required />
          </div>
          <div className="space-y-2">
            <Label htmlFor="prov-display">{t('providers.displayName')}</Label>
            <Input id="prov-display" value={displayName} onChange={(e) => setDisplayName(e.target.value)} placeholder="OpenAI Production" />
          </div>
          <div className="space-y-2">
            <Label htmlFor="prov-type">{t('providers.providerType')}</Label>
            <Select value={providerType} onValueChange={(v) => { setProviderType(v ?? 'openai'); setHeaders(defaultHeadersForType(v ?? 'openai')); }}>
              <SelectTrigger><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="openai">OpenAI</SelectItem>
                <SelectItem value="anthropic">Anthropic</SelectItem>
                <SelectItem value="google">Google Gemini</SelectItem>
                <SelectItem value="azure_openai">Azure OpenAI</SelectItem>
                <SelectItem value="bedrock">AWS Bedrock</SelectItem>
                <SelectItem value="custom">Custom (OpenAI-compatible)</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-2">
            <Label htmlFor="prov-url">
              {providerType === 'bedrock' ? t('providers.awsRegion') :
               providerType === 'azure_openai' ? t('providers.azureEndpoint') :
               t('providers.baseUrl')}
            </Label>
            <Input id="prov-url" value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder={
              providerType === 'azure_openai' ? 'https://your-resource.openai.azure.com' :
              providerType === 'bedrock' ? 'us-east-1' :
              providerType === 'anthropic' ? 'https://api.anthropic.com' :
              providerType === 'google' ? 'https://generativelanguage.googleapis.com' :
              'https://api.openai.com'
            } />
          </div>
          {authHeaderKey[providerType] && (
            <div className="space-y-2">
              <Label htmlFor="prov-apikey">{t('providers.apiKey')}</Label>
              <Input
                id="prov-apikey"
                type="password"
                autoComplete="off"
                value={unwrapAuthValue(providerType, headers.find(([k]) => k === authHeaderKey[providerType])?.[1] ?? '')}
                onChange={(e) => {
                  const hk = authHeaderKey[providerType]!;
                  const wrapped = wrapAuthValue(providerType, e.target.value);
                  const exists = headers.some(([k]) => k === hk);
                  if (exists) {
                    setHeaders(headers.map(([k, v]) => k === hk ? [k, wrapped] : [k, v]));
                  } else {
                    setHeaders([[hk, wrapped], ...headers]);
                  }
                }}
                placeholder={providerType === 'openai' ? 'sk-...' : t('providers.apiKey')}
              />
            </div>
          )}
          {providerType === 'bedrock' && (
            <>
              <div className="space-y-2">
                <Label>{t('providers.awsAuthMode')}</Label>
                <Select value={bedrockAuthMode} onValueChange={(v) => setBedrockAuthMode(v as 'aksk' | 'imdsv2')}>
                  <SelectTrigger><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="aksk">{t('providers.awsAuthAkSk')}</SelectItem>
                    <SelectItem value="imdsv2">{t('providers.awsAuthImdsv2')}</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              {bedrockAuthMode === 'aksk' && (
                <>
                  <div className="space-y-2">
                    <Label htmlFor="prov-ak">Access Key ID</Label>
                    <Input id="prov-ak" value={awsAccessKeyId} onChange={(e) => setAwsAccessKeyId(e.target.value)} placeholder="AKIA..." required />
                  </div>
                  <div className="space-y-2">
                    <Label htmlFor="prov-sk">Secret Access Key</Label>
                    <Input id="prov-sk" value={awsSecretKey} onChange={(e) => setAwsSecretKey(e.target.value)} placeholder="wJalr..." required />
                  </div>
                </>
              )}
              {bedrockAuthMode === 'imdsv2' && (
                <p className="text-xs text-muted-foreground">{t('providers.awsImdsv2Hint')}</p>
              )}
            </>
          )}
          <div className="space-y-2">
            <Label>{t('providers.headers')}</Label>
            <p className="text-xs text-muted-foreground">{t('providers.headersDesc')}</p>
            <HeaderEditor
              headers={headers}
              onChange={setHeaders}
              presets={headerPresets(t)}
            />
          </div>
          <TestResultPanel testResult={testResult} />
          {formError && (
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{formError}</AlertDescription>
            </Alert>
          )}
          <DialogFooter>
            <TestConnectionButton
              testing={testing}
              disabled={testing || !baseUrl}
              onClick={() => handleTestConnection(providerType, baseUrl, headers, setTesting, setTestResult)}
            />
            <Button type="submit" disabled={submitting}>
              {submitting ? (
                <><Loader2 className="mr-1 h-4 w-4 animate-spin" />{t('providers.creating')}</>
              ) : t('providers.createProvider')}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Edit Provider Dialog
// ---------------------------------------------------------------------------

interface EditProviderDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  provider: Provider | null;
  onSuccess: () => Promise<void>;
}

export function EditProviderDialog({ open, onOpenChange, provider, onSuccess }: EditProviderDialogProps) {
  const { t } = useTranslation();

  const [editDisplayName, setEditDisplayName] = useState('');
  const [editBaseUrl, setEditBaseUrl] = useState('');
  const [editHeaders, setEditHeaders] = useState<[string, string][]>([]);
  /**
   * Header keys the server holds a secret for. Their values come back
   * redacted, so an untouched row stays blank and the PATCH preserves
   * what's stored — this set only drives the "saved" placeholder. Keyed
   * by header name rather than row index so removing or reordering rows
   * can't shift the flags onto the wrong header.
   */
  const [editStoredKeys, setEditStoredKeys] = useState<Set<string>>(new Set());
  const [editSaving, setEditSaving] = useState(false);
  const [editError, setEditError] = useState('');
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  // Sync local state when provider changes (dialog opens with a new provider)
  const [prevProvider, setPrevProvider] = useState<Provider | null>(null);
  if (provider !== prevProvider) {
    setPrevProvider(provider);
    if (provider) {
      setEditDisplayName(provider.display_name);
      setEditBaseUrl(provider.base_url);
      setEditError('');
      setTestResult(null);
      const existing = provider.config_json?.headers ?? [];
      // `value` is redacted server-side; coerce defensively so a legacy
      // row that still carries the raw `{"$enc": …}` envelope renders as
      // an empty input instead of the string "[object Object]" — which
      // would then be saved back as the provider's new secret.
      setEditHeaders(
        existing.map(h => [h.key, typeof h.value === 'string' ? h.value : ''] as [string, string]),
      );
      setEditStoredKeys(
        new Set(existing.filter(h => h.encrypted || typeof h.value !== 'string').map(h => h.key)),
      );
    }
  }

  const handleEdit = async () => {
    if (!provider) return;
    setEditError('');
    setEditSaving(true);
    try {
      await apiPatch(`/api/admin/providers/${provider.id}`, {
        display_name: editDisplayName,
        base_url: editBaseUrl,
        headers: editHeaders.filter(([k]) => k.trim()).map(([k, v]) => ({ key: k, value: v })),
      });
      onOpenChange(false);
      await onSuccess();
    } catch (err) {
      setEditError(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setEditSaving(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t('providers.editProvider')}</DialogTitle>
          <DialogDescription>{t('providers.editDescription')}</DialogDescription>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>{t('providers.displayName')}</Label>
            <Input value={editDisplayName} onChange={(e) => setEditDisplayName(e.target.value)} />
          </div>
          <div className="space-y-2">
            <Label>{t('providers.baseUrl')}</Label>
            <Input value={editBaseUrl} onChange={(e) => setEditBaseUrl(e.target.value)} />
          </div>
          {provider && authHeaderKey[provider.provider_type] && (
            <div className="space-y-2">
              <Label>{t('providers.apiKey')}</Label>
              <Input
                type="password"
                autoComplete="off"
                value={unwrapAuthValue(provider.provider_type, editHeaders.find(([k]) => k === authHeaderKey[provider.provider_type])?.[1] ?? '')}
                onChange={(e) => {
                  const hk = authHeaderKey[provider.provider_type]!;
                  const wrapped = wrapAuthValue(provider.provider_type, e.target.value);
                  const exists = editHeaders.some(([k]) => k === hk);
                  if (exists) {
                    setEditHeaders(editHeaders.map(([k, v]) => k === hk ? [k, wrapped] : [k, v]));
                  } else {
                    setEditHeaders([[hk, wrapped], ...editHeaders]);
                  }
                }}
                placeholder={
                  editStoredKeys.has(authHeaderKey[provider.provider_type]!)
                    ? t('providers.headerValueStored')
                    : t('providers.apiKey')
                }
              />
            </div>
          )}
          <div className="space-y-2">
            <Label>{t('providers.headers')}</Label>
            <p className="text-xs text-muted-foreground">{t('providers.headersDesc')}</p>
            <HeaderEditor
              headers={editHeaders}
              onChange={setEditHeaders}
              valuePlaceholder={(i) =>
                editHeaders[i][1] === '' && editStoredKeys.has(editHeaders[i][0])
                  ? t('providers.headerValueStored')
                  : undefined
              }
              presets={headerPresets(t)}
            />
          </div>
        </div>
        <TestResultPanel testResult={testResult} />
        {editError && (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>{editError}</AlertDescription>
          </Alert>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>{t('common.cancel')}</Button>
          <TestConnectionButton
            testing={testing}
            disabled={testing || !editBaseUrl}
            onClick={() => handleTestConnection(provider!.provider_type, editBaseUrl, editHeaders, setTesting, setTestResult, provider!.id)}
          />
          <Button onClick={handleEdit} disabled={editSaving}>
            {editSaving ? t('common.loading') : t('common.save')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
