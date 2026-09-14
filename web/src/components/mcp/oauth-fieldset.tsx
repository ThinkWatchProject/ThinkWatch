import { useId, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ChevronDown, Loader2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { apiPost } from '@/lib/api';
import { toast } from 'sonner';
import type { OAuthFields } from './oauth-fields';

interface OAuthFieldsetProps {
  values: OAuthFields;
  onChange: (next: OAuthFields) => void;
  secretPlaceholder?: string;
  /** When true, advanced endpoints + scopes live in a collapsed section. */
  collapsibleAdvanced?: boolean;
  /** Hide the bordered wrapper (wizard step renders its own surface). */
  flat?: boolean;
  /**
   * When true, the AS advertised public-client support
   * (`token_endpoint_auth_methods_supported: ["none"]`). Hide the
   * Client Secret field — admin only needs Client ID, PKCE
   * authenticates the request. Saves one cognitive step.
   */
  publicClient?: boolean;
}

export function OAuthFieldset({
  values,
  onChange,
  secretPlaceholder,
  collapsibleAdvanced = false,
  flat = false,
  publicClient = false,
}: OAuthFieldsetProps) {
  const { t } = useTranslation();
  const [discovering, setDiscovering] = useState(false);
  // useId() guarantees per-instance unique IDs so the same fieldset can
  // mount in multiple dialogs without colliding. htmlFor/id pairs make
  // inputs findable via <label> association — required by Playwright's
  // getByLabel and by screen readers.
  const idBase = useId();
  const ids = {
    issuer: `${idBase}-issuer`,
    clientId: `${idBase}-client-id`,
    clientSecret: `${idBase}-client-secret`,
    auth: `${idBase}-auth-endpoint`,
    token: `${idBase}-token-endpoint`,
    revocation: `${idBase}-revocation-endpoint`,
    userinfo: `${idBase}-userinfo-endpoint`,
    scopes: `${idBase}-scopes`,
  };

  const handleDiscover = async () => {
    if (!values.issuer.trim()) {
      toast.error(t('mcpServers.oauth.setIssuerFirst'));
      return;
    }
    setDiscovering(true);
    try {
      const meta = await apiPost<{
        authorization_endpoint?: string;
        token_endpoint?: string;
        revocation_endpoint?: string;
        userinfo_endpoint?: string;
        scopes_supported?: string[];
      }>('/api/admin/mcp/oauth-discover', { issuer: values.issuer.trim() });
      onChange({
        ...values,
        authorizationEndpoint:
          meta.authorization_endpoint || values.authorizationEndpoint,
        tokenEndpoint: meta.token_endpoint || values.tokenEndpoint,
        revocationEndpoint:
          meta.revocation_endpoint || values.revocationEndpoint,
        userinfoEndpoint: meta.userinfo_endpoint || values.userinfoEndpoint,
        scopes:
          values.scopes ||
          (meta.scopes_supported ? meta.scopes_supported.join(' ') : ''),
      });
      toast.success(t('mcpServers.oauth.discoverSuccess'));
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('mcpServers.oauth.discoverFailed'));
    } finally {
      setDiscovering(false);
    }
  };

  const primary = (
    <>
      <div className="flex items-end justify-between gap-2">
        <div className="flex-1 space-y-1">
          <Label htmlFor={ids.issuer} className="text-xs">{t('mcpServers.oauth.issuer')}</Label>
          <Input
            id={ids.issuer}
            value={values.issuer}
            onChange={(e) => onChange({ ...values, issuer: e.target.value })}
            placeholder="https://github.com"
          />
        </div>
        <Button
          type="button"
          size="sm"
          variant="outline"
          disabled={discovering || !values.issuer}
          onClick={handleDiscover}
        >
          {discovering ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
          {discovering ? t('mcpServers.oauth.discovering') : t('mcpServers.oauth.discoverFromIssuer')}
        </Button>
      </div>
      {publicClient ? (
        <div className="space-y-1">
          <Label htmlFor={ids.clientId} className="text-xs">{t('mcpServers.oauth.clientId')}</Label>
          <Input
            id={ids.clientId}
            value={values.clientId}
            onChange={(e) => onChange({ ...values, clientId: e.target.value })}
          />
          <p className="text-xs text-muted-foreground">
            {t('mcpServers.oauth.publicClientNote')}
          </p>
        </div>
      ) : (
        <div className="grid grid-cols-2 gap-2">
          <div className="space-y-1">
            <Label htmlFor={ids.clientId} className="text-xs">{t('mcpServers.oauth.clientId')}</Label>
            <Input
              id={ids.clientId}
              value={values.clientId}
              onChange={(e) => onChange({ ...values, clientId: e.target.value })}
            />
          </div>
          <div className="space-y-1">
            <Label htmlFor={ids.clientSecret} className="text-xs">{t('mcpServers.oauth.clientSecret')}</Label>
            <Input
              id={ids.clientSecret}
              type="password"
              value={values.clientSecret}
              onChange={(e) => onChange({ ...values, clientSecret: e.target.value })}
              placeholder={secretPlaceholder}
            />
          </div>
        </div>
      )}
    </>
  );

  const advanced = (
    <>
      <div className="grid grid-cols-2 gap-2">
        <div className="space-y-1">
          <Label htmlFor={ids.auth} className="text-xs">{t('mcpServers.oauth.authEndpoint')}</Label>
          <Input
            id={ids.auth}
            value={values.authorizationEndpoint}
            onChange={(e) => onChange({ ...values, authorizationEndpoint: e.target.value })}
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={ids.token} className="text-xs">{t('mcpServers.oauth.tokenEndpoint')}</Label>
          <Input
            id={ids.token}
            value={values.tokenEndpoint}
            onChange={(e) => onChange({ ...values, tokenEndpoint: e.target.value })}
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={ids.revocation} className="text-xs">{t('mcpServers.oauth.revocationEndpoint')}</Label>
          <Input
            id={ids.revocation}
            value={values.revocationEndpoint}
            onChange={(e) => onChange({ ...values, revocationEndpoint: e.target.value })}
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor={ids.scopes} className="text-xs">{t('mcpServers.oauth.scopes')}</Label>
          <Input
            id={ids.scopes}
            value={values.scopes}
            onChange={(e) => onChange({ ...values, scopes: e.target.value })}
            placeholder="repo read:user"
          />
        </div>
      </div>
      <div className="space-y-1">
        <Label htmlFor={ids.userinfo} className="text-xs">{t('mcpServers.oauth.userinfoEndpoint')}</Label>
        <Input
          id={ids.userinfo}
          value={values.userinfoEndpoint}
          onChange={(e) => onChange({ ...values, userinfoEndpoint: e.target.value })}
          placeholder="https://api.github.com/user"
        />
        <p className="text-xs text-muted-foreground">
          {t('mcpServers.oauth.userinfoHint')}
        </p>
      </div>
    </>
  );

  const body = (
    <>
      {primary}
      {collapsibleAdvanced ? (
        <Collapsible className="space-y-2">
          <CollapsibleTrigger className="group flex items-center gap-1 text-xs text-muted-foreground hover:text-foreground">
            <ChevronDown className="h-3 w-3 transition-transform group-data-[state=open]:rotate-180" />
            {t('mcpServers.oauth.advanced')}
          </CollapsibleTrigger>
          <CollapsibleContent className="space-y-2 pt-1">
            {advanced}
          </CollapsibleContent>
        </Collapsible>
      ) : (
        advanced
      )}
    </>
  );

  if (flat) {
    return <div className="space-y-2">{body}</div>;
  }

  return (
    <div className="space-y-2 rounded-md border p-3">
      <p className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
        {t('mcpServers.oauth.sectionLabel')}
      </p>
      {body}
    </div>
  );
}
