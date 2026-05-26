import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { CheckCircle2, AlertTriangle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { apiDelete, apiGet, apiPost, apiPut } from '@/lib/api';
import { toast } from 'sonner';

interface SharedCredentialStatus {
  configured: boolean;
  credential_type: 'oauth_authcode' | 'static_token' | null;
  expires_at: string | null;
  upstream_subject: string | null;
  configured_by: string | null;
  updated_at: string | null;
}

interface Props {
  serverId: string;
  /** Server's single-valued auth shape — drives whether the OAuth
   *  authorize button or the PAT input is rendered. Mutually
   *  exclusive: a server is `'oauth'` or `'static'` (or
   *  `'anonymous'`, in which case this panel isn't mounted). */
  authShape: 'anonymous' | 'oauth' | 'static';
  authHeaderName: string;
  authValueTemplate: string;
}

/**
 * Admin-side shared-credential management. Renders the current status
 * (configured / not, expiry, upstream subject) and provides the two
 * paths to populate / rotate it: OAuth dance or paste a PAT/API key.
 *
 * Only mounted when the server is already in `admin_shared` mode —
 * the credential_owner toggle in the parent form is the gate.
 */
export function SharedCredentialPanel({
  serverId,
  authShape,
  authHeaderName,
  authValueTemplate,
}: Props) {
  const oauthCapable = authShape === 'oauth';
  const allowStaticToken = authShape === 'static';
  const { t } = useTranslation();
  const [status, setStatus] = useState<SharedCredentialStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [pasted, setPasted] = useState('');
  const [submitting, setSubmitting] = useState(false);

  const refresh = async () => {
    setLoading(true);
    try {
      const s = await apiGet<SharedCredentialStatus>(
        `/api/admin/mcp/servers/${serverId}/shared-credential`,
      );
      setStatus(s);
    } catch (err) {
      // Status endpoint is read-only; failure here just leaves the
      // panel in "loading" state — the user can save and retry.
      console.warn('shared-credential status fetch failed', err);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
  }, [serverId]);

  const startOAuth = async () => {
    setSubmitting(true);
    try {
      const res = await apiPost<{ authorize_url: string }>(
        `/api/admin/mcp/servers/${serverId}/shared-credential/authorize`,
        {},
      );
      window.location.href = res.authorize_url;
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
      setSubmitting(false);
    }
  };

  const submitPasteToken = async () => {
    if (!pasted.trim()) return;
    setSubmitting(true);
    try {
      await apiPut(
        `/api/admin/mcp/servers/${serverId}/shared-credential/static-token`,
        { token: pasted.trim() },
      );
      toast.success(t('mcpServers.sharedCred.tokenSaved'));
      setPasted('');
      await refresh();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setSubmitting(false);
    }
  };

  const revoke = async () => {
    if (!window.confirm(t('mcpServers.sharedCred.revokeConfirm'))) return;
    setSubmitting(true);
    try {
      await apiDelete(`/api/admin/mcp/servers/${serverId}/shared-credential`);
      toast.success(t('mcpServers.sharedCred.revoked'));
      await refresh();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setSubmitting(false);
    }
  };

  // The header preview helps admins sanity-check what they'll send
  // before pasting (mirrors the per-user dialog's preview).
  const previewSample = pasted.length > 0 ? `${pasted.slice(0, 6)}…` : '••••••••';
  const previewHeader = `${authHeaderName}: ${authValueTemplate.replaceAll(
    '{{token}}',
    previewSample,
  )}`;

  return (
    <div className="space-y-3 rounded-md border p-3">
      <div>
        <Label className="text-sm font-medium">
          {t('mcpServers.sharedCred.title')}
        </Label>
        <p className="text-xs text-muted-foreground">
          {t('mcpServers.sharedCred.hint')}
        </p>
      </div>

      {loading ? (
        <p className="text-xs text-muted-foreground">{t('common.loading')}</p>
      ) : status?.configured ? (
        <Alert className="border-emerald-500/30 bg-emerald-500/10 text-emerald-900 dark:text-emerald-200 [&_svg]:text-emerald-600 dark:[&_svg]:text-emerald-300">
          <CheckCircle2 className="h-4 w-4" />
          <AlertDescription className="space-y-1 text-xs">
            <div className="font-medium">
              {t('mcpServers.sharedCred.configuredAs', {
                type:
                  status.credential_type === 'oauth_authcode'
                    ? 'OAuth'
                    : 'PAT/API key',
              })}
            </div>
            {status.upstream_subject && (
              <div>
                {t('mcpServers.sharedCred.upstreamSubject')}:{' '}
                <code className="font-mono">{status.upstream_subject}</code>
              </div>
            )}
            {status.expires_at && (
              <div>
                {t('mcpServers.sharedCred.expiresAt')}:{' '}
                {new Date(status.expires_at).toLocaleString()}
              </div>
            )}
          </AlertDescription>
        </Alert>
      ) : (
        <Alert className="border-amber-500/30 bg-amber-500/10 text-amber-900 dark:text-amber-200 [&_svg]:text-amber-600 dark:[&_svg]:text-amber-300">
          <AlertTriangle className="h-4 w-4" />
          <AlertDescription className="text-xs">
            {t('mcpServers.sharedCred.notConfigured')}
          </AlertDescription>
        </Alert>
      )}

      <div className="grid gap-2 sm:grid-cols-2">
        {oauthCapable && (
          <Button
            variant="outline"
            size="sm"
            type="button"
            onClick={startOAuth}
            disabled={submitting}
          >
            {status?.configured && status.credential_type === 'oauth_authcode'
              ? t('mcpServers.sharedCred.reauthorizeOAuth')
              : t('mcpServers.sharedCred.authorizeOAuth')}
          </Button>
        )}
        {status?.configured && (
          <Button
            variant="outline"
            size="sm"
            type="button"
            onClick={revoke}
            disabled={submitting}
            className="text-destructive hover:bg-destructive/10"
          >
            {t('mcpServers.sharedCred.revoke')}
          </Button>
        )}
      </div>

      {allowStaticToken && (
        <div className="space-y-1.5 border-t pt-2">
          <Label htmlFor="shared-paste-token" className="text-xs">
            {t('mcpServers.sharedCred.pasteTokenLabel')}
          </Label>
          <div className="flex gap-2">
            <Input
              id="shared-paste-token"
              type="password"
              autoComplete="off"
              value={pasted}
              onChange={(e) => setPasted(e.target.value)}
              placeholder={t('mcpServers.sharedCred.pastePlaceholder')}
            />
            <Button
              type="button"
              size="sm"
              onClick={submitPasteToken}
              disabled={submitting || !pasted.trim()}
            >
              {t('mcpServers.sharedCred.savePasted')}
            </Button>
          </div>
          <div className="rounded border bg-muted/30 px-2 py-1 text-[11px] text-muted-foreground">
            {t('mcpServers.sharedCred.headerPreview')}:{' '}
            <code className="font-mono">{previewHeader}</code>
          </div>
        </div>
      )}
    </div>
  );
}
