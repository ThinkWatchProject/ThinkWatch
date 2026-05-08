import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { DialogFooter } from '@/components/ui/dialog';
import { HeaderEditor } from '@/components/header-editor';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import { apiPatch, apiPost } from '@/lib/api';
import { sanitizePrefixInput } from '@/lib/prefix-utils';
import { AuthModeBadge } from './auth-mode-badge';
import { deriveAuthMode, type AuthMode } from './auth-mode-utils';
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
  /** Single-valued auth shape — `'anonymous'`, `'oauth'`, or `'static'`. */
  auth_shape: 'anonymous' | 'oauth' | 'static';
  static_token_help_url: string | null;
  auth_header_name: string;
  auth_value_template: string;
  credential_owner: 'per_user' | 'admin_shared';
  config_json?: { custom_headers?: Record<string, string>; cache_ttl_secs?: number };
}

interface ServerEditFormProps {
  server: McpServerForEdit;
  onSaved: () => void;
  onCancel: () => void;
}

type CredentialOwner = 'per_user' | 'admin_shared';

export function ServerEditForm({ server, onSaved, onCancel }: ServerEditFormProps) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<AuthMode>(() => deriveAuthMode(server));

  const [name, setName] = useState(server.name);
  const [displayLabel, setDisplayLabel] = useState(server.display_label ?? '');
  const [namespacePrefix, setNamespacePrefix] = useState(server.namespace_prefix ?? '');
  const [description, setDescription] = useState(server.description ?? '');
  const [endpointUrl, setEndpointUrl] = useState(server.endpoint_url);
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
    setMode(deriveAuthMode(server));
    setName(server.name);
    setDisplayLabel(server.display_label ?? '');
    setNamespacePrefix(server.namespace_prefix ?? '');
    setDescription(server.description ?? '');
    setEndpointUrl(server.endpoint_url);
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

  const buildHeaders = () =>
    customHeaders.length > 0
      ? Object.fromEntries(customHeaders.filter(([k]) => k.trim()))
      : {};

  const handleSave = async () => {
    setError('');
    setSaving(true);
    try {
      const headers = buildHeaders();
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
        ...(mode === 'oauth' ? oauthPayload(oauth, includeSecret) : {}),
        auth_shape: server.auth_shape,
        static_token_help_url: server.auth_shape === 'static'
          ? staticTokenHelpUrl || null
          : null,
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

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-2">
        <AuthModeBadge mode={mode} />
        <p className="text-xs text-muted-foreground">
          {t('mcpServers.edit.modeImmutableHint')}
        </p>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

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

      {mode === 'oauth' && (
        <>
          {/* Client_secret rotation foot-gun: the password input is
              always rendered empty, and on save we treat
              `length > 0` as "rotate" / `length === 0` as "keep". */}
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

      {mode === 'static' && (
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

      {mode === 'direct' && (
        <div className="space-y-2">
          <Label>{t('providers.customHeaders')}</Label>
          <p className="text-xs text-muted-foreground">{t('providers.customHeadersDesc')}</p>
          <HeaderEditor
            headers={customHeaders}
            onChange={setCustomHeaders}
            keyPlaceholder="X-Custom-Header"
            presets={[
              { label: t('mcpServers.presetUserId'), header: ['X-User-Id', '{{user_id}}'] },
              { label: t('mcpServers.presetUserEmail'), header: ['X-User-Email', '{{user_email}}'] },
            ]}
          />
        </div>
      )}

      {(mode === 'oauth' || mode === 'static') && (
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
          {credentialOwner === 'admin_shared' &&
            server.credential_owner !== 'admin_shared' && (
              <p className="rounded bg-amber-50 px-2 py-1 text-xs text-amber-800 dark:bg-amber-950/30 dark:text-amber-300">
                {t('mcpServers.credentialOwner.switchWarning')}
              </p>
            )}
        </div>
      )}

      {/* Shared-credential management — only meaningful once the server
          is *already* in admin_shared mode (after a save). */}
      {server.credential_owner === 'admin_shared' && (
        <SharedCredentialPanel
          serverId={server.id}
          authShape={server.auth_shape}
          authHeaderName={server.auth_header_name}
          authValueTemplate={server.auth_value_template}
        />
      )}

      {(mode === 'oauth' || mode === 'static') && (
        <div className="space-y-2">
          <AuthHeaderFieldset value={authHeader} onChange={setAuthHeader} />
        </div>
      )}

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

      <DialogFooter>
        <Button variant="outline" onClick={onCancel}>{t('common.cancel')}</Button>
        <Button onClick={handleSave} disabled={saving}>
          {saving ? t('common.loading') : t('common.save')}
        </Button>
      </DialogFooter>
    </div>
  );
}
