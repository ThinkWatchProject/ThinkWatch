import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from '@/components/ui/card';
import { ServiceLogo } from '@/components/ui/service-logo';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Badge } from '@/components/ui/badge';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { AuthModeBadge } from '@/components/mcp/auth-mode-badge';
import { api, apiPost, apiPut, apiDelete } from '@/lib/api';
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
import { safeExternalHref } from '@/lib/utils';

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
  /** Optional human-friendly label set by the admin. Falls back to
   *  `server_name` when null — useful when two installs of the same
   *  template would otherwise look identical here. */
  display_label?: string | null;
  namespace_prefix: string;
  /** Single-valued auth shape — `'oauth'` or `'static'` (anonymous
   *  servers are filtered server-side and never appear here). Drives
   *  which UI renders in the connect dialog. */
  auth_shape: 'oauth' | 'static';
  static_token_help_url: string | null;
  /** Header name + template the gateway will send the user-supplied
   *  token under. Used to render the "submitted as `…`" preview in
   *  the paste dialog. */
  auth_header_name: string;
  auth_value_template: string;
  accounts: ConnectionAccount[];
}

/** Pick the user-facing name for a server. */
function serverDisplay(s: ServerConnections): string {
  return s.display_label && s.display_label.trim() !== ''
    ? s.display_label
    : s.server_name;
}

export function ConnectionsPage() {
  const { t } = useTranslation();
  const [servers, setServers] = useState<ServerConnections[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string>('');

  // "Add account" dialog. The dialog's mode is derived from the
  // target server's `auth_shape` — a server is OAuth or static, never
  // both, so there's no admin / user choice between flows.
  const [addTarget, setAddTarget] = useState<ServerConnections | null>(null);
  const [addLabel, setAddLabel] = useState('');
  const [addToken, setAddToken] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const addMode: 'oauth' | 'static' = addTarget?.auth_shape ?? 'oauth';

  // Revoke confirmation
  const [revokeTarget, setRevokeTarget] = useState<{ server_id: string; account_label: string } | null>(null);

  // Highlight callback success / failure from URL fragment
  // Seeded from the OAuth callback fragment in the initialiser rather than
  // an effect: an effect paints the page once without the banner, so the
  // "connected" confirmation arrives as a flash of layout shift on the very
  // screen the user is checking for it.
  const [flash] = useState<{ kind: 'connected' | 'error'; detail: string } | null>(
    () => {
      if (typeof window === 'undefined') return null;
      const hash = window.location.hash.replace(/^#/, '');
      if (!hash) return null;
      const params = new URLSearchParams(hash);
      if (params.has('connected')) {
        return { kind: 'connected' as const, detail: params.get('connected') ?? '' };
      }
      if (params.has('error')) return { kind: 'error' as const, detail: params.get('error') ?? '' };
      return null;
    },
  );

  const fetchAll = async (signal?: AbortSignal) => {
    try {
      const data = await api<ServerConnections[]>('/api/mcp/connections', { signal });
      setServers(data);
      setError('');
    } catch (err) {
      if (signal?.aborted) return;
      setError(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    const controller = new AbortController();
    // Async loader: its first statement is the `await`, so every setState
    // inside runs in the continuation — never synchronously with this
    // effect, and never as a cascading render. The rule's cross-function
    // analysis does not model `await`.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    fetchAll(controller.signal);
    return () => controller.abort();
  }, []);

  // Parse the URL hash for `connected=...` / `error=...` / `need=...`
  // markers. Strip the fragment after we've consumed it so a refresh
  // doesn't re-fire the toast.
  useEffect(() => {
    if (typeof window === 'undefined') return;
    if (!window.location.hash) return;
    // `flash` is seeded from the hash in its own initialiser (see above);
    // this effect only has to clean the URL back up, so a refresh doesn't
    // re-fire the banner.
    history.replaceState(null, '', window.location.pathname);
  }, []);

  const sortedServers = useMemo(() => {
    // Show servers that need user action (no accounts) first, then
    // alphabetically by *display* name — when admins set custom
    // display_labels, sorting by the system `server_name` would put
    // rows in an order that doesn't match what users see.
    return [...servers].sort((a, b) => {
      const aHas = a.accounts.length > 0 ? 1 : 0;
      const bHas = b.accounts.length > 0 ? 1 : 0;
      if (aHas !== bHas) return aHas - bHas;
      return serverDisplay(a).localeCompare(serverDisplay(b));
    });
  }, [servers]);

  const openAdd = (s: ServerConnections) => {
    setAddTarget(s);
    setAddLabel('');
    setAddToken('');
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
        // Must go through `apiPut`, not raw `fetch` — the api client
        // attaches ECDSA signature headers (read from IndexedDB) and
        // retries once after re-registering the key on 401. Bypassing
        // that meant any user with a registered public key got
        // `401 Unauthorized` from `verify_signature` middleware.
        await apiPut(
          `/api/mcp/connections/${addTarget.server_id}/${encodeURIComponent(
            addLabel.trim(),
          )}/static-token`,
          { token: addToken.trim() },
        );

        // Verify the token works by exercising the test endpoint. If
        // the upstream rejects the bearer (typical for typo'd PATs),
        // the user finds out NOW, not on their first tool call.
        // The credential is already saved — they can revoke from the
        // row if the test fails.
        try {
          const testResult = await apiPost<{ success: boolean; message: string }>(
            `/api/mcp/connections/${addTarget.server_id}/${encodeURIComponent(
              addLabel.trim(),
            )}/test`,
            {},
          );
          if (testResult.success) {
            toast.success(t('connections.tokenSavedAndVerified'));
          } else {
            toast.warning(
              t('connections.tokenSavedButTestFailed', { msg: testResult.message }),
              { duration: 10000 },
            );
          }
        } catch {
          // Test endpoint may not exist for some configs; fall back to
          // plain "saved" — the token is in the DB regardless.
          toast.success(t('connections.tokenSaved'));
        }
        setAddTarget(null);
        await fetchAll();
      }
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setSubmitting(false);
    }
  };

  const setDefault = async (server_id: string, account_label: string) => {
    try {
      // Same signing-bypass story as the static-token PUT above —
      // raw fetch skips the ECDSA headers and gets 401 from
      // verify_signature when a public key is registered.
      await apiPut(
        `/api/mcp/connections/${server_id}/${encodeURIComponent(account_label)}/default`,
        {},
      );
      await fetchAll();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.error'));
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
      toast.error(err instanceof Error ? err.message : t('common.error'));
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
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {[...Array(3)].map((_, i) => (
            <Card key={i}>
              <CardHeader className="flex flex-row items-center gap-3">
                <Skeleton className="h-10 w-10 rounded" />
                <div className="space-y-2 flex-1">
                  <Skeleton className="h-4 w-32" />
                  <Skeleton className="h-3 w-24" />
                </div>
              </CardHeader>
              <CardContent>
                <Skeleton className="h-3 w-full" />
                <Skeleton className="h-3 w-3/4 mt-2" />
              </CardContent>
            </Card>
          ))}
        </div>
      ) : servers.length === 0 ? (
        // This page is strictly about a user authorizing their own
        // account against admin-registered MCP servers. Don't surface
        // "browse store" / "register server" CTAs even for admins —
        // those actions live on /mcp/store and /mcp/servers (and are
        // already in the sidebar nav). Mixing in admin-management
        // semantics here muddied the page's purpose.
        <Card>
          <CardContent className="flex flex-col items-center justify-center gap-3 py-10 text-center">
            <Plug className="h-10 w-10 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">{t('connections.empty')}</p>
          </CardContent>
        </Card>
      ) : (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {sortedServers.map((s) => (
            <ServerCard
              key={s.server_id}
              server={s}
              onAdd={() => openAdd(s)}
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
                ? t('connections.connectOauth', { name: addTarget ? serverDisplay(addTarget) : '' })
                : t('connections.pasteToken', { name: addTarget ? serverDisplay(addTarget) : '' })}
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
                {addTarget && (
                  <div className="rounded border bg-muted/30 px-2 py-1 text-[11px] text-muted-foreground">
                    {t('connections.headerPreviewLabel')}:{' '}
                    <code className="font-mono">
                      {addTarget.auth_header_name || 'Authorization'}:{' '}
                      {(addTarget.auth_value_template || 'Bearer {{token}}').replaceAll(
                        '{{token}}',
                        addToken.length > 0 ? `${addToken.slice(0, 6)}…` : '••••••••',
                      )}
                    </code>
                  </div>
                )}
                {/* `static_token_help_url` is admin-supplied free
                    text, so a `javascript:` href would execute in
                    the user's session when clicked. `rel` flags don't
                    block that — only scheme validation does. Render
                    no link if the value isn't a real http(s) URL. */}
                {(() => {
                  const safe = safeExternalHref(addTarget?.static_token_help_url);
                  return safe ? (
                    <a
                      href={safe}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="text-xs text-primary inline-flex items-center gap-1"
                    >
                      {t('connections.howToGetToken')} <ExternalLink className="h-3 w-3" />
                    </a>
                  ) : null;
                })()}
              </div>
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
  onAdd,
  onSetDefault,
  onRevoke,
  t,
}: {
  server: ServerConnections;
  onAdd: () => void;
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
    // Tile-style integrations card: logo on the left, name + mode
    // stacked next to it, CTA(s) on a separated muted footer strip.
    // Modeled on the /mcp/store template tiles so the two pages feel
    // like the same product surface. `flex flex-col` lets the footer
    // anchor to the bottom regardless of how many accounts the
    // middle section has.
    <Card data-size="sm" className="flex flex-col">
      <CardHeader>
        <div className="flex items-start gap-2.5">
          <ServiceLogo
            service={server.server_name}
            className="size-9 rounded-md shrink-0"
          />
          <div className="min-w-0 flex-1 space-y-0.5">
            <CardTitle className="truncate text-sm">{serverDisplay(server)}</CardTitle>
            <div className="flex items-center gap-1.5">
              <AuthModeBadge mode={server.auth_shape} />
              {server.display_label && server.display_label !== server.server_name && (
                <span className="truncate font-mono text-[10px] text-muted-foreground/70">
                  {server.server_name}
                </span>
              )}
            </div>
          </div>
        </div>
      </CardHeader>
      {/* Account list — only rendered when the user has connected
          at least once. `flex-1` lets the footer stick to the bottom
          when this card has more accounts than its grid neighbors. */}
      {!empty && (
        <CardContent className="flex-1">
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
                        <Button size="sm" variant="ghost" onClick={onAdd}>
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
                              {result.latency_ms}ms · {result.tools_count ?? result.tools?.length ?? 0} {t('connections.toolsLabel')}
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
        </CardContent>
      )}
      <CardFooter className="gap-2">
        {/* Single-shape model: a server is OAuth or static, never both,
            so we render exactly one CTA. The dialog mode picks itself
            up from `addTarget.auth_shape` when opened. */}
        {server.auth_shape === 'oauth' && (
          <Button size="sm" variant="default" className="flex-1" onClick={onAdd}>
            <Plus className="h-3 w-3 mr-1" />
            {empty ? t('connections.connect') : t('connections.addAccount')}
          </Button>
        )}
        {server.auth_shape === 'static' && (
          <Button size="sm" variant="default" className="flex-1" onClick={onAdd}>
            <KeyRound className="h-3 w-3 mr-1" />
            {empty ? t('connections.useToken') : t('connections.addAccount')}
          </Button>
        )}
      </CardFooter>
    </Card>
  );
}
