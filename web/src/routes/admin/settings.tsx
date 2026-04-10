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
import { Settings, Shield, Key, Database, Lock, AlertCircle, CheckCircle, MemoryStick, Search } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Switch } from '@/components/ui/switch';
import { api, apiPatch } from '@/lib/api';
// Types, value-coercion helpers, and the small NumberField input
// live in the `settings/` sibling directory. The page component
// itself is still big — owns all editable state and the save
// flow — but the data shapes and the helpers it consumes are now
// reusable and unit-testable in isolation.
import {
  type AuditConfig,
  getSettingValue,
  num,
  type OidcConfig,
  type SettingEntry,
  str,
  type SystemInfo,
} from './settings/types';
import { NumberField } from './settings/NumberField';

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function SettingsPage() {
  const { t } = useTranslation();

  // Read-only state from dedicated endpoints
  const [systemInfo, setSystemInfo] = useState<SystemInfo | null>(null);
  const [health, setHealth] = useState<{ postgres: boolean; redis: boolean; clickhouse: boolean } | null>(null);
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
  const [rateLimitFailClosed, setRateLimitFailClosed] = useState(false);
  const [clientIpSource, setClientIpSource] = useState('xff');
  const [clientIpXffPosition, setClientIpXffPosition] = useState('left');
  const [clientIpXffDepth, setClientIpXffDepth] = useState(1);
  // API Keys config
  const [defaultExpiry, setDefaultExpiry] = useState(90);
  const [inactivityTimeout, setInactivityTimeout] = useState(0);
  const [rotationPeriod, setRotationPeriod] = useState(0);
  const [gracePeriod, setGracePeriod] = useState(24);
  // Data retention
  const [usageRetention, setUsageRetention] = useState(90);
  const [auditRetention, setAuditRetention] = useState(90);
  const [gatewayRetention, setGatewayRetention] = useState(90);
  const [mcpRetention, setMcpRetention] = useState(90);
  const [platformRetention, setPlatformRetention] = useState(90);
  const [accessRetention, setAccessRetention] = useState(30);
  const [appRetention, setAppRetention] = useState(30);
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

    setClientIpSource(str(getSettingValue(data, 'security', 'client_ip_source'), 'xff'));
    setClientIpXffPosition(str(getSettingValue(data, 'security', 'client_ip_xff_position'), 'left'));
    setClientIpXffDepth(num(getSettingValue(data, 'security', 'client_ip_xff_depth'), 1));
    setRateLimitFailClosed(getSettingValue(data, 'security', 'rate_limit_fail_closed') === true);

    setDefaultExpiry(num(getSettingValue(data, 'api_keys', 'default_expiry_days'), 90));
    setInactivityTimeout(num(getSettingValue(data, 'api_keys', 'inactivity_timeout_days'), 0));
    setRotationPeriod(num(getSettingValue(data, 'api_keys', 'rotation_period_days'), 0));
    setGracePeriod(num(getSettingValue(data, 'api_keys', 'rotation_grace_period_hours'), 24));

    setUsageRetention(num(getSettingValue(data, 'data', 'retention_days_usage'), 90));
    setAuditRetention(num(getSettingValue(data, 'data', 'retention_days_audit'), 90));
    setGatewayRetention(num(getSettingValue(data, 'data', 'retention_days_gateway'), 90));
    setMcpRetention(num(getSettingValue(data, 'data', 'retention_days_mcp'), 90));
    setPlatformRetention(num(getSettingValue(data, 'data', 'retention_days_platform'), 90));
    setAccessRetention(num(getSettingValue(data, 'data', 'retention_days_access'), 30));
    setAppRetention(num(getSettingValue(data, 'data', 'retention_days_app'), 30));
  }, []);

  useEffect(() => {
    Promise.all([
      api<SystemInfo>('/api/admin/settings/system').catch(() => null),
      api<OidcConfig>('/api/admin/settings/oidc').catch(() => null),
      api<AuditConfig>('/api/admin/settings/audit').catch(() => null),
      api<Record<string, SettingEntry[]>>('/api/admin/settings').catch(() => ({})),
      api<{ postgres: boolean; redis: boolean; clickhouse: boolean }>('/api/health').catch(() => null),
    ])
      .then(([sys, oidc, audit, settings, hp]) => {
        setSystemInfo(sys);
        setHealth(hp);
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
          'security.client_ip_source': clientIpSource,
          'security.client_ip_xff_position': clientIpXffPosition,
          'security.client_ip_xff_depth': clientIpXffDepth,
          'security.rate_limit_fail_closed': rateLimitFailClosed,
          'api_keys.default_expiry_days': defaultExpiry,
          'api_keys.inactivity_timeout_days': inactivityTimeout,
          'api_keys.rotation_period_days': rotationPeriod,
          'api_keys.rotation_grace_period_hours': gracePeriod,
          'data.retention_days_usage': usageRetention,
          'data.retention_days_audit': auditRetention,
          'data.retention_days_gateway': gatewayRetention,
          'data.retention_days_mcp': mcpRetention,
          'data.retention_days_platform': platformRetention,
          'data.retention_days_access': accessRetention,
          'data.retention_days_app': appRetention,
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
                    <Separator className="my-4" />
                    <div>
                      <Label className="text-xs text-muted-foreground">{t('dashboard.systemStatus')}</Label>
                      <div className="mt-2 grid gap-2 sm:grid-cols-3">
                        {[
                          { name: 'PostgreSQL', key: 'postgres' as const, icon: Database },
                          { name: 'Redis', key: 'redis' as const, icon: MemoryStick },
                          { name: 'ClickHouse', key: 'clickhouse' as const, icon: Search },
                        ].map((svc) => {
                          const ok = health?.[svc.key] ?? false;
                          return (
                            <div key={svc.key} className="flex items-center justify-between rounded-md border px-3 py-2">
                              <div className="flex items-center gap-2">
                                <svc.icon className="h-4 w-4 text-muted-foreground" />
                                <span className="text-sm font-medium">{svc.name}</span>
                              </div>
                              <Badge variant={ok ? 'default' : 'destructive'}>
                                {ok ? t('common.healthy') : t('dashboard.unreachable')}
                              </Badge>
                            </div>
                          );
                        })}
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

            {/* Rate limiter failure mode */}
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settings.rateLimiter.title')}</CardTitle>
              </CardHeader>
              <CardContent>
                <div className="flex items-start justify-between max-w-2xl gap-4">
                  <div>
                    <Label className="text-sm">{t('settings.rateLimiter.failClosed')}</Label>
                    <p className="text-xs text-muted-foreground mt-0.5 max-w-xl">
                      {t('settings.rateLimiter.failClosedHint')}
                    </p>
                  </div>
                  <Switch
                    checked={rateLimitFailClosed}
                    onCheckedChange={setRateLimitFailClosed}
                  />
                </div>
              </CardContent>
            </Card>

          </div>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* Budget Tab                                                        */}
        {/* ---------------------------------------------------------------- */}
        {/* The legacy "Budget alerts" tab is gone. Budget caps are
            managed per-subject (user / API key / provider / team) on
            their respective edit pages, not in global settings. */}

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
                <CardTitle className="text-base">{t('settings.retention.title')}</CardTitle>
                <p className="text-xs text-muted-foreground mt-1">
                  {t('settings.retention.intro')}
                </p>
              </CardHeader>
              <CardContent className="space-y-6">
                <div>
                  <Label className="text-xs uppercase tracking-wide text-muted-foreground mb-3 block">
                    {t('settings.retention.postgresGroup')}
                  </Label>
                  <div className="grid gap-6 sm:grid-cols-2 max-w-2xl">
                    <NumberField
                      label={t('settings.retention.usage')}
                      hint={t('settings.retention.usageHint')}
                      value={usageRetention}
                      onChange={setUsageRetention}
                      min={1}
                      max={3650}
                    />
                  </div>
                </div>
                <Separator />
                <div>
                  <Label className="text-xs uppercase tracking-wide text-muted-foreground mb-3 block">
                    {t('settings.retention.clickhouseGroup')}
                  </Label>
                  <div className="grid gap-6 sm:grid-cols-2 lg:grid-cols-3 max-w-3xl">
                    <NumberField
                      label={t('settings.retention.audit')}
                      hint={t('settings.retention.auditHint')}
                      value={auditRetention}
                      onChange={setAuditRetention}
                      min={1}
                      max={3650}
                    />
                    <NumberField
                      label={t('settings.retention.gateway')}
                      hint={t('settings.retention.gatewayHint')}
                      value={gatewayRetention}
                      onChange={setGatewayRetention}
                      min={1}
                      max={3650}
                    />
                    <NumberField
                      label={t('settings.retention.mcp')}
                      hint={t('settings.retention.mcpHint')}
                      value={mcpRetention}
                      onChange={setMcpRetention}
                      min={1}
                      max={3650}
                    />
                    <NumberField
                      label={t('settings.retention.platform')}
                      hint={t('settings.retention.platformHint')}
                      value={platformRetention}
                      onChange={setPlatformRetention}
                      min={1}
                      max={3650}
                    />
                    <NumberField
                      label={t('settings.retention.access')}
                      hint={t('settings.retention.accessHint')}
                      value={accessRetention}
                      onChange={setAccessRetention}
                      min={1}
                      max={3650}
                    />
                    <NumberField
                      label={t('settings.retention.app')}
                      hint={t('settings.retention.appHint')}
                      value={appRetention}
                      onChange={setAppRetention}
                      min={1}
                      max={3650}
                    />
                  </div>
                </div>
              </CardContent>
            </Card>
          </div>
        </TabsContent>
      </Tabs>

    </div>
  );
}
