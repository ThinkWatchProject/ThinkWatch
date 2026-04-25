import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useSearch } from '@tanstack/react-router';
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
import { Settings, Shield, Key, Database, Lock, AlertCircle, MemoryStick, Search } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Switch } from '@/components/ui/switch';
import { api, apiPatch } from '@/lib/api';
import { toast } from 'sonner';
// Types, value-coercion helpers, and the small NumberField input
// live in the `settings/` sibling directory.
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
import { useFieldAutosave } from './settings/useFieldAutosave';
import { SaveIndicator } from './settings/SaveIndicator';

// One PATCH per field, keyed by the backend's dotted setting id. The
// server treats the body as a merge so sending a one-key object is the
// minimum-footprint way to commit an inline edit.
async function patchOne(key: string, value: unknown): Promise<void> {
  await apiPatch('/api/admin/settings', { settings: { [key]: value } });
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

export function SettingsPage() {
  const { t } = useTranslation();

  // URL is the source of truth for the active tab — makes the page
  // deep-linkable and keeps the browser back/forward buttons honest.
  // `strict: false` so the route's typed search shape doesn't clash
  // with nested routes; the validator in router.tsx already narrows it.
  const search = useSearch({ strict: false }) as { tab?: string };
  const activeTab = search.tab ?? 'general';
  const navigate = useNavigate();
  const setTab = (tab: string) => {
    navigate({
      to: '/admin/settings',
      // Tab is re-validated in router.tsx so the cast is safe — anything
      // unknown falls through to undefined there.
      search: { tab: tab === 'general' ? undefined : (tab as 'auth') },
      replace: true,
    });
  };

  // Read-only state from dedicated endpoints
  const [systemInfo, setSystemInfo] = useState<SystemInfo | null>(null);
  const [health, setHealth] = useState<{ postgres: boolean; redis: boolean; clickhouse: boolean } | null>(null);
  const [, setOidcConfig] = useState<OidcConfig | null>(null);
  const [auditConfig, setAuditConfig] = useState<AuditConfig | null>(null);

  // Editable settings from GET /api/admin/settings
  const [_allSettings, setAllSettings] = useState<Record<string, SettingEntry[]>>({});

  const [loading, setLoading] = useState(true);
  // OIDC keeps explicit group save because secret handling is write-only
  // and issuer changes trigger provider re-discovery on the server.
  const [oidcSaving, setOidcSaving] = useState(false);

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
  const [defaultRole, setDefaultRole] = useState('');
  const [availableRoles, setAvailableRoles] = useState<{ id: string; name: string }[]>([]);
  // MCP Store
  const [registryUrl, setRegistryUrl] = useState('');
  const [mcpHealthInterval, setMcpHealthInterval] = useState(300);
  const [mcpSessionTtl, setMcpSessionTtl] = useState(3600);
  const [mcpCacheTtl, setMcpCacheTtl] = useState(0);
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
  const [auditRetention, setAuditRetention] = useState(90);
  const [gatewayRetention, setGatewayRetention] = useState(90);
  const [mcpRetention, setMcpRetention] = useState(90);
  const [accessRetention, setAccessRetention] = useState(30);
  const [appRetention, setAppRetention] = useState(30);
  // Performance tuning
  const [perfHttpClientSecs, setPerfHttpClientSecs] = useState(15);
  const [perfMcpPoolSecs, setPerfMcpPoolSecs] = useState(30);
  const [perfConsoleRequestSecs, setPerfConsoleRequestSecs] = useState(30);
  const [perfDashboardWsIoSecs, setPerfDashboardWsIoSecs] = useState(5);
  const [perfDashboardWsTickSecs, setPerfDashboardWsTickSecs] = useState(4);
  const [perfDashboardWsMaxPerUser, setPerfDashboardWsMaxPerUser] = useState(4);
  // OIDC
  const [oidcEnabled, setOidcEnabled] = useState(false);
  const [oidcIssuerUrl, setOidcIssuerUrl] = useState('');
  const [oidcClientId, setOidcClientId] = useState('');
  // Snapshot of the value the server initially returned (masked form
  // like `abcd...wxyz`). On save we only re-send client_id when the
  // input has actually changed — otherwise the masked sentinel would
  // get round-tripped back as the new client_id and silently corrupt
  // the OIDC config.
  const [oidcClientIdLoaded, setOidcClientIdLoaded] = useState('');
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
    setDefaultRole(str(getSettingValue(data, 'auth', 'default_role'), ''));
    setRegistryUrl(str(getSettingValue(data, 'mcp_store', 'registry_url'), ''));
    setMcpHealthInterval(num(getSettingValue(data, 'mcp', 'health_interval_secs'), 300));
    setMcpSessionTtl(num(getSettingValue(data, 'mcp', 'session_ttl_secs'), 3600));
    setMcpCacheTtl(num(getSettingValue(data, 'mcp', 'cache_ttl_secs'), 0));
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

    setAuditRetention(num(getSettingValue(data, 'data', 'retention_days_audit'), 90));
    setGatewayRetention(num(getSettingValue(data, 'data', 'retention_days_gateway'), 90));
    setMcpRetention(num(getSettingValue(data, 'data', 'retention_days_mcp'), 90));
    setAccessRetention(num(getSettingValue(data, 'data', 'retention_days_access'), 30));
    setAppRetention(num(getSettingValue(data, 'data', 'retention_days_app'), 30));

    setPerfHttpClientSecs(num(getSettingValue(data, 'perf', 'http_client_secs'), 15));
    setPerfMcpPoolSecs(num(getSettingValue(data, 'perf', 'mcp_pool_secs'), 30));
    setPerfConsoleRequestSecs(num(getSettingValue(data, 'perf', 'console_request_secs'), 30));
    setPerfDashboardWsIoSecs(num(getSettingValue(data, 'perf', 'dashboard_ws_io_secs'), 5));
    setPerfDashboardWsTickSecs(num(getSettingValue(data, 'perf', 'dashboard_ws_tick_secs'), 4));
    setPerfDashboardWsMaxPerUser(num(getSettingValue(data, 'perf', 'dashboard_ws_max_per_user'), 4));
  }, []);

  useEffect(() => {
    Promise.all([
      api<SystemInfo>('/api/admin/settings/system').catch(() => null),
      api<OidcConfig>('/api/admin/settings/oidc').catch(() => null),
      api<AuditConfig>('/api/admin/settings/audit').catch(() => null),
      api<Record<string, SettingEntry[]>>('/api/admin/settings').catch(() => ({})),
      api<{ postgres: boolean; redis: boolean; clickhouse: boolean }>('/api/health').catch(() => null),
      api<{ items: { id: string; name: string }[] }>('/api/admin/roles').catch(() => ({ items: [] })),
    ])
      .then(([sys, oidc, audit, settings, hp, rolesData]) => {
        if (rolesData) setAvailableRoles(rolesData.items);
        setSystemInfo(sys);
        setHealth(hp);
        setOidcConfig(oidc);
        if (oidc) {
          setOidcEnabled(oidc.enabled);
          setOidcIssuerUrl(oidc.issuer_url ?? '');
          setOidcClientId(oidc.client_id ?? '');
          setOidcClientIdLoaded(oidc.client_id ?? '');
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
  // Inline autosave — one hook per editable scalar. Text/number fields
  // debounce 600ms; switches and selects pass 0 so a toggle commits
  // immediately. The readOnly gateway fields (requestTimeout, bodyLimit)
  // need a restart to apply so they don't autosave.
  // ---------------------------------------------------------------------------
  const isLoaded = !loading;

  // General
  const siteNameSave = useFieldAutosave({ value: siteName, isLoaded, persist: (v) => patchOne('setup.site_name', v) });
  const publicProtocolSave = useFieldAutosave({ value: publicProtocol, isLoaded, persist: (v) => patchOne('general.public_protocol', v), debounceMs: 0 });
  const publicHostSave = useFieldAutosave({ value: publicHost, isLoaded, persist: (v) => patchOne('general.public_host', v) });
  const publicPortSave = useFieldAutosave({ value: publicPort, isLoaded, persist: (v) => patchOne('general.public_port', v) });
  // Auth
  const accessTtlSave = useFieldAutosave({ value: accessTtl, isLoaded, persist: (v) => patchOne('auth.jwt_access_ttl_secs', v) });
  const refreshTtlSave = useFieldAutosave({ value: refreshTtl, isLoaded, persist: (v) => patchOne('auth.jwt_refresh_ttl_days', v) });
  const allowRegistrationSave = useFieldAutosave({ value: allowRegistration, isLoaded, persist: (v) => patchOne('auth.allow_registration', v), debounceMs: 0 });
  const defaultRoleSave = useFieldAutosave({ value: defaultRole, isLoaded, persist: (v) => patchOne('auth.default_role', v), debounceMs: 0 });
  // Security
  const signatureDriftSave = useFieldAutosave({ value: signatureDrift, isLoaded, persist: (v) => patchOne('security.signature_drift_secs', v) });
  const nonceTtlSave = useFieldAutosave({ value: nonceTtl, isLoaded, persist: (v) => patchOne('security.signature_nonce_ttl_secs', v) });
  const clientIpSourceSave = useFieldAutosave({ value: clientIpSource, isLoaded, persist: (v) => patchOne('security.client_ip_source', v), debounceMs: 0 });
  const clientIpXffPositionSave = useFieldAutosave({ value: clientIpXffPosition, isLoaded, persist: (v) => patchOne('security.client_ip_xff_position', v), debounceMs: 0 });
  const clientIpXffDepthSave = useFieldAutosave({ value: clientIpXffDepth, isLoaded, persist: (v) => patchOne('security.client_ip_xff_depth', v) });
  const rateLimitFailClosedSave = useFieldAutosave({ value: rateLimitFailClosed, isLoaded, persist: (v) => patchOne('security.rate_limit_fail_closed', v), debounceMs: 0 });
  // MCP Store + runtime
  const registryUrlSave = useFieldAutosave({ value: registryUrl, isLoaded, persist: (v) => patchOne('mcp_store.registry_url', v) });
  const mcpHealthIntervalSave = useFieldAutosave({ value: mcpHealthInterval, isLoaded, persist: (v) => patchOne('mcp.health_interval_secs', v) });
  const mcpSessionTtlSave = useFieldAutosave({ value: mcpSessionTtl, isLoaded, persist: (v) => patchOne('mcp.session_ttl_secs', v) });
  const mcpCacheTtlSave = useFieldAutosave({ value: mcpCacheTtl, isLoaded, persist: (v) => patchOne('mcp.cache_ttl_secs', v) });
  // Gateway
  const cacheTtlSave = useFieldAutosave({ value: cacheTtl, isLoaded, persist: (v) => patchOne('gateway.cache_ttl_secs', v) });
  // API Keys
  const defaultExpirySave = useFieldAutosave({ value: defaultExpiry, isLoaded, persist: (v) => patchOne('api_keys.default_expiry_days', v) });
  const inactivityTimeoutSave = useFieldAutosave({ value: inactivityTimeout, isLoaded, persist: (v) => patchOne('api_keys.inactivity_timeout_days', v) });
  const rotationPeriodSave = useFieldAutosave({ value: rotationPeriod, isLoaded, persist: (v) => patchOne('api_keys.rotation_period_days', v) });
  const gracePeriodSave = useFieldAutosave({ value: gracePeriod, isLoaded, persist: (v) => patchOne('api_keys.rotation_grace_period_hours', v) });
  // Data retention
  const auditRetentionSave = useFieldAutosave({ value: auditRetention, isLoaded, persist: (v) => patchOne('data.retention_days_audit', v) });
  const gatewayRetentionSave = useFieldAutosave({ value: gatewayRetention, isLoaded, persist: (v) => patchOne('data.retention_days_gateway', v) });
  const mcpRetentionSave = useFieldAutosave({ value: mcpRetention, isLoaded, persist: (v) => patchOne('data.retention_days_mcp', v) });
  const accessRetentionSave = useFieldAutosave({ value: accessRetention, isLoaded, persist: (v) => patchOne('data.retention_days_access', v) });
  const appRetentionSave = useFieldAutosave({ value: appRetention, isLoaded, persist: (v) => patchOne('data.retention_days_app', v) });
  // Performance
  const perfHttpClientSave = useFieldAutosave({ value: perfHttpClientSecs, isLoaded, persist: (v) => patchOne('perf.http_client_secs', v) });
  const perfMcpPoolSave = useFieldAutosave({ value: perfMcpPoolSecs, isLoaded, persist: (v) => patchOne('perf.mcp_pool_secs', v) });
  const perfConsoleRequestSave = useFieldAutosave({ value: perfConsoleRequestSecs, isLoaded, persist: (v) => patchOne('perf.console_request_secs', v) });
  const perfDashboardWsIoSave = useFieldAutosave({ value: perfDashboardWsIoSecs, isLoaded, persist: (v) => patchOne('perf.dashboard_ws_io_secs', v) });
  const perfDashboardWsTickSave = useFieldAutosave({ value: perfDashboardWsTickSecs, isLoaded, persist: (v) => patchOne('perf.dashboard_ws_tick_secs', v) });
  const perfDashboardWsMaxPerUserSave = useFieldAutosave({ value: perfDashboardWsMaxPerUser, isLoaded, persist: (v) => patchOne('perf.dashboard_ws_max_per_user', v) });

  // ---------------------------------------------------------------------------
  // OIDC group save — dedicated endpoint because the server runs discovery
  // and write-only secret handling on submit. Can't safely autosave per
  // keystroke, so the OIDC card keeps its own button.
  // ---------------------------------------------------------------------------

  const handleOidcSave = async () => {
    setOidcSaving(true);
    try {
      // Only re-send `client_id` when the user actually edited the
      // input — otherwise the masked sentinel returned on read would
      // get round-tripped back and overwrite the real value.
      const clientIdChanged = oidcClientId !== oidcClientIdLoaded;
      await apiPatch('/api/admin/settings/oidc', {
        enabled: oidcEnabled,
        issuer_url: oidcIssuerUrl,
        ...(clientIdChanged ? { client_id: oidcClientId } : {}),
        client_secret: oidcClientSecret || undefined,
        redirect_url: oidcRedirectUrl,
      });
      setOidcClientSecret('');
      const updated = await api<OidcConfig>('/api/admin/settings/oidc').catch(() => null);
      if (updated) {
        setOidcConfig(updated);
        setOidcClientId(updated.client_id ?? '');
        setOidcClientIdLoaded(updated.client_id ?? '');
        setOidcHasSecret(updated.has_secret ?? false);
      }
      toast.success(t('settings.saved'));
    } catch (err) {
      toast.error(`${t('settings.saveError')}: ${err instanceof Error ? err.message : 'Unknown error'}`);
    } finally {
      setOidcSaving(false);
    }
  };

  // ---------------------------------------------------------------------------
  // Render
  // ---------------------------------------------------------------------------

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('settingsPage.title')}</h1>
        <p className="text-muted-foreground">
          {t('settingsPage.subtitle')} <span className="text-xs">· {t('settings.autosaveHint')}</span>
        </p>
      </div>

      <Tabs value={activeTab} onValueChange={setTab}>
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
          <TabsTrigger value="perf">
            <Settings className="h-4 w-4" />
            {t('settings.perf')}
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
                <CardTitle className="text-base flex items-center gap-2">
                  {t('settings.siteName')}
                  <SaveIndicator state={siteNameSave.state} error={siteNameSave.error} />
                </CardTitle>
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
                    <div className="flex items-center justify-between gap-2">
                      <Label className="text-sm">{t('settings.publicProtocol')}</Label>
                      <SaveIndicator state={publicProtocolSave.state} error={publicProtocolSave.error} />
                    </div>
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
                    <div className="flex items-center justify-between gap-2">
                      <Label className="text-sm">{t('settings.publicHost')}</Label>
                      <SaveIndicator state={publicHostSave.state} error={publicHostSave.error} />
                    </div>
                    <Input
                      className="mt-1"
                      value={publicHost}
                      onChange={(e) => setPublicHost(e.target.value)}
                      placeholder={t('settings.publicHostPlaceholder')}
                    />
                  </div>
                  <div>
                    <div className="flex items-center justify-between gap-2">
                      <Label className="text-sm">{t('settings.publicPort')}</Label>
                      <SaveIndicator state={publicPortSave.state} error={publicPortSave.error} />
                    </div>
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
                  <NumberField label={t('settings.accessTtl')} value={accessTtl} onChange={setAccessTtl} min={60} max={86400}
                    indicator={<SaveIndicator state={accessTtlSave.state} error={accessTtlSave.error} />} />
                  <NumberField label={t('settings.refreshTtl')} value={refreshTtl} onChange={setRefreshTtl} min={1} max={365}
                    indicator={<SaveIndicator state={refreshTtlSave.state} error={refreshTtlSave.error} />} />
                  <NumberField label={t('settings.signatureDrift')} value={signatureDrift} onChange={setSignatureDrift} min={0} max={3600}
                    indicator={<SaveIndicator state={signatureDriftSave.state} error={signatureDriftSave.error} />} />
                  <NumberField label={t('settings.nonceTtl')} value={nonceTtl} onChange={setNonceTtl} min={0} max={3600}
                    indicator={<SaveIndicator state={nonceTtlSave.state} error={nonceTtlSave.error} />} />
                </div>
                <Separator className="my-6" />
                <div className="flex items-center justify-between max-w-2xl">
                  <div>
                    <Label className="text-sm">{t('settings.allowRegistration')}</Label>
                    <p className="text-xs text-muted-foreground mt-0.5">{t('settings.allowRegistrationHint')}</p>
                  </div>
                  <div className="flex items-center gap-2">
                    <SaveIndicator state={allowRegistrationSave.state} error={allowRegistrationSave.error} />
                    <Switch checked={allowRegistration} onCheckedChange={setAllowRegistration} />
                  </div>
                </div>
                <Separator className="my-6" />
                <div className="space-y-2 max-w-2xl">
                  <div className="flex items-center justify-between gap-2">
                    <Label className="text-sm">{t('settings.defaultRole')}</Label>
                    <SaveIndicator state={defaultRoleSave.state} error={defaultRoleSave.error} />
                  </div>
                  <p className="text-xs text-muted-foreground">{t('settings.defaultRoleHint')}</p>
                  <Select value={defaultRole || '__none__'} onValueChange={(v) => setDefaultRole(v === '__none__' ? '' : v)}>
                    <SelectTrigger className="w-64">
                      <SelectValue placeholder={t('settings.noRole')} />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="__none__">{t('settings.noRole')}</SelectItem>
                      {availableRoles.map((r) => (
                        <SelectItem key={r.id} value={r.name}>{r.name}</SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
              </CardContent>
            </Card>

            {/* OIDC / SSO — explicit group save. Secret is write-only and
                issuer changes re-discover the provider on submit, so
                inline autosave per keystroke is not appropriate. */}
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
                      <Label className="text-sm">{t('settings.oidc.clientSecret')}</Label>
                      <Input
                        type="password"
                        value={oidcClientSecret}
                        onChange={(e) => setOidcClientSecret(e.target.value)}
                        placeholder={oidcHasSecret ? t('settings.oidc.secretKeepPlaceholder') : t('settings.oidc.secretEnterPlaceholder')}
                      />
                      {oidcHasSecret && (
                        <p className="text-xs text-muted-foreground">{t('settings.oidc.secretConfiguredHint')}</p>
                      )}
                    </div>
                    <div className="space-y-1">
                      <Label className="text-sm">{t('settings.oidc.redirectUrl')}</Label>
                      <Input
                        value={oidcRedirectUrl}
                        onChange={(e) => setOidcRedirectUrl(e.target.value)}
                        placeholder="https://thinkwatch.example.com/api/auth/sso/callback"
                      />
                      <p className="text-xs text-muted-foreground">{t('settings.oidc.redirectUrlHint')}</p>
                    </div>
                    <div className="flex justify-end pt-2">
                      <Button size="sm" onClick={handleOidcSave} disabled={oidcSaving}>
                        {oidcSaving ? t('common.saving') : t('settings.oidc.save')}
                      </Button>
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
        <TabsContent value="gateway" className="space-y-6">
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
                  indicator={<SaveIndicator state={cacheTtlSave.state} error={cacheTtlSave.error} />}
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

          <Card>
            <CardHeader>
              <CardTitle className="text-base">{t('settings.mcpStoreTitle')}</CardTitle>
            </CardHeader>
            <CardContent>
              <div className="space-y-2 max-w-2xl">
                <div className="flex items-center justify-between gap-2">
                  <Label className="text-sm">{t('settings.registryUrl')}</Label>
                  <SaveIndicator state={registryUrlSave.state} error={registryUrlSave.error} />
                </div>
                <p className="text-xs text-muted-foreground">{t('settings.registryUrlHint')}</p>
                <Input value={registryUrl} onChange={(e) => setRegistryUrl(e.target.value)} placeholder="https://thinkwatch.dev/registry/mcp-templates.json" />
              </div>
            </CardContent>
          </Card>

          {/* Background health-check cadence — read on every loop tick
              so changes here propagate to the next probe round without
              a server restart. */}
          <Card>
            <CardHeader>
              <CardTitle className="text-base">{t('settings.mcpRuntimeTitle')}</CardTitle>
            </CardHeader>
            <CardContent>
              <div className="space-y-2 max-w-md">
                <div className="flex items-center justify-between gap-2">
                  <Label className="text-sm">{t('settings.mcpHealthIntervalLabel')}</Label>
                  <SaveIndicator state={mcpHealthIntervalSave.state} error={mcpHealthIntervalSave.error} />
                </div>
                <p className="text-xs text-muted-foreground">{t('settings.mcpHealthIntervalHint')}</p>
                <Input
                  type="number"
                  min={5}
                  step={5}
                  value={mcpHealthInterval}
                  onChange={(e) => setMcpHealthInterval(Number(e.target.value) || 0)}
                />
              </div>
              <div className="space-y-2 max-w-md mt-4">
                <div className="flex items-center justify-between gap-2">
                  <Label className="text-sm">{t('settings.mcpSessionTtlLabel')}</Label>
                  <SaveIndicator state={mcpSessionTtlSave.state} error={mcpSessionTtlSave.error} />
                </div>
                <p className="text-xs text-muted-foreground">{t('settings.mcpSessionTtlHint')}</p>
                <Input
                  type="number"
                  min={60}
                  step={60}
                  value={mcpSessionTtl}
                  onChange={(e) => setMcpSessionTtl(Number(e.target.value) || 0)}
                />
              </div>
              <div className="space-y-2 max-w-md mt-4">
                <div className="flex items-center justify-between gap-2">
                  <Label className="text-sm">{t('settings.mcpCacheTtlLabel')}</Label>
                  <SaveIndicator state={mcpCacheTtlSave.state} error={mcpCacheTtlSave.error} />
                </div>
                <p className="text-xs text-muted-foreground">{t('settings.mcpCacheTtlHint')}</p>
                <Input
                  type="number"
                  min={0}
                  step={60}
                  value={mcpCacheTtl}
                  onChange={(e) => setMcpCacheTtl(Number(e.target.value) || 0)}
                />
              </div>
            </CardContent>
          </Card>

          <PlatformPricingCard />
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
                    <div className="flex items-center justify-between gap-2">
                      <Label className="text-sm">{t('settings.clientIpSource')}</Label>
                      <SaveIndicator state={clientIpSourceSave.state} error={clientIpSourceSave.error} />
                    </div>
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
                        <div className="flex items-center justify-between gap-2">
                          <Label className="text-sm">{t('settings.xffPosition')}</Label>
                          <SaveIndicator state={clientIpXffPositionSave.state} error={clientIpXffPositionSave.error} />
                        </div>
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
                        indicator={<SaveIndicator state={clientIpXffDepthSave.state} error={clientIpXffDepthSave.error} />}
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
                  <div className="flex items-center gap-2">
                    <SaveIndicator state={rateLimitFailClosedSave.state} error={rateLimitFailClosedSave.error} />
                    <Switch
                      checked={rateLimitFailClosed}
                      onCheckedChange={setRateLimitFailClosed}
                    />
                  </div>
                </div>
              </CardContent>
            </Card>

          </div>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* Budget Tab                                                        */}
        {/* ---------------------------------------------------------------- */}
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
                <NumberField label={t('settings.defaultExpiry')} value={defaultExpiry} onChange={setDefaultExpiry} min={0} max={3650} hint={t('settings.zeroDisabled')}
                  indicator={<SaveIndicator state={defaultExpirySave.state} error={defaultExpirySave.error} />} />
                <NumberField label={t('settings.inactivityTimeout')} value={inactivityTimeout} onChange={setInactivityTimeout} min={0} max={365} hint={t('settings.zeroDisabled')}
                  indicator={<SaveIndicator state={inactivityTimeoutSave.state} error={inactivityTimeoutSave.error} />} />
                <NumberField label={t('settings.rotationPeriod')} value={rotationPeriod} onChange={setRotationPeriod} min={0} max={365} hint={t('settings.zeroDisabled')}
                  indicator={<SaveIndicator state={rotationPeriodSave.state} error={rotationPeriodSave.error} />} />
                <NumberField label={t('settings.gracePeriod')} value={gracePeriod} onChange={setGracePeriod} min={0} max={720}
                  indicator={<SaveIndicator state={gracePeriodSave.state} error={gracePeriodSave.error} />} />
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
                      indicator={<SaveIndicator state={auditRetentionSave.state} error={auditRetentionSave.error} />}
                    />
                    <NumberField
                      label={t('settings.retention.gateway')}
                      hint={t('settings.retention.gatewayHint')}
                      value={gatewayRetention}
                      onChange={setGatewayRetention}
                      min={1}
                      max={3650}
                      indicator={<SaveIndicator state={gatewayRetentionSave.state} error={gatewayRetentionSave.error} />}
                    />
                    <NumberField
                      label={t('settings.retention.mcp')}
                      hint={t('settings.retention.mcpHint')}
                      value={mcpRetention}
                      onChange={setMcpRetention}
                      min={1}
                      max={3650}
                      indicator={<SaveIndicator state={mcpRetentionSave.state} error={mcpRetentionSave.error} />}
                    />
                    <NumberField
                      label={t('settings.retention.access')}
                      hint={t('settings.retention.accessHint')}
                      value={accessRetention}
                      onChange={setAccessRetention}
                      min={1}
                      max={3650}
                      indicator={<SaveIndicator state={accessRetentionSave.state} error={accessRetentionSave.error} />}
                    />
                    <NumberField
                      label={t('settings.retention.app')}
                      hint={t('settings.retention.appHint')}
                      value={appRetention}
                      onChange={setAppRetention}
                      min={1}
                      max={3650}
                      indicator={<SaveIndicator state={appRetentionSave.state} error={appRetentionSave.error} />}
                    />
                  </div>
                </div>
              </CardContent>
            </Card>
          </div>
        </TabsContent>

        {/* ---------------------------------------------------------------- */}
        {/* Performance Tuning Tab                                           */}
        {/* ---------------------------------------------------------------- */}
        <TabsContent value="perf">
          <div className="space-y-6">
            <Card>
              <CardHeader>
                <CardTitle className="text-base">{t('settings.perf')}</CardTitle>
                <p className="text-xs text-muted-foreground mt-1">
                  {t('settingsPage.perfHint')}
                </p>
              </CardHeader>
              <CardContent>
                <div className="grid gap-6 sm:grid-cols-2 max-w-2xl">
                  <NumberField label={t('settingsPage.perfHttpClient')} value={perfHttpClientSecs} onChange={setPerfHttpClientSecs} min={1} max={300} hint={t('settingsPage.perfHttpClientHint')}
                    indicator={<SaveIndicator state={perfHttpClientSave.state} error={perfHttpClientSave.error} />} />
                  <NumberField label={t('settingsPage.perfMcpPool')} value={perfMcpPoolSecs} onChange={setPerfMcpPoolSecs} min={1} max={300} hint={t('settingsPage.perfMcpPoolHint')}
                    indicator={<SaveIndicator state={perfMcpPoolSave.state} error={perfMcpPoolSave.error} />} />
                  <NumberField label={t('settingsPage.perfConsoleRequest')} value={perfConsoleRequestSecs} onChange={setPerfConsoleRequestSecs} min={1} max={300} hint={t('settingsPage.perfConsoleRequestHint')}
                    indicator={<SaveIndicator state={perfConsoleRequestSave.state} error={perfConsoleRequestSave.error} />} />
                  <NumberField label={t('settingsPage.perfWsIo')} value={perfDashboardWsIoSecs} onChange={setPerfDashboardWsIoSecs} min={1} max={60} hint={t('settingsPage.perfWsIoHint')}
                    indicator={<SaveIndicator state={perfDashboardWsIoSave.state} error={perfDashboardWsIoSave.error} />} />
                  <NumberField label={t('settingsPage.perfWsTick')} value={perfDashboardWsTickSecs} onChange={setPerfDashboardWsTickSecs} min={1} max={60} hint={t('settingsPage.perfWsTickHint')}
                    indicator={<SaveIndicator state={perfDashboardWsTickSave.state} error={perfDashboardWsTickSave.error} />} />
                  <NumberField label={t('settingsPage.perfWsMax')} value={perfDashboardWsMaxPerUser} onChange={setPerfDashboardWsMaxPerUser} min={1} max={50} hint={t('settingsPage.perfWsMaxHint')}
                    indicator={<SaveIndicator state={perfDashboardWsMaxPerUserSave.state} error={perfDashboardWsMaxPerUserSave.error} />} />
                </div>
              </CardContent>
            </Card>
          </div>
        </TabsContent>
      </Tabs>

    </div>
  );
}

/* ---------- Platform pricing card ---------- */

// The baseline `$/token` that Models use as `cost = baseline × weight × tokens`.
// Self-contained: own fetch, own save, own dirty-flag. Lives under the
// gateway tab because costs are an AI-Gateway concern.
function PlatformPricingCard() {
  const { t } = useTranslation();
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  // Edit in $/1M tokens because 0.0000025 is unreadable — multiply
  // by 1e6 on load and divide by 1e6 on save.
  const [inputPerM, setInputPerM] = useState('');
  const [outputPerM, setOutputPerM] = useState('');
  const [currency, setCurrency] = useState('USD');
  const [initial, setInitial] = useState({ input: '', output: '', currency: 'USD' });

  const reload = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const p = await api<{
        input_price_per_token: string;
        output_price_per_token: string;
        currency: string;
      }>('/api/admin/platform-pricing');
      const i = (Number(p.input_price_per_token) * 1_000_000).toString();
      const o = (Number(p.output_price_per_token) * 1_000_000).toString();
      setInputPerM(i);
      setOutputPerM(o);
      setCurrency(p.currency);
      setInitial({ input: i, output: o, currency: p.currency });
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load pricing');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  const dirty =
    inputPerM !== initial.input ||
    outputPerM !== initial.output ||
    currency !== initial.currency;

  const save = async () => {
    setSaving(true);
    setError('');
    try {
      const i = Number(inputPerM);
      const o = Number(outputPerM);
      if (!Number.isFinite(i) || !Number.isFinite(o) || i < 0 || o < 0) {
        setError(t('settingsPage.platformPricing.invalid'));
        setSaving(false);
        return;
      }
      await apiPatch('/api/admin/platform-pricing', {
        input_price_per_token: i / 1_000_000,
        output_price_per_token: o / 1_000_000,
        currency,
      });
      toast.success(t('settingsPage.platformPricing.saved'));
      await reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setSaving(false);
    }
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">
          {t('settingsPage.platformPricing.title')}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-4">
        <p className="text-sm text-muted-foreground">
          {t('settingsPage.platformPricing.hint')}
        </p>
        {error && (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}
        {loading ? (
          <p className="text-xs italic text-muted-foreground">{t('common.loading')}</p>
        ) : (
          <>
            <div className="grid gap-4 sm:grid-cols-3 max-w-2xl">
              <div className="space-y-1.5">
                <Label htmlFor="pp_input">
                  {t('settingsPage.platformPricing.inputPerM')}
                </Label>
                <Input
                  id="pp_input"
                  value={inputPerM}
                  onChange={(e) => setInputPerM(e.target.value)}
                  inputMode="decimal"
                  placeholder="2.0"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="pp_output">
                  {t('settingsPage.platformPricing.outputPerM')}
                </Label>
                <Input
                  id="pp_output"
                  value={outputPerM}
                  onChange={(e) => setOutputPerM(e.target.value)}
                  inputMode="decimal"
                  placeholder="8.0"
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="pp_currency">
                  {t('settingsPage.platformPricing.currency')}
                </Label>
                <Input
                  id="pp_currency"
                  value={currency}
                  onChange={(e) => setCurrency(e.target.value.toUpperCase())}
                  maxLength={3}
                />
              </div>
            </div>
            <div>
              <Button
                type="button"
                size="sm"
                disabled={!dirty || saving}
                onClick={save}
              >
                {saving ? t('common.saving') : t('common.save')}
              </Button>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}
