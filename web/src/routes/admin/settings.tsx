import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Label } from '@/components/ui/label';
import { Separator } from '@/components/ui/separator';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Settings, Shield, ScrollText } from 'lucide-react';
import { api } from '@/lib/api';

interface SystemInfo {
  version: string;
  uptime: string;
  go_version: string;
}

interface OidcConfig {
  issuer_url: string;
  client_id: string;
  enabled: boolean;
}

interface AuditConfig {
  quickwit_url: string;
  quickwit_index: string;
}

export function SettingsPage() {
  const { t } = useTranslation();
  const [systemInfo, setSystemInfo] = useState<SystemInfo | null>(null);
  const [oidcConfig, setOidcConfig] = useState<OidcConfig | null>(null);
  const [auditConfig, setAuditConfig] = useState<AuditConfig | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    Promise.all([
      api<SystemInfo>('/api/admin/settings/system').catch(() => null),
      api<OidcConfig>('/api/admin/settings/oidc').catch(() => null),
      api<AuditConfig>('/api/admin/settings/audit').catch(() => null),
    ])
      .then(([sys, oidc, audit]) => {
        setSystemInfo(sys);
        setOidcConfig(oidc);
        setAuditConfig(audit);
      })
      .finally(() => setLoading(false));
  }, []);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('settingsPage.title')}</h1>
        <p className="text-muted-foreground">{t('settingsPage.subtitle')}</p>
      </div>

      <Tabs defaultValue="general">
        <TabsList>
          <TabsTrigger value="general">
            <Settings className="h-4 w-4" />
            {t('settingsPage.general')}
          </TabsTrigger>
          <TabsTrigger value="oidc">
            <Shield className="h-4 w-4" />
            {t('settingsPage.oidc')}
          </TabsTrigger>
          <TabsTrigger value="audit">
            <ScrollText className="h-4 w-4" />
            {t('settingsPage.auditConfig')}
          </TabsTrigger>
        </TabsList>

        <TabsContent value="general">
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
                      <Label className="text-xs text-muted-foreground">{t('settingsPage.goVersion')}</Label>
                      <p className="text-sm font-medium">{systemInfo?.go_version ?? '—'}</p>
                    </div>
                  </div>
                </div>
              )}
            </CardContent>
          </Card>
        </TabsContent>

        <TabsContent value="oidc">
          <Card>
            <CardHeader>
              <CardTitle className="text-base">{t('settingsPage.oidcTitle')}</CardTitle>
            </CardHeader>
            <CardContent>
              {loading ? (
                <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
              ) : (
                <div className="space-y-4">
                  <div className="flex items-center justify-between">
                    <Label className="text-sm">{t('common.status')}</Label>
                    <Badge variant={oidcConfig?.enabled ? 'default' : 'secondary'}>
                      {oidcConfig?.enabled ? t('common.enabled') : t('common.disabled')}
                    </Badge>
                  </div>
                  <Separator />
                  <div>
                    <Label className="text-xs text-muted-foreground">{t('settingsPage.issuerUrl')}</Label>
                    <p className="mt-1 font-mono text-sm">{oidcConfig?.issuer_url ?? '—'}</p>
                  </div>
                  <div>
                    <Label className="text-xs text-muted-foreground">{t('settingsPage.clientId')}</Label>
                    <p className="mt-1 font-mono text-sm">
                      {oidcConfig?.client_id
                        ? `${oidcConfig.client_id.slice(0, 8)}${'*'.repeat(Math.max(0, oidcConfig.client_id.length - 8))}`
                        : '—'}
                    </p>
                  </div>
                </div>
              )}
            </CardContent>
          </Card>
        </TabsContent>

        <TabsContent value="audit">
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
                      <Label className="text-sm">{t('settingsPage.quickwit')}</Label>
                      <p className="text-xs text-muted-foreground mt-0.5">
                        {auditConfig?.quickwit_url || '—'}
                      </p>
                    </div>
                    <Badge variant={auditConfig?.quickwit_url ? 'default' : 'secondary'}>
                      {auditConfig?.quickwit_url ? t('settingsPage.connected') : t('settingsPage.disconnected')}
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
                    <a href="/admin/log-forwarders" className="text-sm text-primary hover:underline">
                      {t('settingsPage.manage')}
                    </a>
                  </div>
                </div>
              )}
            </CardContent>
          </Card>
        </TabsContent>
      </Tabs>
    </div>
  );
}
