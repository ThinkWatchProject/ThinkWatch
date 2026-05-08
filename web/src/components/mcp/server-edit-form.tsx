import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { DialogFooter } from '@/components/ui/dialog';
import { HeaderEditor } from '@/components/header-editor';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { apiPatch, apiPost } from '@/lib/api';
import { sanitizePrefixInput } from '@/lib/prefix-utils';
import { cn } from '@/lib/utils';
import { AuthHeaderFieldset, type AuthHeaderFields } from './auth-header-fieldset';
import { SharedCredentialPanel } from './shared-credential-panel';
import {
  oauthFromServer,
  oauthPayload,
  OAuthFieldset,
  type OAuthFields,
} from './oauth-fieldset';

export interface McpServerForEdit {
  id: string;
  name: string;
  namespace_prefix: string;
  display_label: string | null;
  description: string | null;
  endpoint_url: string;
  oauth_issuer: string | null;
  oauth_authorization_endpoint: string | null;
  oauth_token_endpoint: string | null;
  oauth_revocation_endpoint: string | null;
  oauth_userinfo_endpoint: string | null;
  oauth_client_id: string | null;
  oauth_scopes: string[];
  auth_shape: AuthShape;
  static_token_help_url: string | null;
  auth_header_name: string;
  auth_value_template: string;
  credential_owner: CredentialOwner;
  config_json?: { custom_headers?: Record<string, string>; cache_ttl_secs?: number };
}

interface ServerEditFormProps {
  server: McpServerForEdit;
  onSaved: () => void;
  onCancel: () => void;
}

type AuthShape = 'anonymous' | 'oauth' | 'static';
type CredentialOwner = 'per_user' | 'admin_shared';
type TabKey = 'basic' | 'auth' | 'credential' | 'advanced';

/**
 * Edit form, 4-tab layout. Tabs map to the same axes the wizard
 * walks through:
 *
 *   Basic       — identity (name, prefix, display label, URL).
 *   Auth        — auth_shape + OAuth/static fields + header injection.
 *   Credential  — credential_owner + (when admin_shared persisted)
 *                 the shared-credential management panel.
 *                 Tab is hidden entirely when auth_shape='anonymous'
 *                 (no credential to own).
 *   Advanced    — custom headers + cache TTL (universal — apply to
 *                 all auth shapes, including anonymous).
 *
 * State lives at the parent level so switching tabs is free; save is
 * a single PATCH covering every tab. A small dot on each tab title
 * indicates that tab carries a pending change relative to the
 * persisted server, so the admin can see at a glance what they've
 * touched without scrolling.
 */
export function ServerEditForm({ server, onSaved, onCancel }: ServerEditFormProps) {
  const { t } = useTranslation();

  const [tab, setTab] = useState<TabKey>('basic');
  const [name, setName] = useState(server.name);
  const [displayLabel, setDisplayLabel] = useState(server.display_label ?? '');
  const [namespacePrefix, setNamespacePrefix] = useState(server.namespace_prefix ?? '');
  const [description, setDescription] = useState(server.description ?? '');
  const [endpointUrl, setEndpointUrl] = useState(server.endpoint_url);
  const [authShape, setAuthShape] = useState<AuthShape>(server.auth_shape);
  const [oauth, setOauth] = useState<OAuthFields>(() => oauthFromServer(server));
  const [staticTokenHelpUrl, setStaticTokenHelpUrl] = useState(server.static_token_help_url ?? '');
  const [customHeaders, setCustomHeaders] = useState<[string, string][]>(
    Object.entries(server.config_json?.custom_headers ?? {}),
  );
  const [cacheTtl, setCacheTtl] = useState(
    server.config_json?.cache_ttl_secs != null ? String(server.config_json.cache_ttl_secs) : '',
  );
  const [credentialOwner, setCredentialOwner] = useState<CredentialOwner>(server.credential_owner);
  const [authHeader, setAuthHeader] = useState<AuthHeaderFields>({
    headerName: server.auth_header_name,
    valueTemplate: server.auth_value_template,
  });

  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');

  // Reset state if a different server is edited without unmounting.
  useEffect(() => {
    setTab('basic');
    setName(server.name);
    setDisplayLabel(server.display_label ?? '');
    setNamespacePrefix(server.namespace_prefix ?? '');
    setDescription(server.description ?? '');
    setEndpointUrl(server.endpoint_url);
    setAuthShape(server.auth_shape);
    setOauth(oauthFromServer(server));
    setStaticTokenHelpUrl(server.static_token_help_url ?? '');
    setCustomHeaders(Object.entries(server.config_json?.custom_headers ?? {}));
    setCacheTtl(
      server.config_json?.cache_ttl_secs != null ? String(server.config_json.cache_ttl_secs) : '',
    );
    setCredentialOwner(server.credential_owner);
    setAuthHeader({
      headerName: server.auth_header_name,
      valueTemplate: server.auth_value_template,
    });
    setError('');
  }, [server]);

  // Per-tab "has pending change" indicators. We stay shallow: deep
  // diffs aren't worth it for a form this size, and any change to
  // auth_shape / credential_owner is significant enough to re-flag
  // the related tab. Custom headers compare via a stable JSON shape.
  const dirty = useMemo(() => {
    const baseHeaders: Record<string, string> = server.config_json?.custom_headers ?? {};
    const currentHeadersObj: Record<string, string> = Object.fromEntries(
      customHeaders.filter(([k]) => k.trim()),
    );
    const headersChanged =
      JSON.stringify(baseHeaders) !== JSON.stringify(currentHeadersObj);
    const cacheTtlChanged =
      (server.config_json?.cache_ttl_secs != null
        ? String(server.config_json.cache_ttl_secs)
        : '') !== cacheTtl;
    return {
      basic:
        name !== server.name ||
        displayLabel !== (server.display_label ?? '') ||
        namespacePrefix !== (server.namespace_prefix ?? '') ||
        description !== (server.description ?? '') ||
        endpointUrl !== server.endpoint_url,
      auth:
        authShape !== server.auth_shape ||
        oauthDirty(oauth, server) ||
        staticTokenHelpUrl !== (server.static_token_help_url ?? '') ||
        authHeader.headerName !== server.auth_header_name ||
        authHeader.valueTemplate !== server.auth_value_template,
      credential: credentialOwner !== server.credential_owner,
      advanced: headersChanged || cacheTtlChanged,
    };
  }, [
    server,
    name,
    displayLabel,
    namespacePrefix,
    description,
    endpointUrl,
    authShape,
    oauth,
    staticTokenHelpUrl,
    authHeader,
    credentialOwner,
    customHeaders,
    cacheTtl,
  ]);

  const handleSave = async () => {
    setError('');
    setSaving(true);
    try {
      const headers = customHeaders.length > 0
        ? Object.fromEntries(customHeaders.filter(([k]) => k.trim()))
        : {};
      const test = await apiPost<{
        success: boolean;
        requires_auth?: boolean;
        message: string;
      }>('/api/mcp/servers/test', {
        endpoint_url: endpointUrl,
        custom_headers: headers,
      });
      if (!test.success) {
        setError(t('mcpServers.testFailedBlocking', { msg: test.message }));
        return;
      }

      const includeSecret = oauth.clientSecret.length > 0;
      await apiPatch(`/api/mcp/servers/${server.id}`, {
        name,
        display_label: displayLabel.trim() === '' ? null : displayLabel.trim(),
        namespace_prefix: namespacePrefix || undefined,
        description,
        endpoint_url: endpointUrl,
        auth_shape: authShape,
        ...(authShape === 'oauth' ? oauthPayload(oauth, includeSecret) : {}),
        static_token_help_url:
          authShape === 'static' ? staticTokenHelpUrl || null : null,
        custom_headers: headers,
        cache_ttl_secs: cacheTtl ? Number(cacheTtl) : undefined,
        auth_header_name: authHeader.headerName,
        auth_value_template: authHeader.valueTemplate,
        credential_owner: credentialOwner,
      });
      onSaved();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update server');
    } finally {
      setSaving(false);
    }
  };

  const showCredTab = authShape !== 'anonymous';
  const authShapeChanged = authShape !== server.auth_shape;
  const switchingToAdminShared =
    credentialOwner === 'admin_shared' && server.credential_owner !== 'admin_shared';

  // Auto-switch off the credential tab if the user moves to anonymous
  // while sitting on it — otherwise the dialog body would render
  // empty content.
  useEffect(() => {
    if (!showCredTab && tab === 'credential') setTab('auth');
  }, [showCredTab, tab]);

  return (
    <div className="space-y-4">
      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Tabs value={tab} onValueChange={(v) => setTab(v as TabKey)}>
        <TabsList className="grid w-full grid-cols-4">
          <TabHead label={t('mcpServers.edit.tabs.basic')} dirty={dirty.basic} value="basic" />
          <TabHead label={t('mcpServers.edit.tabs.auth')} dirty={dirty.auth} value="auth" />
          <TabHead
            label={t('mcpServers.edit.tabs.credential')}
            dirty={dirty.credential}
            value="credential"
            disabled={!showCredTab}
          />
          <TabHead
            label={t('mcpServers.edit.tabs.advanced')}
            dirty={dirty.advanced}
            value="advanced"
          />
        </TabsList>

        <TabsContent value="basic" className="space-y-4 pt-4">
          <div className="space-y-2">
            <Label htmlFor="edit-mcp-name">{t('common.name')}</Label>
            <Input
              id="edit-mcp-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="edit-mcp-display-label">{t('mcpServers.displayLabel')}</Label>
            <Input
              id="edit-mcp-display-label"
              value={displayLabel}
              onChange={(e) => setDisplayLabel(e.target.value)}
              placeholder={t('mcpServers.displayLabelPlaceholder')}
              maxLength={120}
            />
            <p className="text-xs text-muted-foreground">{t('mcpServers.displayLabelHint')}</p>
          </div>
          <div className="space-y-2">
            <Label htmlFor="edit-mcp-prefix">{t('mcpServers.namespacePrefix')}</Label>
            <Input
              id="edit-mcp-prefix"
              value={namespacePrefix}
              onChange={(e) => setNamespacePrefix(sanitizePrefixInput(e.target.value))}
              pattern="[a-z0-9_]{1,32}"
              maxLength={32}
            />
            <p className="text-xs text-muted-foreground">{t('mcpServers.namespacePrefixHint')}</p>
          </div>
          <div className="space-y-2">
            <Label htmlFor="edit-mcp-desc">{t('common.description')}</Label>
            <Input
              id="edit-mcp-desc"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="edit-mcp-url">{t('mcpServers.endpointUrl')}</Label>
            <Input
              id="edit-mcp-url"
              value={endpointUrl}
              onChange={(e) => setEndpointUrl(e.target.value)}
            />
          </div>
        </TabsContent>

        <TabsContent value="auth" className="space-y-4 pt-4">
          <div className="space-y-2 rounded-md border p-3">
            <Label className="text-sm font-medium">
              {t('mcpServers.edit.authShapeTitle')}
            </Label>
            <RadioGroup
              value={authShape}
              onValueChange={(v) => setAuthShape(v as AuthShape)}
              className="space-y-1.5 pt-1"
            >
              <ShapeRadio
                id="anonymous"
                value="anonymous"
                label={t('mcpServers.wizard.shape.anonymousTitle')}
                hint={t('mcpServers.wizard.shape.anonymousHint')}
              />
              <ShapeRadio
                id="oauth"
                value="oauth"
                label={t('mcpServers.wizard.shape.oauthTitle')}
                hint={t('mcpServers.wizard.shape.oauthHint')}
              />
              <ShapeRadio
                id="static"
                value="static"
                label={t('mcpServers.wizard.shape.staticTitle')}
                hint={t('mcpServers.wizard.shape.staticHint')}
              />
            </RadioGroup>
            {authShapeChanged && (
              <Alert className="border-amber-500/30 bg-amber-500/10 text-amber-900 dark:text-amber-200 [&_svg]:text-amber-600 dark:[&_svg]:text-amber-300">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription className="text-xs">
                  {t('mcpServers.edit.authShapeChangeWarning')}
                </AlertDescription>
              </Alert>
            )}
          </div>

          {authShape === 'oauth' && (
            <>
              <Alert className="border-amber-500/30 bg-amber-500/10 text-amber-900 dark:text-amber-200 [&_svg]:text-amber-600 dark:[&_svg]:text-amber-300">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription className="text-xs">
                  {t('mcpServers.edit.clientSecretRotateHint')}
                </AlertDescription>
              </Alert>
              <OAuthFieldset
                values={oauth}
                onChange={setOauth}
                secretPlaceholder={t('mcpServers.oauth.secretKeepCurrent')}
                flat
              />
            </>
          )}

          {authShape === 'static' && (
            <div className="space-y-2">
              <Label htmlFor="edit-static-help">{t('mcpServers.wizard.staticHelpUrl')}</Label>
              <Input
                id="edit-static-help"
                value={staticTokenHelpUrl}
                onChange={(e) => setStaticTokenHelpUrl(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                {t('mcpServers.wizard.staticHelpUrlHint')}
              </p>
            </div>
          )}

          {authShape !== 'anonymous' && (
            <AuthHeaderFieldset value={authHeader} onChange={setAuthHeader} />
          )}
        </TabsContent>

        {showCredTab && (
          <TabsContent value="credential" className="space-y-4 pt-4">
            <div className="space-y-2 rounded-md border p-3">
              <Label className="text-sm font-medium">
                {t('mcpServers.credentialOwner.title')}
              </Label>
              <p className="text-xs text-muted-foreground">
                {t('mcpServers.credentialOwner.hint')}
              </p>
              <RadioGroup
                value={credentialOwner}
                onValueChange={(v) => setCredentialOwner(v as CredentialOwner)}
                className="space-y-1.5 pt-1"
              >
                <div className="flex items-start gap-2">
                  <RadioGroupItem id="edit-owner-per-user" value="per_user" className="mt-0.5" />
                  <Label htmlFor="edit-owner-per-user" className="cursor-pointer space-y-0.5">
                    <div className="text-sm font-medium">
                      {t('mcpServers.credentialOwner.perUser')}
                    </div>
                    <div className="text-xs font-normal text-muted-foreground">
                      {t('mcpServers.credentialOwner.perUserHint')}
                    </div>
                  </Label>
                </div>
                <div className="flex items-start gap-2">
                  <RadioGroupItem id="edit-owner-shared" value="admin_shared" className="mt-0.5" />
                  <Label htmlFor="edit-owner-shared" className="cursor-pointer space-y-0.5">
                    <div className="text-sm font-medium">
                      {t('mcpServers.credentialOwner.adminShared')}
                    </div>
                    <div className="text-xs font-normal text-muted-foreground">
                      {t('mcpServers.credentialOwner.adminSharedHint')}
                    </div>
                  </Label>
                </div>
              </RadioGroup>
              {switchingToAdminShared && (
                <p className="rounded bg-amber-50 px-2 py-1 text-xs text-amber-800 dark:bg-amber-950/30 dark:text-amber-300">
                  {t('mcpServers.credentialOwner.switchWarning')}
                </p>
              )}
            </div>

            {/* Shared-credential panel reads against the persisted
                shape — mid-edit transitions haven't been saved yet,
                so render only when the *server* (not the in-flight
                radio) is admin_shared. */}
            {server.credential_owner === 'admin_shared' &&
              server.auth_shape !== 'anonymous' && (
                <SharedCredentialPanel
                  serverId={server.id}
                  authShape={server.auth_shape}
                  authHeaderName={server.auth_header_name}
                  authValueTemplate={server.auth_value_template}
                />
              )}
          </TabsContent>
        )}

        <TabsContent value="advanced" className="space-y-4 pt-4">
          <div className="space-y-2 rounded-md border p-3">
            <Label className="text-sm font-medium">{t('providers.customHeaders')}</Label>
            <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
            <HeaderEditor
              headers={customHeaders}
              onChange={setCustomHeaders}
              keyPlaceholder="X-Custom-Header"
              presets={[
                { label: t('mcpServers.presetUserId'), header: ['X-User-Id', '{{user_id}}'] },
                {
                  label: t('mcpServers.presetUserEmail'),
                  header: ['X-User-Email', '{{user_email}}'],
                },
              ]}
            />
          </div>

          <div className="space-y-2">
            <Label>{t('mcpServers.cacheTtlLabel')}</Label>
            <p className="text-xs text-muted-foreground">{t('mcpServers.cacheTtlHint')}</p>
            <Input
              type="number"
              min={0}
              step={60}
              placeholder={t('mcpServers.cacheTtlPlaceholder')}
              value={cacheTtl}
              onChange={(e) => setCacheTtl(e.target.value)}
            />
          </div>
        </TabsContent>
      </Tabs>

      <DialogFooter>
        <Button variant="outline" onClick={onCancel}>
          {t('common.cancel')}
        </Button>
        <Button onClick={handleSave} disabled={saving}>
          {saving ? t('common.loading') : t('common.save')}
        </Button>
      </DialogFooter>
    </div>
  );
}

/** Tab title with a small dot when the tab carries unsaved changes. */
function TabHead({
  label,
  dirty,
  value,
  disabled,
}: {
  label: string;
  dirty: boolean;
  value: TabKey;
  disabled?: boolean;
}) {
  return (
    <TabsTrigger value={value} disabled={disabled} className="relative">
      {label}
      {dirty && (
        <span
          className={cn(
            'ml-1.5 inline-block h-1.5 w-1.5 rounded-full',
            'bg-primary',
          )}
          aria-label="modified"
        />
      )}
    </TabsTrigger>
  );
}

function ShapeRadio({
  id,
  value,
  label,
  hint,
}: {
  id: string;
  value: AuthShape;
  label: string;
  hint: string;
}) {
  return (
    <div className="flex items-start gap-2">
      <RadioGroupItem id={`edit-shape-${id}`} value={value} className="mt-0.5" />
      <Label htmlFor={`edit-shape-${id}`} className="cursor-pointer space-y-0.5">
        <div className="text-sm font-medium">{label}</div>
        <div className="text-xs font-normal text-muted-foreground">{hint}</div>
      </Label>
    </div>
  );
}

/** Whether OAuth fields differ from the persisted server. Used to
 *  decide if the Auth tab should show a dirty dot. */
function oauthDirty(oauth: OAuthFields, server: McpServerForEdit): boolean {
  return (
    oauth.issuer !== (server.oauth_issuer ?? '') ||
    oauth.authorizationEndpoint !== (server.oauth_authorization_endpoint ?? '') ||
    oauth.tokenEndpoint !== (server.oauth_token_endpoint ?? '') ||
    oauth.revocationEndpoint !== (server.oauth_revocation_endpoint ?? '') ||
    oauth.userinfoEndpoint !== (server.oauth_userinfo_endpoint ?? '') ||
    oauth.clientId !== (server.oauth_client_id ?? '') ||
    oauth.scopes !== server.oauth_scopes.join(' ') ||
    // client_secret is "rotate when non-empty"; non-empty input means dirty.
    oauth.clientSecret.length > 0
  );
}
