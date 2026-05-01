import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Badge } from '@/components/ui/badge';
import { Alert, AlertDescription } from '@/components/ui/alert';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { api, apiPost, apiDelete } from '@/lib/api';
import { cn } from '@/lib/utils';
import {
  Plug,
  KeyRound,
  RefreshCw,
  Trash2,
  Plus,
  ExternalLink,
  CheckCircle2,
  Activity,
  Loader2,
  ChevronDown,
  ChevronRight,
} from 'lucide-react';
import { toast } from 'sonner';
import { format } from 'date-fns';

interface ConnectionAccount {
  account_label: string;
  credential_type: 'oauth_authcode' | 'static_token';
  is_default: boolean;
  scopes: string[];
  expires_at: string | null;
  upstream_subject: string | null;
  created_at: string;
  updated_at: string;
}

interface ServerConnections {
  server_id: string;
  server_name: string;
  namespace_prefix: string;
  oauth_capable: boolean;
  allow_static_token: boolean;
  static_token_help_url: string | null;
  accounts: ConnectionAccount[];
}

export function ConnectionsPage() {
  const { t } = useTranslation();
  const [servers, setServers] = useState<ServerConnections[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>('');

  // "Add account" dialog (works for both OAuth and static-token paths)
  const [addTarget, setAddTarget] = useState<ServerConnections | null>(null);
  const [addLabel, setAddLabel] = useState('');
  const [addToken, setAddToken] = useState('');
  const [addMode, setAddMode] = useState<'oauth' | 'static'>('oauth');
  const [submitting, setSubmitting] = useState(false);

  // Revoke confirmation
  const [revokeTarget, setRevokeTarget] = useState<{ server_id: string; account_label: string } | null>(null);

  // Highlight callback success / failure from URL fragment
  const [flash, setFlash] = useState<{ kind: 'connected' | 'error'; detail: string } | null>(null);

  const fetchAll = async (signal?: AbortSignal) => {
    try {
      const data = await api<ServerConnections[]>('/api/mcp/connections', { signal });
      setServers(data);
      setError('');
    } catch (err) {
      if (signal?.aborted) return;
      setError(err instanceof Error ? err.message : 'Failed to load connections');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    const controller = new AbortController();
    fetchAll(controller.signal);
    return () => controller.abort();
  }, []);

  // Parse the URL hash for `connected=...` / `error=...` / `need=...`
  // markers. Strip the fragment after we've consumed it so a refresh
  // doesn't re-fire the toast.
  useEffect(() => {
    if (typeof window === 'undefined') return;
    const hash = window.location.hash.replace(/^#/, '');
    if (!hash) return;
    const params = new URLSearchParams(hash);
    if (params.has('connected')) {
      setFlash({ kind: 'connected', detail: params.get('connected') ?? '' });
    } else if (params.has('error')) {
      setFlash({ kind: 'error', detail: params.get('error') ?? '' });
    }
    history.replaceState(null, '', window.location.pathname);
  }, []);

  const sortedServers = useMemo(() => {
    // Show servers that need user action (no accounts) first, then by name.
    return [...servers].sort((a, b) => {
      const aHas = a.accounts.length > 0 ? 1 : 0;
      const bHas = b.accounts.length > 0 ? 1 : 0;
      if (aHas !== bHas) return aHas - bHas;
      return a.server_name.localeCompare(b.server_name);
    });
  }, [servers]);

  const openAdd = (s: ServerConnections, defaultMode: 'oauth' | 'static') => {
    setAddTarget(s);
    setAddLabel('');
    setAddToken('');
    setAddMode(defaultMode);
  };

  const submitAdd = async () => {
    if (!addTarget) return;
    if (!addLabel.trim()) {
      toast.error(t('connections.labelRequired'));
      return;
    }
    setSubmitting(true);
    try {
      if (addMode === 'oauth') {
        const res = await apiPost<{ authorize_url: string }>(
          `/api/mcp/connections/${addTarget.server_id}/authorize`,
          { account_label: addLabel.trim() },
        );
        // Redirect the browser to the upstream authorize URL. Returning
        // here means the user closed the popup or denied — in that case
        // we'll get the error fragment back at /connections#error=...
        window.location.href = res.authorize_url;
      } else {
        if (!addToken.trim()) {
          toast.error(t('connections.tokenRequired'));
          setSubmitting(false);
          return;
        }
        await fetch(
          `/api/mcp/connections/${addTarget.server_id}/${encodeURIComponent(
            addLabel.trim(),
          )}/static-token`,
          {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            credentials: 'include',
            body: JSON.stringify({ token: addToken.trim() }),
          },
        ).then(async (r) => {
          if (!r.ok) throw new Error(await r.text());
        });
        toast.success(t('connections.tokenSaved'));
        setAddTarget(null);
        await fetchAll();
      }
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setSubmitting(false);
    }
  };

  const setDefault = async (server_id: string, account_label: string) => {
    try {
      await fetch(
        `/api/mcp/connections/${server_id}/${encodeURIComponent(account_label)}/default`,
        { method: 'PUT', credentials: 'include' },
      ).then(async (r) => {
        if (!r.ok) throw new Error(await r.text());
      });
      await fetchAll();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to set default');
    }
  };

  const revoke = async () => {
    if (!revokeTarget) return;
    try {
      await apiDelete(
        `/api/mcp/connections/${revokeTarget.server_id}/${encodeURIComponent(
          revokeTarget.account_label,
        )}`,
      );
      toast.success(t('connections.revoked'));
      setRevokeTarget(null);
      await fetchAll();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to revoke');
    }
  };

  return (
    <div className="flex flex-col flex-1 min-h-0 space-y-4">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('connections.title')}</h1>
        <p className="text-muted-foreground">{t('connections.subtitle')}</p>
      </div>

      {flash?.kind === 'connected' && (
        <Alert>
          <CheckCircle2 className="h-4 w-4" />
          <AlertDescription>{t('connections.connectedToast')}</AlertDescription>
        </Alert>
      )}
      {flash?.kind === 'error' && (
        <Alert variant="destructive">
          <AlertDescription>
            {t('connections.connectFailed')}: {flash.detail}
          </AlertDescription>
        </Alert>
      )}
      {error && (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {loading ? (
        <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
      ) : servers.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center justify-center py-10 text-center">
            <Plug className="h-10 w-10 text-muted-foreground mb-3" />
            <p className="text-sm text-muted-foreground">{t('connections.empty')}</p>
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-4">
          {sortedServers.map((s) => (
            <ServerCard
              key={s.server_id}
              server={s}
              onAddOauth={() => openAdd(s, 'oauth')}
              onAddStatic={() => openAdd(s, 'static')}
              onSetDefault={(label) => setDefault(s.server_id, label)}
              onRevoke={(label) =>
                setRevokeTarget({ server_id: s.server_id, account_label: label })
              }
              t={t}
            />
          ))}
        </div>
      )}

      <Dialog open={!!addTarget} onOpenChange={(o) => { if (!o) setAddTarget(null); }}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>
              {addMode === 'oauth'
                ? t('connections.connectOauth', { name: addTarget?.server_name ?? '' })
                : t('connections.pasteToken', { name: addTarget?.server_name ?? '' })}
            </DialogTitle>
            <DialogDescription>
              {addMode === 'oauth'
                ? t('connections.connectOauthDesc')
                : t('connections.pasteTokenDesc')}
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <div className="space-y-1">
              <Label htmlFor="conn-label">{t('connections.accountLabel')}</Label>
              <Input
                id="conn-label"
                value={addLabel}
                onChange={(e) => setAddLabel(e.target.value)}
                placeholder="work"
                maxLength={64}
              />
              <p className="text-xs text-muted-foreground">{t('connections.accountLabelHint')}</p>
            </div>
            {addMode === 'static' && (
              <div className="space-y-1">
                <Label htmlFor="conn-token">{t('connections.tokenLabel')}</Label>
                <Input
                  id="conn-token"
                  type="password"
                  value={addToken}
                  onChange={(e) => setAddToken(e.target.value)}
                  placeholder="ghp_..."
                />
                {addTarget?.static_token_help_url && (
                  <a
                    href={addTarget.static_token_help_url}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="text-xs text-primary inline-flex items-center gap-1"
                  >
                    {t('connections.howToGetToken')} <ExternalLink className="h-3 w-3" />
                  </a>
                )}
              </div>
            )}
            {addMode === 'oauth' && addTarget?.allow_static_token && (
              <button
                type="button"
                className="text-xs text-muted-foreground underline"
                onClick={() => setAddMode('static')}
              >
                {t('connections.useTokenInstead')}
              </button>
            )}
            {addMode === 'static' && addTarget?.oauth_capable && (
              <button
                type="button"
                className="text-xs text-muted-foreground underline"
                onClick={() => setAddMode('oauth')}
              >
                {t('connections.useOauthInstead')}
              </button>
            )}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setAddTarget(null)}>
              {t('common.cancel')}
            </Button>
            <Button onClick={submitAdd} disabled={submitting}>
              {addMode === 'oauth' ? t('connections.authorize') : t('connections.saveToken')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={revokeTarget !== null}
        onOpenChange={(open) => { if (!open) setRevokeTarget(null); }}
        title={t('connections.revokeTitle')}
        description={t('connections.revokeConfirm')}
        variant="destructive"
        confirmLabel={t('connections.revoke')}
        onConfirm={revoke}
      />
    </div>
  );
}

interface TestResult {
  success: boolean;
  message: string;
  latency_ms: number;
  tools_count?: number;
  tools?: { name: string; description?: string }[];
}

function ServerCard({
  server,
  onAddOauth,
  onAddStatic,
  onSetDefault,
  onRevoke,
  t,
}: {
  server: ServerConnections;
  onAddOauth: () => void;
  onAddStatic: () => void;
  onSetDefault: (label: string) => void;
  onRevoke: (label: string) => void;
  t: (key: string, options?: Record<string, unknown>) => string;
}) {
  const empty = server.accounts.length === 0;
  // Test state is per-account_label and lives inside the card — the
  // parent page never needs to read it, and keeping it local avoids
  // threading another callback through ServerCard's interface.
  const [testing, setTesting] = useState<string | null>(null);
  const [results, setResults] = useState<Record<string, TestResult>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const handleTest = async (label: string) => {
    setTesting(label);
    try {
      const res = await apiPost<TestResult>(
        `/api/mcp/connections/${server.server_id}/${encodeURIComponent(label)}/test`,
        {},
      );
      setResults((prev) => ({ ...prev, [label]: res }));
      // Reset the disclosure so a fresh result starts collapsed —
      // the user can decide whether to drill in.
      setExpanded((prev) => ({ ...prev, [label]: false }));
    } catch (err) {
      setResults((prev) => ({
        ...prev,
        [label]: {
          success: false,
          message: err instanceof Error ? err.message : t('connections.testFailed'),
          latency_ms: 0,
        },
      }));
    } finally {
      setTesting(null);
    }
  };
  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between gap-4 space-y-0 pb-3">
        <div>
          <CardTitle className="text-base">{server.server_name}</CardTitle>
          <p className="text-xs text-muted-foreground font-mono">{server.namespace_prefix}__</p>
        </div>
        <div className="flex gap-2">
          {server.oauth_capable && (
            <Button size="sm" variant="default" onClick={onAddOauth}>
              <Plus className="h-3 w-3 mr-1" />
              {empty ? t('connections.connect') : t('connections.addAccount')}
            </Button>
          )}
          {server.allow_static_token && !server.oauth_capable && (
            <Button size="sm" variant="default" onClick={onAddStatic}>
              <KeyRound className="h-3 w-3 mr-1" />
              {empty ? t('connections.pasteToken', { name: '' }) : t('connections.addAccount')}
            </Button>
          )}
          {server.allow_static_token && server.oauth_capable && (
            <Button size="sm" variant="outline" onClick={onAddStatic}>
              <KeyRound className="h-3 w-3 mr-1" />
              {t('connections.useToken')}
            </Button>
          )}
        </div>
      </CardHeader>
      <CardContent className="pt-0">
        {empty ? (
          <p className="text-sm text-muted-foreground">
            {server.oauth_capable || server.allow_static_token
              ? t('connections.notConnected')
              : t('connections.anonymous')}
          </p>
        ) : (
          <ul className="divide-y">
            {server.accounts.map((a) => {
              const result = results[a.account_label];
              const isTesting = testing === a.account_label;
              const isExpanded = expanded[a.account_label] ?? false;
              return (
                <li key={a.account_label} className="py-2">
                  <div className="flex items-center justify-between gap-3">
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        {/* Upstream identity goes first when we have it
                            (e.g. `@octocat`, `user@example.com`) so users
                            can tell their accounts apart at a glance.
                            Falls back to the user-supplied account_label
                            when the upstream gave us nothing usable. */}
                        {a.upstream_subject ? (
                          <>
                            <span className="font-medium">{a.upstream_subject}</span>
                            <span className="text-xs text-muted-foreground">
                              ({a.account_label})
                            </span>
                          </>
                        ) : (
                          <span className="font-medium">{a.account_label}</span>
                        )}
                        {a.is_default && (
                          <Badge variant="secondary" className="text-xs">
                            {t('connections.default')}
                          </Badge>
                        )}
                        <Badge variant="outline" className="text-xs">
                          {a.credential_type === 'oauth_authcode' ? 'OAuth' : 'Token'}
                        </Badge>
                      </div>
                      <div className="text-xs text-muted-foreground space-x-2">
                        {a.scopes.length > 0 && (
                          <span className="font-mono">{a.scopes.join(' ')}</span>
                        )}
                        {a.expires_at && (
                          <span>
                            {t('connections.expiresAt', {
                              when: format(new Date(a.expires_at), 'yyyy-MM-dd HH:mm'),
                            })}
                          </span>
                        )}
                      </div>
                    </div>
                    <div className="flex shrink-0 gap-1">
                      {!a.is_default && (
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => onSetDefault(a.account_label)}
                        >
                          {t('connections.setDefault')}
                        </Button>
                      )}
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => handleTest(a.account_label)}
                        disabled={isTesting}
                        title={t('connections.test')}
                      >
                        {isTesting ? (
                          <Loader2 className="h-3 w-3 animate-spin" />
                        ) : (
                          <Activity className="h-3 w-3" />
                        )}
                      </Button>
                      {a.credential_type === 'oauth_authcode' && (
                        <Button size="sm" variant="ghost" onClick={onAddOauth}>
                          <RefreshCw className="h-3 w-3" />
                        </Button>
                      )}
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => onRevoke(a.account_label)}
                        title={t('connections.revoke')}
                      >
                        <Trash2 className="h-3 w-3 text-destructive" />
                      </Button>
                    </div>
                  </div>
                  {result && (
                    <div
                      className={cn(
                        'mt-2 rounded-md border px-2.5 py-1.5 text-xs',
                        result.success
                          ? 'border-emerald-500/30 bg-emerald-500/5 text-emerald-700 dark:text-emerald-400'
                          : 'border-destructive/30 bg-destructive/5 text-destructive',
                      )}
                    >
                      <div className="flex items-center justify-between gap-2">
                        <span className="truncate">
                          {result.success ? '✓' : '✗'} {result.message}
                          {result.success && (
                            <span className="ml-2 font-mono text-muted-foreground">
                              {result.latency_ms}ms · {result.tools_count} {t('connections.toolsLabel')}
                            </span>
                          )}
                        </span>
                        {result.success && result.tools && result.tools.length > 0 && (
                          <button
                            type="button"
                            onClick={() =>
                              setExpanded((prev) => ({
                                ...prev,
                                [a.account_label]: !prev[a.account_label],
                              }))
                            }
                            className="inline-flex items-center gap-0.5 text-muted-foreground hover:text-foreground"
                          >
                            {isExpanded ? (
                              <>
                                <ChevronDown className="h-3 w-3" />
                                {t('connections.hideTools')}
                              </>
                            ) : (
                              <>
                                <ChevronRight className="h-3 w-3" />
                                {t('connections.showTools')}
                              </>
                            )}
                          </button>
                        )}
                      </div>
                      {isExpanded && result.tools && (
                        <ul className="mt-2 max-h-48 space-y-0.5 overflow-y-auto border-t pt-1.5 font-mono text-[11px] text-muted-foreground">
                          {result.tools.map((tool) => (
                            <li key={tool.name} className="truncate">
                              <span className="text-foreground">{tool.name}</span>
                              {tool.description && (
                                <span className="ml-2 opacity-70">— {tool.description}</span>
                              )}
                            </li>
                          ))}
                        </ul>
                      )}
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}
