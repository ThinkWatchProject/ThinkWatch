import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { DialogFooter } from '@/components/ui/dialog';
import { HeaderEditor } from '@/components/header-editor';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiPatch, apiPost } from '@/lib/api';
import { sanitizePrefixInput } from '@/lib/prefix-utils';
import { AuthModeBadge } from './auth-mode-badge';
import { deriveAuthMode, type AuthMode } from './auth-mode-utils';
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
  description: string | null;
  endpoint_url: string;
  oauth_issuer: string | null;
  oauth_authorization_endpoint: string | null;
  oauth_token_endpoint: string | null;
  oauth_revocation_endpoint: string | null;
  oauth_userinfo_endpoint: string | null;
  oauth_client_id: string | null;
  oauth_scopes: string[];
  allow_static_token: boolean;
  static_token_help_url: string | null;
  config_json?: { custom_headers?: Record<string, string>; cache_ttl_secs?: number };
}

interface ServerEditFormProps {
  server: McpServerForEdit;
  onSaved: () => void;
  onCancel: () => void;
}

export function ServerEditForm({ server, onSaved, onCancel }: ServerEditFormProps) {
  const { t } = useTranslation();
  // Derived once per `server.id` — the form lets the operator edit
  // mode-specific fields, but switching auth mode itself isn't allowed
  // from this dialog (delete + recreate to change mode). The badge +
  // conditional rendering reflect what the *server currently is*.
  const [mode, setMode] = useState<AuthMode>(() => deriveAuthMode(server));

  const [name, setName] = useState(server.name);
  const [namespacePrefix, setNamespacePrefix] = useState(server.namespace_prefix ?? '');
  const [description, setDescription] = useState(server.description ?? '');
  const [endpointUrl, setEndpointUrl] = useState(server.endpoint_url);
  const [oauth, setOauth] = useState<OAuthFields>(() => oauthFromServer(server));
  const [allowStaticToken, setAllowStaticToken] = useState(server.allow_static_token);
  const [staticTokenHelpUrl, setStaticTokenHelpUrl] = useState(server.static_token_help_url ?? '');
  const [customHeaders, setCustomHeaders] = useState<[string, string][]>(
    Object.entries(server.config_json?.custom_headers ?? {}),
  );
  const [cacheTtl, setCacheTtl] = useState(
    server.config_json?.cache_ttl_secs != null ? String(server.config_json.cache_ttl_secs) : '',
  );

  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');

  // Reset state if a different server is edited without unmounting (e.g.
  // the parent reuses the dialog for a sequence of edits).
  useEffect(() => {
    setMode(deriveAuthMode(server));
    setName(server.name);
    setNamespacePrefix(server.namespace_prefix ?? '');
    setDescription(server.description ?? '');
    setEndpointUrl(server.endpoint_url);
    setOauth(oauthFromServer(server));
    setAllowStaticToken(server.allow_static_token);
    setStaticTokenHelpUrl(server.static_token_help_url ?? '');
    setCustomHeaders(Object.entries(server.config_json?.custom_headers ?? {}));
    setCacheTtl(
      server.config_json?.cache_ttl_secs != null ? String(server.config_json.cache_ttl_secs) : '',
    );
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
      // The /test endpoint already treats 401/403 as soft success
      // (`success=true`) for anonymous probes, so we don't need a
      // separate requires_auth check here.
      if (!test.success) {
        setError(t('mcpServers.testFailedBlocking', { msg: test.message }));
        return;
      }

      // Only include oauth_client_secret in PATCH when the user typed a
      // new one — empty string means "no change".
      const includeSecret = oauth.clientSecret.length > 0;
      await apiPatch(`/api/mcp/servers/${server.id}`, {
        name,
        namespace_prefix: namespacePrefix || undefined,
        description,
        endpoint_url: endpointUrl,
        ...(mode === 'oauth' ? oauthPayload(oauth, includeSecret) : {}),
        allow_static_token: allowStaticToken,
        static_token_help_url: allowStaticToken ? (staticTokenHelpUrl || null) : null,
        custom_headers: headers,
        cache_ttl_secs: cacheTtl ? Number(cacheTtl) : undefined,
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
          <OAuthFieldset
            values={oauth}
            onChange={setOauth}
            secretPlaceholder={t('mcpServers.oauth.secretKeepCurrent')}
            flat
          />
          <div className="flex items-center gap-2">
            <Checkbox
              id="edit-allow-static"
              checked={allowStaticToken}
              onCheckedChange={(v) => setAllowStaticToken(v === true)}
            />
            <Label htmlFor="edit-allow-static" className="cursor-pointer text-sm">
              {t('mcpServers.wizard.allowStaticFallback')}
            </Label>
          </div>
          {allowStaticToken && (
            <div className="space-y-2">
              <Label htmlFor="edit-static-help">{t('mcpServers.wizard.staticHelpUrl')}</Label>
              <Input
                id="edit-static-help"
                value={staticTokenHelpUrl}
                onChange={(e) => setStaticTokenHelpUrl(e.target.value)}
              />
            </div>
          )}
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

      {(mode === 'headers' || mode === 'public') && (
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
