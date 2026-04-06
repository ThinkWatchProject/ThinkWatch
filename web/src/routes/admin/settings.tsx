import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Separator } from '@/components/ui/separator';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  Select,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Settings, Shield, Key, DollarSign, Database, Lock, Plus, Trash2, AlertCircle, CheckCircle, FlaskConical, Sparkles } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { api, apiPatch, apiPost } from '@/lib/api';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface SystemInfo {
  version: string;
  uptime: string;
  rust_version: string;
  server_host: string;
  gateway_port: number;
  console_port: number;
  public_protocol: string;
  public_host: string;
  public_port: number;
}

interface OidcConfig {
  issuer_url: string | null;
  client_id: string | null;
  redirect_url: string | null;
  enabled: boolean;
  has_secret?: boolean;
}

interface AuditConfig {
  clickhouse_url: string;
  clickhouse_db: string;
  connected: boolean;
}

interface SettingEntry {
  key: string;
  value: unknown;
  category: string;
  description: string;
  updated_at: string;
}

interface ContentFilterRule {
  name: string;
  pattern: string;
  match_type: 'contains' | 'regex';
  action: 'block' | 'warn' | 'log';
}

interface ContentFilterPreset {
  id: string;
  rules: ContentFilterRule[];
}

interface ContentFilterTestMatch {
  name: string;
  pattern: string;
  match_type: string;
  action: string;
  matched_snippet: string;
}

interface PiiPattern {
  name: string;
  regex: string;
  placeholder_prefix: string;
}

interface PiiTestMatch {
  name: string;
  original: string;
  placeholder: string;
}

interface PiiTestResponse {
  redacted_text: string;
  matches: PiiTestMatch[];
}

/// Normalize a rule loaded from the backend, accepting both the new schema
/// (name/match_type/action) and the legacy schema (category/severity).
function normalizeContentRule(raw: unknown): ContentFilterRule {
  const r = (raw || {}) as Record<string, unknown>;
  const legacySeverity = typeof r.severity === 'string' ? r.severity : '';
  const action: ContentFilterRule['action'] =
    r.action === 'block' || r.action === 'warn' || r.action === 'log'
      ? r.action
      : legacySeverity === 'critical' || legacySeverity === 'high'
        ? 'block'
        : legacySeverity === 'medium'
          ? 'warn'
          : legacySeverity === 'low'
            ? 'log'
            : 'block';
  return {
    name: typeof r.name === 'string' ? r.name : typeof r.category === 'string' ? r.category : '',
    pattern: typeof r.pattern === 'string' ? r.pattern : '',
    match_type: r.match_type === 'regex' ? 'regex' : 'contains',
    action,
  };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function getSettingValue(
  settings: Record<string, SettingEntry[]>,
  category: string,
  shortKey: string,
): unknown {
  const entries = settings[category];
  if (!entries) return undefined;
  const fullKey = `${category}.${shortKey}`;
  const entry = entries.find((e) => e.key === fullKey);
  return entry?.value;
}

function num(v: unknown, fallback = 0): number {
  if (v === undefined || v === null || v === '') return fallback;
  const n = Number(v);
  return Number.isNaN(n) ? fallback : n;
}

function str(v: unknown, fallback = ''): string {
  if (v === undefined || v === null) return fallback;
  return String(v);
}

// ---------------------------------------------------------------------------
// NumberField — a small helper for number inputs with label + hint
// ---------------------------------------------------------------------------

function NumberField({
  label,
  value,
  onChange,
  min = 0,
  max,
  hint,
  readOnly,
}: {
  label: string;
  value: number;
  onChange: (v: number) => void;
  min?: number;
  max?: number;
  hint?: string;
  readOnly?: boolean;
}) {
  return (
    <div className="space-y-1">
      <Label className="text-sm">{label}</Label>
      <Input
        type="number"
        value={value}
        onChange={(e) => onChange(Number(e.target.value))}
        min={min}
        max={max}
        readOnly={readOnly}
        className={readOnly ? 'bg-muted' : ''}
      />
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function SettingsPage() {
  const { t } = useTranslation();

  // Read-only state from dedicated endpoints
  const [systemInfo, setSystemInfo] = useState<SystemInfo | null>(null);
  const [, setOidcConfig] = useState<OidcConfig | null>(null);
  const [auditConfig, setAuditConfig] = useState<AuditConfig | null>(null);

  // Editable settings from GET /api/admin/settings
  const [_allSettings, setAllSettings] = useState<Record<string, SettingEntry[]>>({});

  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [statusMsg, setStatusMsg] = useState<{ type: 'success' | 'error'; text: string } | null>(null);

  // --- Editable form state ---
  // General
  const [siteName, setSiteName] = useState('');
  const [publicProtocol, setPublicProtocol] = useState('');
  const [publicHost, setPublicHost] = useState('');
  const [publicPort, setPublicPort] = useState(0);
  // Auth
  const [accessTtl, setAccessTtl] = useState(3600);
  const [refreshTtl, setRefreshTtl] = useState(7);
  const [signatureDrift, setSignatureDrift] = useState(300);
  const [nonceTtl, setNonceTtl] = useState(300);
  const [allowRegistration, setAllowRegistration] = useState(false);
  // Gateway
  const [cacheTtl, setCacheTtl] = useState(0);
  const [requestTimeout, setRequestTimeout] = useState(30);
  const [bodyLimit, setBodyLimit] = useState(1048576);
  // Security
  const [contentFilters, setContentFilters] = useState<ContentFilterRule[]>([]);
  const [piiPatterns, setPiiPatterns] = useState<PiiPattern[]>([]);

  // Sandbox + presets state
  const [cfSandboxOpen, setCfSandboxOpen] = useState(false);
  const [cfSandboxText, setCfSandboxText] = useState('');
  const [cfSandboxResult, setCfSandboxResult] = useState<ContentFilterTestMatch[] | null>(null);
  const [cfSandboxLoading, setCfSandboxLoading] = useState(false);
  const [cfPresetsOpen, setCfPresetsOpen] = useState(false);
  const [cfPresets, setCfPresets] = useState<ContentFilterPreset[]>([]);

  const [piiSandboxOpen, setPiiSandboxOpen] = useState(false);
  const [piiSandboxText, setPiiSandboxText] = useState('');
  const [piiSandboxResult, setPiiSandboxResult] = useState<PiiTestResponse | null>(null);
  const [piiSandboxLoading, setPiiSandboxLoading] = useState(false);
  const [clientIpSource, setClientIpSource] = useState('xff');
  const [clientIpXffPosition, setClientIpXffPosition] = useState('left');
  const [clientIpXffDepth, setClientIpXffDepth] = useState(1);
  // Budget
  const [alertThresholds, setAlertThresholds] = useState('');
  const [webhookUrl, setWebhookUrl] = useState('');
  // API Keys config
  const [defaultExpiry, setDefaultExpiry] = useState(90);
  const [inactivityTimeout, setInactivityTimeout] = useState(0);
  const [rotationPeriod, setRotationPeriod] = useState(0);
  const [gracePeriod, setGracePeriod] = useState(24);
  // Data retention
  const [usageRetention, setUsageRetention] = useState(90);
  const [auditRetention, setAuditRetention] = useState(365);
  // OIDC
  const [oidcEnabled, setOidcEnabled] = useState(false);
  const [oidcIssuerUrl, setOidcIssuerUrl] = useState('');
  const [oidcClientId, setOidcClientId] = useState('');
  const [oidcClientSecret, setOidcClientSecret] = useState('');
  const [oidcRedirectUrl, setOidcRedirectUrl] = useState('');
  const [oidcHasSecret, setOidcHasSecret] = useState(false);

  // ---------------------------------------------------------------------------
  // Load
  // ---------------------------------------------------------------------------

  const populateForm = useCallback((data: Record<string, SettingEntry[]>) => {
    setSiteName(str(getSettingValue(data, 'setup', 'site_name'), ''));
    setPublicProtocol(str(getSettingValue(data, 'general', 'public_protocol'), ''));
    setPublicHost(str(getSettingValue(data, 'general', 'public_host'), ''));
    setPublicPort(num(getSettingValue(data, 'general', 'public_port'), 0));
    setAccessTtl(num(getSettingValue(data, 'auth', 'jwt_access_ttl_secs'), 900));
    setRefreshTtl(num(getSettingValue(data, 'auth', 'jwt_refresh_ttl_days'), 7));
    setSignatureDrift(num(getSettingValue(data, 'security', 'signature_drift_secs'), 300));
    setNonceTtl(num(getSettingValue(data, 'security', 'signature_nonce_ttl_secs'), 600));
    setAllowRegistration(getSettingValue(data, 'auth', 'allow_registration') === true);
    setCacheTtl(num(getSettingValue(data, 'gateway', 'cache_ttl_secs'), 3600));
    setRequestTimeout(num(getSettingValue(data, 'gateway', 'request_timeout_secs'), 120));
    setBodyLimit(num(getSettingValue(data, 'gateway', 'body_limit_bytes'), 10485760));

    const cf = getSettingValue(data, 'security', 'content_filter_patterns');
    setContentFilters(Array.isArray(cf) ? cf.map(normalizeContentRule) : []);
    const pp = getSettingValue(data, 'security', 'pii_redactor_patterns');
    setPiiPatterns(Array.isArray(pp) ? pp : []);
    setClientIpSource(str(getSettingValue(data, 'security', 'client_ip_source'), 'xff'));
    setClientIpXffPosition(str(getSettingValue(data, 'security', 'client_ip_xff_position'), 'left'));
    setClientIpXffDepth(num(getSettingValue(data, 'security', 'client_ip_xff_depth'), 1));

    const th = getSettingValue(data, 'budget', 'alert_thresholds');
    setAlertThresholds(Array.isArray(th) ? th.join(', ') : str(th, ''));
    setWebhookUrl(str(getSettingValue(data, 'budget', 'webhook_url'), ''));

    setDefaultExpiry(num(getSettingValue(data, 'api_keys', 'default_expiry_days'), 90));
    setInactivityTimeout(num(getSettingValue(data, 'api_keys', 'inactivity_timeout_days'), 0));
    setRotationPeriod(num(getSettingValue(data, 'api_keys', 'rotation_period_days'), 0));
    setGracePeriod(num(getSettingValue(data, 'api_keys', 'rotation_grace_period_hours'), 24));

    setUsageRetention(num(getSettingValue(data, 'data', 'retention_days_usage'), 90));
    setAuditRetention(num(getSettingValue(data, 'data', 'retention_days_audit'), 365));
  }, []);

  useEffect(() => {
    Promise.all([
      api<SystemInfo>('/api/admin/settings/system').catch(() => null),
      api<OidcConfig>('/api/admin/settings/oidc').catch(() => null),
      api<AuditConfig>('/api/admin/settings/audit').catch(() => null),
      api<Record<string, SettingEntry[]>>('/api/admin/settings').catch(() => ({})),
    ])
      .then(([sys, oidc, audit, settings]) => {
        setSystemInfo(sys);
        setOidcConfig(oidc);
        if (oidc) {
          setOidcEnabled(oidc.enabled);
          setOidcIssuerUrl(oidc.issuer_url ?? '');
          setOidcClientId(oidc.client_id ?? '');
          setOidcRedirectUrl(oidc.redirect_url ?? '');
          setOidcHasSecret(oidc.has_secret ?? false);
        }
        setAuditConfig(audit);
        const s = settings ?? {};
        setAllSettings(s);
        populateForm(s);
      })
      .finally(() => setLoading(false));
  }, [populateForm]);

  // ---------------------------------------------------------------------------
  // Save
  // ---------------------------------------------------------------------------

  const handleSave = async () => {
    setSaving(true);
    setStatusMsg(null);
    try {
      const thresholdsParsed = alertThresholds
        .split(',')
        .map((s) => s.trim())
        .filter(Boolean)
        .map(Number)
        .filter((n) => !Number.isNaN(n));

      await apiPatch('/api/admin/settings', {
        settings: {
          'setup.site_name': siteName,
          'general.public_protocol': publicProtocol,
          'general.public_host': publicHost,
          'general.public_port': publicPort,
          'auth.jwt_access_ttl_secs': accessTtl,
          'auth.jwt_refresh_ttl_days': refreshTtl,
          'security.signature_drift_secs': signatureDrift,
          'security.signature_nonce_ttl_secs': nonceTtl,
          'auth.allow_registration': allowRegistration,
          'gateway.cache_ttl_secs': cacheTtl,
          'gateway.request_timeout_secs': requestTimeout,
          'gateway.body_limit_bytes': bodyLimit,
          'security.content_filter_patterns': contentFilters,
          'security.pii_redactor_patterns': piiPatterns,
          'security.client_ip_source': clientIpSource,
          'security.client_ip_xff_position': clientIpXffPosition,
          'security.client_ip_xff_depth': clientIpXffDepth,
          'budget.alert_thresholds': thresholdsParsed,
          'budget.webhook_url': webhookUrl,
          'api_keys.default_expiry_days': defaultExpiry,
          'api_keys.inactivity_timeout_days': inactivityTimeout,
          'api_keys.rotation_period_days': rotationPeriod,
          'api_keys.rotation_grace_period_hours': gracePeriod,
          'data.retention_days_usage': usageRetention,
          'data.retention_days_audit': auditRetention,
        },
      });

      // OIDC settings use a dedicated endpoint because saving triggers
      // provider re-discovery and encrypted secret handling.
      await apiPatch('/api/admin/settings/oidc', {
        enabled: oidcEnabled,
        issuer_url: oidcIssuerUrl,
        client_id: oidcClientId,
        client_secret: oidcClientSecret || undefined,
        redirect_url: oidcRedirectUrl,
      });
      setOidcClientSecret('');
      const updatedOidc = await api<OidcConfig>('/api/admin/settings/oidc').catch(() => null);
      if (updatedOidc) {
        setOidcConfig(updatedOidc);
        setOidcHasSecret(updatedOidc.has_secret ?? false);
      }

      setStatusMsg({ type: 'success', text: t('settings.saved') });
    } catch (err) {
      setStatusMsg({
        type: 'error',
        text: `${t('settings.saveError')}: ${err instanceof Error ? err.message : 'Unknown error'}`,
      });
    } finally {
      setSaving(false);
    }
  };

  // ---------------------------------------------------------------------------
  // Content filter helpers
  // ---------------------------------------------------------------------------

  const addContentFilter = () =>
    setContentFilters([
      ...contentFilters,
      { name: '', pattern: '', match_type: 'contains', action: 'block' },
    ]);

  const removeContentFilter = (i: number) =>
    setContentFilters(contentFilters.filter((_, idx) => idx !== i));

  const updateContentFilter = (
    i: number,
    field: keyof ContentFilterRule,
    value: string,
  ) =>
    setContentFilters(
      contentFilters.map((p, idx) => (idx === i ? { ...p, [field]: value } : p)),
    );

  // Sandbox: test the current (unsaved) rules against a sample text
  const runContentFilterSandbox = async () => {
    setCfSandboxLoading(true);
    setCfSandboxResult(null);
    try {
      const res = await apiPost<{ matches: ContentFilterTestMatch[] }>(
        '/api/admin/settings/content-filter/test',
        { text: cfSandboxText, rules: contentFilters },
      );
      setCfSandboxResult(res.matches);
    } catch (err) {
      console.error('Sandbox test failed:', err);
      setCfSandboxResult([]);
    } finally {
      setCfSandboxLoading(false);
    }
  };

  // Load presets on demand and append selected group's rules
  const openCfPresets = async () => {
    setCfPresetsOpen(true);
    if (cfPresets.length === 0) {
      try {
        const presets = await api<ContentFilterPreset[]>(
          '/api/admin/settings/content-filter/presets',
        );
        setCfPresets(presets);
      } catch (err) {
        console.error('Failed to load presets:', err);
      }
    }
  };

  const applyPreset = (preset: ContentFilterPreset) => {
    // Append preset rules, deduplicating by (pattern + match_type + action)
    const existing = new Set(
      contentFilters.map((r) => `${r.pattern}|${r.match_type}|${r.action}`),
    );
    const additions = preset.rules.filter(
      (r) => !existing.has(`${r.pattern}|${r.match_type}|${r.action}`),
    );
    setContentFilters([...contentFilters, ...additions.map(normalizeContentRule)]);
    setCfPresetsOpen(false);
  };

  const runPiiSandbox = async () => {
    setPiiSandboxLoading(true);
    setPiiSandboxResult(null);
    try {
      const res = await apiPost<PiiTestResponse>(
        '/api/admin/settings/pii-redactor/test',
        { text: piiSandboxText, patterns: piiPatterns },
      );
      setPiiSandboxResult(res);
    } catch (err) {
      console.error('PII sandbox failed:', err);
      setPiiSandboxResult({ redacted_text: '', matches: [] });
    } finally {
      setPiiSandboxLoading(false);
    }
  };

  // PII helpers
  const addPiiPattern = () =>
    setPiiPatterns([...piiPatterns, { name: '', regex: '', placeholder_prefix: '' }]);

  const removePiiPattern = (i: number) =>
    setPiiPatterns(piiPatterns.filter((_, idx) => idx !== i));

  const updatePiiPattern = (i: number, field: keyof PiiPattern, value: string) =>
    setPiiPatterns(piiPatterns.map((p, idx) => (idx === i ? { ...p, [field]: value } : p)));

  // ---------------------------------------------------------------------------
  // Render
  // ---------------------------------------------------------------------------

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('settingsPage.title')}</h1>
          <p className="text-muted-foreground">{t('settingsPage.subtitle')}</p>
        </div>
        <Button onClick={handleSave} disabled={saving}>
          {saving ? t('common.loading') : t('common.save')}
        </Button>
      </div>

      {statusMsg && (
        <Alert variant={statusMsg.type === 'success' ? 'default' : 'destructive'}>
          {statusMsg.type === 'success'
            ? <CheckCircle className="h-4 w-4" />
            : <AlertCircle className="h-4 w-4" />}
          <AlertDescription>{statusMsg.text}</AlertDescription>
        </Alert>
      )}

      <Tabs defaultValue="general">
        <TabsList>
          <TabsTrigger value="general">
            <Settings className="h-4 w-4" />
            {t('settings.general')}
          </TabsTrigger>
          <TabsTrigger value="auth">
            <Shield className="h-4 w-4" />
            {t('settings.auth')}
          </TabsTrigger>
          <TabsTrigger value="gateway">
            <Lock className="h-4 w-4" />
            {t('settings.gateway')}
          </TabsTrigger>
          <TabsTrigger value="security">
            <Shield className="h-4 w-4" />
            {t('settings.security')}
          </TabsTrigger>
          <TabsTrigger value="budget">
            <DollarSign className="h-4 w-4" />
            {t('settings.budget')}
          </TabsTrigger>
          <TabsTrigger value="apikeys">
            <Key className="h-4 w-4" />
            {t('settings.apiKeysConfig')}
          </TabsTrigger>
          <TabsTrigger value="audit">
            <Database className="h-4 w-4" />
            {t('settings.auditConfig')}
          </TabsTrigger>
        </TabsList>

        {/* ---------------------------------------------------------------- */}
        {/* General Tab                                                       */}
        {/* ---------------------------------------------------------------- */}
        <TabsContent value="general">
          <div className="space-y-6">
            {/* System info — read-only */}
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settingsPage.serverInfo')}</CardTitle>
              </CardHeader>
              <CardContent>
                {loading ? (
                  <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
                ) : (
                  <div className="space-y-4">
                    <div className="grid gap-4 sm:grid-cols-3">
                      <div>
                        <Label className="text-xs text-muted-foreground">{t('settingsPage.version')}</Label>
                        <p className="text-sm font-medium">{systemInfo?.version ?? '—'}</p>
                      </div>
                      <div>
                        <Label className="text-xs text-muted-foreground">{t('settingsPage.uptime')}</Label>
                        <p className="text-sm font-medium">{systemInfo?.uptime ?? '—'}</p>
                      </div>
                      <div>
                        <Label className="text-xs text-muted-foreground">{t('settingsPage.rustVersion')}</Label>
                        <p className="text-sm font-medium">{systemInfo?.rust_version ?? '—'}</p>
                      </div>
                    </div>
                    <div className="grid gap-4 sm:grid-cols-3 mt-4">
                      <div>
                        <Label className="text-xs text-muted-foreground">{t('settingsPage.serverHost')}</Label>
                        <p className="text-sm font-medium font-mono">{systemInfo?.server_host ?? '—'}</p>
                      </div>
                      <div>
                        <Label className="text-xs text-muted-foreground">{t('settingsPage.gatewayPort')}</Label>
                        <p className="text-sm font-medium font-mono">{systemInfo?.gateway_port ?? '—'}</p>
                      </div>
                      <div>
                        <Label className="text-xs text-muted-foreground">{t('settingsPage.consolePort')}</Label>
                        <p className="text-sm font-medium font-mono">{systemInfo?.console_port ?? '—'}</p>
                      </div>
                    </div>
                  </div>
                )}
              </CardContent>
            </Card>

            {/* Site name — editable */}
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settings.siteName')}</CardTitle>
              </CardHeader>
              <CardContent>
                <div className="max-w-sm">
                  <Input
                    value={siteName}
                    onChange={(e) => setSiteName(e.target.value)}
                    placeholder="ThinkWatch"
                  />
                </div>
              </CardContent>
            </Card>

            {/* Public Gateway URL — editable */}
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settings.publicGatewayUrl')}</CardTitle>
                <p className="text-xs text-muted-foreground mt-1">
                  {t('settings.publicGatewayUrlHint')}
                </p>
              </CardHeader>
              <CardContent>
                <div className="grid gap-4 sm:grid-cols-3">
                  <div>
                    <Label className="text-sm">{t('settings.publicProtocol')}</Label>
                    <Select value={publicProtocol || 'auto'} onValueChange={(v) => setPublicProtocol(v === 'auto' ? '' : v)}>
                      <SelectTrigger className="mt-1">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="auto">{t('settings.auto')}</SelectItem>
                        <SelectItem value="http">http</SelectItem>
                        <SelectItem value="https">https</SelectItem>
                      </SelectContent>
                    </Select>
                  </div>
                  <div>
                    <Label className="text-sm">{t('settings.publicHost')}</Label>
                    <Input
                      className="mt-1"
                      value={publicHost}
                      onChange={(e) => setPublicHost(e.target.value)}
                      placeholder={t('settings.publicHostPlaceholder')}
                    />
                  </div>
                  <div>
                    <Label className="text-sm">{t('settings.publicPort')}</Label>
                    <Input
                      className="mt-1"
                      type="number"
                      min={0}
                      max={65535}
                      value={publicPort}
                      onChange={(e) => setPublicPort(Number(e.target.value) || 0)}
                      placeholder="0"
                    />
                    <p className="text-xs text-muted-foreground mt-1">{t('settings.publicPortHint')}</p>
                  </div>
                </div>
              </CardContent>
            </Card>

          </div>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* Auth Tab                                                          */}
        {/* ---------------------------------------------------------------- */}
        <TabsContent value="auth">
          <div className="space-y-6">
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settings.auth')}</CardTitle>
              </CardHeader>
              <CardContent>
                <div className="grid gap-6 sm:grid-cols-2 max-w-2xl">
                  <NumberField label={t('settings.accessTtl')} value={accessTtl} onChange={setAccessTtl} min={60} max={86400} />
                  <NumberField label={t('settings.refreshTtl')} value={refreshTtl} onChange={setRefreshTtl} min={1} max={365} />
                  <NumberField label={t('settings.signatureDrift')} value={signatureDrift} onChange={setSignatureDrift} min={0} max={3600} />
                  <NumberField label={t('settings.nonceTtl')} value={nonceTtl} onChange={setNonceTtl} min={0} max={3600} />
                </div>
                <Separator className="my-6" />
                <div className="flex items-center justify-between max-w-2xl">
                  <div>
                    <Label className="text-sm">{t('settings.allowRegistration')}</Label>
                    <p className="text-xs text-muted-foreground mt-0.5">{t('settings.allowRegistrationHint')}</p>
                  </div>
                  <Switch checked={allowRegistration} onCheckedChange={setAllowRegistration} />
                </div>
              </CardContent>
            </Card>

            {/* OIDC / SSO — editable (saved together with the global Save button) */}
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settingsPage.oidcTitle')}</CardTitle>
              </CardHeader>
              <CardContent>
                {loading ? (
                  <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
                ) : (
                  <div className="space-y-4 max-w-lg">
                    <div className="flex items-center justify-between">
                      <div>
                        <Label className="text-sm">Enable SSO</Label>
                        <p className="text-xs text-muted-foreground mt-0.5">Allow users to log in via OIDC provider</p>
                      </div>
                      <Switch checked={oidcEnabled} onCheckedChange={setOidcEnabled} />
                    </div>
                    <Separator />
                    <div className="space-y-1">
                      <Label className="text-sm">{t('settingsPage.issuerUrl')}</Label>
                      <Input
                        value={oidcIssuerUrl}
                        onChange={(e) => setOidcIssuerUrl(e.target.value)}
                        placeholder="https://auth.example.com"
                      />
                    </div>
                    <div className="space-y-1">
                      <Label className="text-sm">{t('settingsPage.clientId')}</Label>
                      <Input
                        value={oidcClientId}
                        onChange={(e) => setOidcClientId(e.target.value)}
                        placeholder="your-client-id"
                      />
                    </div>
                    <div className="space-y-1">
                      <Label className="text-sm">Client Secret</Label>
                      <Input
                        type="password"
                        value={oidcClientSecret}
                        onChange={(e) => setOidcClientSecret(e.target.value)}
                        placeholder={oidcHasSecret ? '••••••• (leave empty to keep current)' : 'Enter client secret'}
                      />
                      {oidcHasSecret && (
                        <p className="text-xs text-muted-foreground">Secret is configured. Leave empty to keep unchanged.</p>
                      )}
                    </div>
                    <div className="space-y-1">
                      <Label className="text-sm">Redirect URL</Label>
                      <Input
                        value={oidcRedirectUrl}
                        onChange={(e) => setOidcRedirectUrl(e.target.value)}
                        placeholder="https://thinkwatch.example.com/api/auth/sso/callback"
                      />
                      <p className="text-xs text-muted-foreground">Must match the callback URL registered with your OIDC provider</p>
                    </div>
                  </div>
                )}
              </CardContent>
            </Card>
          </div>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* Gateway Tab                                                       */}
        {/* ---------------------------------------------------------------- */}
        <TabsContent value="gateway">
          <Card>
            <CardHeader>
              <CardTitle className="text-base">{t('settings.gateway')}</CardTitle>
            </CardHeader>
            <CardContent>
              <div className="grid gap-6 sm:grid-cols-2 max-w-2xl">
                <NumberField
                  label={t('settings.cacheTtl')}
                  value={cacheTtl}
                  onChange={setCacheTtl}
                  min={0}
                  max={86400}
                  hint={t('settings.zeroDisabled')}
                />
                <NumberField
                  label={`${t('settingsPage.gatewayPort')} — Request Timeout (s)`}
                  value={requestTimeout}
                  onChange={setRequestTimeout}
                  min={1}
                  max={600}
                  readOnly
                  hint={t('settings.requiresRestart')}
                />
                <NumberField
                  label="Body Limit (bytes)"
                  value={bodyLimit}
                  onChange={setBodyLimit}
                  min={1024}
                  max={104857600}
                  readOnly
                  hint={t('settings.requiresRestart')}
                />
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* Security Tab                                                      */}
        {/* ---------------------------------------------------------------- */}
        <TabsContent value="security">
          <div className="space-y-6">
            {/* Client IP resolution */}
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settings.clientIpTitle')}</CardTitle>
              </CardHeader>
              <CardContent>
                <div className="grid gap-6 sm:grid-cols-2 max-w-2xl">
                  <div className="space-y-1">
                    <Label className="text-sm">{t('settings.clientIpSource')}</Label>
                    <Select value={clientIpSource} onValueChange={setClientIpSource}>
                      <SelectTrigger>
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="connection">{t('settings.ipSourceConnection')}</SelectItem>
                        <SelectItem value="xff">{t('settings.ipSourceXff')}</SelectItem>
                        <SelectItem value="x-real-ip">{t('settings.ipSourceXRealIp')}</SelectItem>
                      </SelectContent>
                    </Select>
                    <p className="text-xs text-muted-foreground">{t('settings.clientIpSourceHint')}</p>
                  </div>
                  {clientIpSource === 'xff' && (
                    <>
                      <div className="space-y-1">
                        <Label className="text-sm">{t('settings.xffPosition')}</Label>
                        <Select value={clientIpXffPosition} onValueChange={setClientIpXffPosition}>
                          <SelectTrigger>
                            <SelectValue />
                          </SelectTrigger>
                          <SelectContent>
                            <SelectItem value="left">{t('settings.xffPositionLeft')}</SelectItem>
                            <SelectItem value="right">{t('settings.xffPositionRight')}</SelectItem>
                          </SelectContent>
                        </Select>
                      </div>
                      <NumberField
                        label={t('settings.xffDepth')}
                        value={clientIpXffDepth}
                        onChange={setClientIpXffDepth}
                        min={1}
                        max={20}
                        hint={t('settings.xffDepthHint')}
                      />
                    </>
                  )}
                </div>
              </CardContent>
            </Card>

            {/* Content filter rules */}
            <Card>
              <CardHeader>
                <div className="flex items-start justify-between gap-4">
                  <div className="space-y-1">
                    <CardTitle className="text-base">{t('settings.contentFilter.title')}</CardTitle>
                    <p className="text-xs text-muted-foreground max-w-2xl">
                      {t('settings.contentFilter.intro')}
                    </p>
                  </div>
                  <div className="flex gap-2 shrink-0">
                    <Button variant="outline" size="sm" onClick={openCfPresets}>
                      <Sparkles className="h-4 w-4" />
                      {t('settings.contentFilter.loadPresets')}
                    </Button>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => {
                        setCfSandboxOpen(true);
                        setCfSandboxResult(null);
                      }}
                    >
                      <FlaskConical className="h-4 w-4" />
                      {t('settings.contentFilter.testSandbox')}
                    </Button>
                    <Button variant="outline" size="sm" onClick={addContentFilter}>
                      <Plus className="h-4 w-4" />
                      {t('settings.addRule')}
                    </Button>
                  </div>
                </div>
              </CardHeader>
              <CardContent>
                {contentFilters.length === 0 ? (
                  <p className="text-sm text-muted-foreground py-4 text-center">
                    {t('settings.contentFilter.empty')}
                  </p>
                ) : (
                  <Table>
                    <TableHeader>
                      <TableRow>
                        <TableHead className="w-[180px]">{t('settings.contentFilter.ruleName')}</TableHead>
                        <TableHead className="w-[130px]">{t('settings.contentFilter.matchType')}</TableHead>
                        <TableHead>{t('settings.contentFilter.pattern')}</TableHead>
                        <TableHead className="w-[110px]">{t('settings.contentFilter.action')}</TableHead>
                        <TableHead className="w-10" />
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {contentFilters.map((cf, i) => (
                        <TableRow key={i}>
                          <TableCell>
                            <Input
                              value={cf.name}
                              onChange={(e) => updateContentFilter(i, 'name', e.target.value)}
                              placeholder={t('settings.contentFilter.namePlaceholder')}
                              className="h-8"
                            />
                          </TableCell>
                          <TableCell>
                            <Select
                              value={cf.match_type}
                              onValueChange={(v) => v && updateContentFilter(i, 'match_type', v)}
                            >
                              <SelectTrigger className="h-8">
                                <SelectValue />
                              </SelectTrigger>
                              <SelectContent>
                                <SelectItem value="contains">{t('settings.contentFilter.contains')}</SelectItem>
                                <SelectItem value="regex">{t('settings.contentFilter.regex')}</SelectItem>
                              </SelectContent>
                            </Select>
                          </TableCell>
                          <TableCell>
                            <Input
                              value={cf.pattern}
                              onChange={(e) => updateContentFilter(i, 'pattern', e.target.value)}
                              placeholder={
                                cf.match_type === 'regex' ? '\\d{4}-\\d{4}' : 'jailbreak'
                              }
                              className="h-8 font-mono text-xs"
                            />
                          </TableCell>
                          <TableCell>
                            <Select
                              value={cf.action}
                              onValueChange={(v) => v && updateContentFilter(i, 'action', v)}
                            >
                              <SelectTrigger className="h-8">
                                <SelectValue />
                              </SelectTrigger>
                              <SelectContent>
                                <SelectItem value="block">
                                  <span className="text-destructive">{t('settings.contentFilter.actionBlock')}</span>
                                </SelectItem>
                                <SelectItem value="warn">
                                  <span className="text-amber-600 dark:text-amber-400">{t('settings.contentFilter.actionWarn')}</span>
                                </SelectItem>
                                <SelectItem value="log">
                                  <span className="text-muted-foreground">{t('settings.contentFilter.actionLog')}</span>
                                </SelectItem>
                              </SelectContent>
                            </Select>
                          </TableCell>
                          <TableCell>
                            <Button variant="ghost" size="icon-sm" onClick={() => removeContentFilter(i)}>
                              <Trash2 className="h-4 w-4" />
                            </Button>
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                )}
                <div className="mt-4 grid grid-cols-1 gap-1 text-xs text-muted-foreground sm:grid-cols-3">
                  <p><strong className="text-destructive">{t('settings.contentFilter.actionBlock')}:</strong> {t('settings.contentFilter.actionBlockHint')}</p>
                  <p><strong className="text-amber-600 dark:text-amber-400">{t('settings.contentFilter.actionWarn')}:</strong> {t('settings.contentFilter.actionWarnHint')}</p>
                  <p><strong>{t('settings.contentFilter.actionLog')}:</strong> {t('settings.contentFilter.actionLogHint')}</p>
                </div>
              </CardContent>
            </Card>

            {/* PII redactor patterns */}
            <Card>
              <CardHeader>
                <div className="flex items-start justify-between gap-4">
                  <div className="space-y-1">
                    <CardTitle className="text-base">{t('settings.pii.title')}</CardTitle>
                    <p className="text-xs text-muted-foreground max-w-2xl">
                      {t('settings.pii.intro')}
                    </p>
                  </div>
                  <div className="flex gap-2 shrink-0">
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => {
                        setPiiSandboxOpen(true);
                        setPiiSandboxResult(null);
                      }}
                    >
                      <FlaskConical className="h-4 w-4" />
                      {t('settings.pii.testSandbox')}
                    </Button>
                    <Button variant="outline" size="sm" onClick={addPiiPattern}>
                      <Plus className="h-4 w-4" />
                      {t('settings.pii.addPattern')}
                    </Button>
                  </div>
                </div>
              </CardHeader>
              <CardContent>
                {piiPatterns.length === 0 ? (
                  <p className="text-sm text-muted-foreground py-4 text-center">
                    {t('settings.pii.empty')}
                  </p>
                ) : (
                  <Table>
                    <TableHeader>
                      <TableRow>
                        <TableHead className="w-[180px]">{t('settings.pii.name')}</TableHead>
                        <TableHead>{t('settings.pii.regex')}</TableHead>
                        <TableHead className="w-[160px]">{t('settings.pii.placeholderLabel')}</TableHead>
                        <TableHead className="w-10" />
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {piiPatterns.map((pp, i) => (
                        <TableRow key={i}>
                          <TableCell>
                            <Input
                              value={pp.name}
                              onChange={(e) => updatePiiPattern(i, 'name', e.target.value)}
                              placeholder={t('settings.pii.namePlaceholder')}
                              className="h-8"
                            />
                          </TableCell>
                          <TableCell>
                            <Input
                              value={pp.regex}
                              onChange={(e) => updatePiiPattern(i, 'regex', e.target.value)}
                              placeholder="\\d{3}-\\d{2}-\\d{4}"
                              className="h-8 font-mono text-xs"
                            />
                          </TableCell>
                          <TableCell>
                            <Input
                              value={pp.placeholder_prefix}
                              onChange={(e) => updatePiiPattern(i, 'placeholder_prefix', e.target.value)}
                              placeholder="EMAIL"
                              className="h-8 font-mono text-xs"
                            />
                          </TableCell>
                          <TableCell>
                            <Button variant="ghost" size="icon-sm" onClick={() => removePiiPattern(i)}>
                              <Trash2 className="h-4 w-4" />
                            </Button>
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                )}
                <p className="mt-4 text-xs text-muted-foreground">
                  {t('settings.pii.behavior')}
                </p>
              </CardContent>
            </Card>
          </div>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* Budget Tab                                                        */}
        {/* ---------------------------------------------------------------- */}
        <TabsContent value="budget">
          <Card>
            <CardHeader>
              <CardTitle className="text-base">{t('settings.budget')}</CardTitle>
            </CardHeader>
            <CardContent>
              <div className="space-y-6 max-w-lg">
                <div className="space-y-1">
                  <Label className="text-sm">{t('settings.thresholds')}</Label>
                  <Input
                    value={alertThresholds}
                    onChange={(e) => setAlertThresholds(e.target.value)}
                    placeholder="50, 80, 100"
                  />
                  <p className="text-xs text-muted-foreground">Comma-separated numeric thresholds</p>
                </div>
                <div className="space-y-1">
                  <Label className="text-sm">{t('settings.webhookUrl')}</Label>
                  <Input
                    value={webhookUrl}
                    onChange={(e) => setWebhookUrl(e.target.value)}
                    placeholder="https://hooks.example.com/budget"
                  />
                </div>
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* API Keys Config Tab                                               */}
        {/* ---------------------------------------------------------------- */}
        <TabsContent value="apikeys">
          <Card>
            <CardHeader>
              <CardTitle className="text-base">{t('settings.apiKeysConfig')}</CardTitle>
            </CardHeader>
            <CardContent>
              <div className="grid gap-6 sm:grid-cols-2 max-w-2xl">
                <NumberField label={t('settings.defaultExpiry')} value={defaultExpiry} onChange={setDefaultExpiry} min={0} max={3650} hint={t('settings.zeroDisabled')} />
                <NumberField label={t('settings.inactivityTimeout')} value={inactivityTimeout} onChange={setInactivityTimeout} min={0} max={365} hint={t('settings.zeroDisabled')} />
                <NumberField label={t('settings.rotationPeriod')} value={rotationPeriod} onChange={setRotationPeriod} min={0} max={365} hint={t('settings.zeroDisabled')} />
                <NumberField label={t('settings.gracePeriod')} value={gracePeriod} onChange={setGracePeriod} min={0} max={720} />
              </div>
            </CardContent>
          </Card>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* Audit Tab                                                         */}
        {/* ---------------------------------------------------------------- */}
        <TabsContent value="audit">
          <div className="space-y-6">
            {/* ClickHouse status — read-only */}
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settingsPage.auditTitle')}</CardTitle>
              </CardHeader>
              <CardContent>
                {loading ? (
                  <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
                ) : (
                  <div className="space-y-4">
                    <div className="flex items-center justify-between">
                      <div>
                        <Label className="text-sm">{t('settingsPage.clickhouse')}</Label>
                        <p className="text-xs text-muted-foreground mt-0.5">
                          {auditConfig?.clickhouse_url || '—'}
                        </p>
                      </div>
                      <Badge variant={auditConfig?.connected ? 'default' : 'destructive'}>
                        {auditConfig?.connected ? t('settingsPage.connected') : t('dashboard.unreachable')}
                      </Badge>
                    </div>
                    <Separator />
                    <div className="flex items-center justify-between">
                      <div>
                        <Label className="text-sm">{t('settingsPage.logForwarding')}</Label>
                        <p className="text-xs text-muted-foreground mt-0.5">
                          {t('settingsPage.logForwardingHint')}
                        </p>
                      </div>
                      <a href="/logs/forwarders" className="text-sm text-primary hover:underline">
                        {t('settingsPage.manage')}
                      </a>
                    </div>
                  </div>
                )}
              </CardContent>
            </Card>

            {/* Retention */}
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settings.dataRetention')}</CardTitle>
              </CardHeader>
              <CardContent>
                <div className="grid gap-6 sm:grid-cols-2 max-w-2xl">
                  <NumberField label={t('settings.usageRetention')} value={usageRetention} onChange={setUsageRetention} min={1} max={3650} />
                  <NumberField label={t('settings.auditRetention')} value={auditRetention} onChange={setAuditRetention} min={1} max={3650} />
                </div>
              </CardContent>
            </Card>
          </div>
        </TabsContent>
      </Tabs>

      {/* Content filter test sandbox */}
      <Dialog open={cfSandboxOpen} onOpenChange={setCfSandboxOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('settings.contentFilter.sandboxTitle')}</DialogTitle>
            <DialogDescription>{t('settings.contentFilter.sandboxDesc')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <Textarea
              rows={5}
              value={cfSandboxText}
              onChange={(e) => setCfSandboxText(e.target.value)}
              placeholder={t('settings.contentFilter.sandboxPlaceholder')}
              className="font-mono text-sm"
            />
            {cfSandboxResult !== null && (
              <div className="border rounded-md p-3 max-h-64 overflow-y-auto">
                {cfSandboxResult.length === 0 ? (
                  <p className="text-sm text-muted-foreground text-center py-2">
                    {t('settings.contentFilter.sandboxNoMatches')}
                  </p>
                ) : (
                  <div className="space-y-2">
                    <p className="text-sm font-medium">
                      {t('settings.contentFilter.sandboxMatchCount', { count: cfSandboxResult.length })}
                    </p>
                    {cfSandboxResult.map((m, i) => (
                      <div key={i} className="text-xs border-l-2 pl-3 py-1" style={{
                        borderColor: m.action === 'block' ? 'hsl(var(--destructive))' : m.action === 'warn' ? 'rgb(217 119 6)' : 'hsl(var(--muted-foreground))',
                      }}>
                        <div className="flex items-center gap-2">
                          <span className="font-semibold">{m.name || m.pattern}</span>
                          <Badge variant="outline" className="text-[10px]">{m.match_type}</Badge>
                          <Badge
                            variant={m.action === 'block' ? 'destructive' : m.action === 'warn' ? 'default' : 'secondary'}
                            className="text-[10px]"
                          >
                            {m.action}
                          </Badge>
                        </div>
                        <p className="font-mono text-muted-foreground mt-1">{m.matched_snippet}</p>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            )}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setCfSandboxOpen(false)}>
              {t('common.cancel')}
            </Button>
            <Button onClick={runContentFilterSandbox} disabled={cfSandboxLoading || !cfSandboxText.trim()}>
              <FlaskConical className="h-4 w-4" />
              {cfSandboxLoading ? t('common.loading') : t('settings.contentFilter.runTest')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Content filter presets */}
      <Dialog open={cfPresetsOpen} onOpenChange={setCfPresetsOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('settings.contentFilter.presetsTitle')}</DialogTitle>
            <DialogDescription>{t('settings.contentFilter.presetsDesc')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            {cfPresets.length === 0 ? (
              <p className="text-sm text-muted-foreground text-center py-4">{t('common.loading')}</p>
            ) : (
              cfPresets.map((preset) => (
                <div
                  key={preset.id}
                  className="border rounded-md p-3 hover:bg-muted/50 cursor-pointer"
                  onClick={() => applyPreset(preset)}
                >
                  <div className="flex items-center justify-between mb-1">
                    <h4 className="text-sm font-semibold">{t(`settings.contentFilter.preset.${preset.id}.name`)}</h4>
                    <Badge variant="secondary">{preset.rules.length} rules</Badge>
                  </div>
                  <p className="text-xs text-muted-foreground mb-2">
                    {t(`settings.contentFilter.preset.${preset.id}.description`)}
                  </p>
                  <div className="flex flex-wrap gap-1">
                    {preset.rules.slice(0, 5).map((r, i) => (
                      <Badge key={i} variant="outline" className="text-[10px] font-mono">
                        {r.pattern}
                      </Badge>
                    ))}
                    {preset.rules.length > 5 && (
                      <Badge variant="outline" className="text-[10px]">
                        +{preset.rules.length - 5}
                      </Badge>
                    )}
                  </div>
                </div>
              ))
            )}
          </div>
        </DialogContent>
      </Dialog>

      {/* PII redactor test sandbox */}
      <Dialog open={piiSandboxOpen} onOpenChange={setPiiSandboxOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>{t('settings.pii.sandboxTitle')}</DialogTitle>
            <DialogDescription>{t('settings.pii.sandboxDesc')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <Textarea
              rows={5}
              value={piiSandboxText}
              onChange={(e) => setPiiSandboxText(e.target.value)}
              placeholder={t('settings.pii.sandboxPlaceholder')}
              className="font-mono text-sm"
            />
            {piiSandboxResult !== null && (
              <div className="border rounded-md p-3 max-h-64 overflow-y-auto space-y-3">
                <div>
                  <Label className="text-xs text-muted-foreground">{t('settings.pii.redactedOutput')}</Label>
                  <pre className="font-mono text-xs bg-muted p-2 rounded mt-1 whitespace-pre-wrap break-all">
                    {piiSandboxResult.redacted_text || t('settings.pii.sandboxNoMatches')}
                  </pre>
                </div>
                {piiSandboxResult.matches.length > 0 && (
                  <div>
                    <Label className="text-xs text-muted-foreground">
                      {t('settings.pii.sandboxMatchCount', { count: piiSandboxResult.matches.length })}
                    </Label>
                    <div className="space-y-1 mt-1">
                      {piiSandboxResult.matches.map((m, i) => (
                        <div key={i} className="text-xs flex items-center gap-2">
                          <Badge variant="outline" className="text-[10px]">{m.name}</Badge>
                          <span className="font-mono text-destructive">{m.original}</span>
                          <span className="text-muted-foreground">→</span>
                          <span className="font-mono text-muted-foreground">{m.placeholder}</span>
                        </div>
                      ))}
                    </div>
                  </div>
                )}
              </div>
            )}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setPiiSandboxOpen(false)}>
              {t('common.cancel')}
            </Button>
            <Button onClick={runPiiSandbox} disabled={piiSandboxLoading || !piiSandboxText.trim()}>
              <FlaskConical className="h-4 w-4" />
              {piiSandboxLoading ? t('common.loading') : t('settings.pii.runTest')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
