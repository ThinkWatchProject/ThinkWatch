import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog';
import { Search, Download, CheckCircle2, Plus, Trash2, Loader2, Star, RefreshCw, Globe, Lock, KeyRound } from 'lucide-react';
import { api, apiPost, hasPermission } from '@/lib/api';
import { slugifyPrefix, resolveCollision, sanitizePrefixInput } from '@/lib/prefix-utils';
import { Skeleton } from '@/components/ui/skeleton';
import { toast } from 'sonner';

interface StoreTemplate {
  id: string;
  slug: string;
  name: string;
  description: string | null;
  icon_url: string | null;
  author: string | null;
  category: string | null;
  tags: string[];
  endpoint_template: string | null;
  oauth_issuer: string | null;
  oauth_token_endpoint: string | null;
  oauth_userinfo_endpoint: string | null;
  allow_static_token: boolean;
  static_token_help_url: string | null;
  auth_instructions: string | null;
  deploy_type: string | null;
  deploy_command: string | null;
  deploy_docs_url: string | null;
  homepage_url: string | null;
  repo_url: string | null;
  featured: boolean;
  install_count: number;
  installed: boolean;
}

interface CategoryCount {
  category: string;
  count: number;
}

const CATEGORIES = ['developer', 'database', 'communication', 'cloud', 'utility', 'knowledge', 'productivity'] as const;

/** Pick the right language from a bilingual string stored as "en\n---\nzh". */
function i18nText(text: string | null | undefined, lang: string): string {
  if (!text) return '';
  const parts = text.split('\n---\n');
  if (parts.length < 2) return text;
  return lang.startsWith('zh') ? (parts[1] || parts[0]) : parts[0];
}

export function McpStorePage() {
  const { t, i18n } = useTranslation();
  const [templates, setTemplates] = useState<StoreTemplate[]>([]);
  const [categories, setCategories] = useState<CategoryCount[]>([]);
  const [loading, setLoading] = useState(true);
  const [searchQuery, setSearchQuery] = useState('');
  const [activeCategory, setActiveCategory] = useState<string | null>(null);

  // Install dialog state
  const [installTemplate, setInstallTemplate] = useState<StoreTemplate | null>(null);
  const [endpointUrl, setEndpointUrl] = useState('');
  const [customHeaders, setCustomHeaders] = useState<[string, string][]>([]);
  const [serverName, setServerName] = useState('');
  const [serverPrefix, setServerPrefix] = useState('');
  // Whether the user has manually edited the prefix — if so, we stop
  // auto-regenerating it from the server name.
  const [prefixManuallyEdited, setPrefixManuallyEdited] = useState(false);
  // Existing servers — used client-side to preview what name/prefix a fresh
  // install will actually receive after collision resolution.
  const [existingServers, setExistingServers] = useState<{ name: string; namespace_prefix: string }[]>([]);
  const [syncing, setSyncing] = useState(false);
  const [installing, setInstalling] = useState(false);

  const fetchTemplates = async () => {
    try {
      const params = new URLSearchParams();
      if (activeCategory) params.set('category', activeCategory);
      if (searchQuery) params.set('search', searchQuery);
      const qs = params.toString();
      const data = await api<StoreTemplate[]>(`/api/mcp/store${qs ? `?${qs}` : ''}`);
      setTemplates(data);
    } catch {
      /* ignore */
    } finally {
      setLoading(false);
    }
  };

  const fetchCategories = async () => {
    try {
      const data = await api<CategoryCount[]>('/api/mcp/store/categories');
      setCategories(data);
    } catch {
      /* ignore */
    }
  };

  useEffect(() => {
    void fetchCategories();
    // Snapshot existing servers once — used only for client-side collision
    // preview. Backend still has authoritative UNIQUE enforcement.
    api<{ name: string; namespace_prefix: string }[]>('/api/mcp/servers')
      .then(setExistingServers)
      .catch(() => { /* ignore — preview will just not show collisions */ });
  }, []);

  useEffect(() => {
    setLoading(true);
    const timer = setTimeout(() => {
      void fetchTemplates();
    }, 200);
    return () => clearTimeout(timer);
  }, [searchQuery, activeCategory]);

  const openInstallDialog = (tmpl: StoreTemplate) => {
    setInstallTemplate(tmpl);
    setEndpointUrl(tmpl.endpoint_template ?? '');
    setCustomHeaders([]);
    setServerName(tmpl.name);
    setServerPrefix(tmpl.slug.replace(/-/g, '_'));
    setPrefixManuallyEdited(false);
  };

  // Live preview: what will `name` and `namespace_prefix` actually look like
  // once the backend resolves collisions? Mirrors backend logic, runs on
  // the current snapshot of `existingServers`.
  const takenSets = useMemo(() => ({
    names: new Set(existingServers.map((s) => s.name)),
    prefixes: new Set(existingServers.map((s) => s.namespace_prefix)),
  }), [existingServers]);

  const resolvedInstall = useMemo(() => {
    if (!installTemplate || !serverName.trim()) return null;
    const basePrefix = prefixManuallyEdited && serverPrefix
      ? serverPrefix
      : slugifyPrefix(serverName);
    if (!basePrefix) return null;
    return resolveCollision(serverName.trim(), basePrefix, takenSets.names, takenSets.prefixes);
  }, [installTemplate, serverName, serverPrefix, prefixManuallyEdited, takenSets]);

  const handleInstall = async (e: FormEvent) => {
    e.preventDefault();
    if (!installTemplate) return;
    setInstalling(true);
    try {
      await apiPost(`/api/mcp/store/${installTemplate.slug}/install`, {
        name: resolvedInstall?.name ?? serverName,
        namespace_prefix: resolvedInstall?.prefix ?? serverPrefix,
        endpoint_url: endpointUrl || undefined,
        custom_headers:
          customHeaders.length > 0
            ? Object.fromEntries(customHeaders.filter(([k]) => k.trim()))
            : undefined,
      });
      toast.success(t('mcpStore.installSuccess'));
      setInstallTemplate(null);
      // Refresh store listing + existing-server snapshot so the just-installed
      // template shows the "installed" badge and future collision previews
      // account for the new name/prefix.
      void fetchTemplates();
      api<{ name: string; namespace_prefix: string }[]>('/api/mcp/servers')
        .then(setExistingServers)
        .catch(() => { /* ignore */ });
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('common.operationFailed'));
    } finally {
      setInstalling(false);
    }
  };

  const addHeader = () => setCustomHeaders((h) => [...h, ['', '']]);
  const removeHeader = (i: number) =>
    setCustomHeaders((h) => h.filter((_, idx) => idx !== i));
  const updateHeader = (i: number, field: 0 | 1, value: string) =>
    setCustomHeaders((h) => h.map((pair, idx) => (idx === i ? (field === 0 ? [value, pair[1]] : [pair[0], value]) as [string, string] : pair)));

  const isHosted = installTemplate?.deploy_type === 'hosted';
  const needsEndpoint = !isHosted || !installTemplate?.endpoint_template;

  // Separate featured templates when no filter is active
  const featuredTemplates =
    !searchQuery && !activeCategory ? templates.filter((t) => t.featured) : [];
  const regularTemplates =
    !searchQuery && !activeCategory ? templates.filter((t) => !t.featured) : templates;

  const getCategoryCount = (cat: string) => {
    const c = categories.find((c) => c.category === cat);
    return c?.count ?? 0;
  };

  const totalCount = categories.reduce((sum, c) => sum + c.count, 0);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold">{t('mcpStore.title')}</h1>
          <p className="text-muted-foreground">{t('mcpStore.subtitle')}</p>
        </div>
        {hasPermission('settings:write') && (
          <Button
            variant="outline"
            size="sm"
            disabled={syncing}
            onClick={async () => {
              setSyncing(true);
              try {
                const res = await apiPost<{ count: number }>('/api/admin/mcp-store/sync', {});
                toast.success(t('mcpStore.syncSuccess', { count: res.count }));
                await fetchTemplates();
              } catch (err) {
                toast.error(err instanceof Error ? err.message : 'Sync failed');
              } finally {
                setSyncing(false);
              }
            }}
          >
            {syncing ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
            {t('mcpStore.syncRegistry')}
          </Button>
        )}
      </div>

      {/* Search */}
      <div className="relative max-w-md">
        <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
        <Input
          placeholder={t('mcpStore.search')}
          value={searchQuery}
          onChange={(e) => setSearchQuery(e.target.value)}
          className="pl-9"
        />
      </div>

      {/* Category filter chips */}
      <div className="flex flex-wrap gap-2">
        <Button
          variant={activeCategory === null ? 'default' : 'outline'}
          size="sm"
          onClick={() => setActiveCategory(null)}
        >
          {t('mcpStore.allCategories')} ({totalCount})
        </Button>
        {CATEGORIES.map((cat) => (
          <Button
            key={cat}
            variant={activeCategory === cat ? 'default' : 'outline'}
            size="sm"
            onClick={() => setActiveCategory(activeCategory === cat ? null : cat)}
          >
            {t(`mcpStore.category.${cat}`)} ({getCategoryCount(cat)})
          </Button>
        ))}
      </div>

      {loading ? (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {Array.from({ length: 6 }).map((_, i) => (
            <Card key={i}>
              <CardHeader>
                <Skeleton className="h-5 w-32" />
              </CardHeader>
              <CardContent>
                <Skeleton className="h-4 w-full" />
                <Skeleton className="mt-2 h-4 w-2/3" />
              </CardContent>
            </Card>
          ))}
        </div>
      ) : templates.length === 0 ? (
        <div className="py-16 text-center text-muted-foreground">
          {t('mcpStore.noTemplates')}
        </div>
      ) : (
        <>
          {/* Featured section */}
          {featuredTemplates.length > 0 && (
            <div className="space-y-3">
              <h2 className="flex items-center gap-2 text-lg font-semibold">
                <Star className="h-5 w-5 text-yellow-500" />
                {t('mcpStore.featured')}
              </h2>
              <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
                {featuredTemplates.map((tmpl) => (
                  <TemplateCard
                    key={tmpl.id}
                    template={tmpl}
                    onInstall={() => openInstallDialog(tmpl)}
                    t={t}
                    lang={i18n.language}
                  />
                ))}
              </div>
            </div>
          )}

          {/* All templates */}
          {regularTemplates.length > 0 && (
            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
              {regularTemplates.map((tmpl) => (
                <TemplateCard
                  key={tmpl.id}
                  template={tmpl}
                  onInstall={() => openInstallDialog(tmpl)}
                  t={t}
                  lang={i18n.language}
                />
              ))}
            </div>
          )}
        </>
      )}

      {/* Install dialog */}
      <Dialog
        open={installTemplate !== null}
        onOpenChange={(open) => {
          if (!open) setInstallTemplate(null);
        }}
      >
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>
              {t('mcpStore.installTitle', { name: installTemplate?.name })}
            </DialogTitle>
            <DialogDescription>
              {i18nText(installTemplate?.description, i18n.language)}
            </DialogDescription>
          </DialogHeader>
          <form onSubmit={handleInstall} className="space-y-4">
            {installTemplate?.installed && (
              <div className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 p-3 text-xs text-amber-700 dark:text-amber-400">
                <CheckCircle2 className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                <span>{t('mcpStore.installAgainWarning')}</span>
              </div>
            )}

            {/* Name + Namespace prefix — pre-populated from template, editable */}
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-1.5">
                <Label htmlFor="install-name">{t('common.name')}</Label>
                <Input
                  id="install-name"
                  value={serverName}
                  onChange={(e) => setServerName(e.target.value)}
                  required
                />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="install-prefix">{t('mcpServers.namespacePrefix')}</Label>
                <Input
                  id="install-prefix"
                  value={prefixManuallyEdited ? serverPrefix : (resolvedInstall?.prefix ?? slugifyPrefix(serverName))}
                  onChange={(e) => {
                    setPrefixManuallyEdited(true);
                    setServerPrefix(sanitizePrefixInput(e.target.value));
                  }}
                  pattern="[a-z0-9_]{1,32}"
                  maxLength={32}
                />
              </div>
            </div>
            {resolvedInstall && (resolvedInstall.name !== serverName || resolvedInstall.prefix !== serverPrefix) && (
              <p className="text-xs text-muted-foreground">
                {t('mcpServers.willBeStoredAs')}{' '}
                <code className="rounded bg-muted px-1 font-mono">{resolvedInstall.name}</code>
                {' / '}
                <code className="rounded bg-muted px-1 font-mono">{resolvedInstall.prefix}</code>
              </p>
            )}

            {/* Auth / deploy instructions */}
            {installTemplate?.auth_instructions && (
              <div className="rounded-md border bg-muted/50 p-3 text-sm">
                <p className="mb-1 font-medium">
                  {isHosted ? t('mcpStore.authInstructions') : t('mcpStore.deployInstructions')}
                </p>
                <p className="text-muted-foreground whitespace-pre-wrap">
                  {i18nText(installTemplate.auth_instructions, i18n.language)}
                </p>
              </div>
            )}

            {/* Endpoint URL — shown for non-hosted or when no template endpoint */}
            {needsEndpoint && (
              <div className="space-y-1.5">
                <Label>{t('mcpStore.endpointUrl')}</Label>
                <Input
                  value={endpointUrl}
                  onChange={(e) => setEndpointUrl(e.target.value)}
                  placeholder="https://..."
                  required
                />
              </div>
            )}

            {/* Note about per-user credentials */}
            {(installTemplate?.oauth_issuer || installTemplate?.allow_static_token) && (
              <div className="rounded-md border bg-muted/40 p-3 text-xs text-muted-foreground">
                After install, each user authorizes their own account at
                <code className="mx-1 rounded bg-muted px-1">/connections</code>
                — admins don't paste a shared token here.
              </div>
            )}

            {/* Custom headers */}
            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <Label className="text-sm">{t('mcpStore.customHeaders')}</Label>
                <Button type="button" variant="ghost" size="sm" onClick={addHeader}>
                  <Plus className="mr-1 h-3 w-3" /> {t('common.add')}
                </Button>
              </div>
              {customHeaders.map(([k, v], i) => (
                <div key={i} className="flex items-center gap-2">
                  <Input
                    placeholder={t('mcpStore.headerName')}
                    aria-label={t('mcpStore.headerName')}
                    value={k}
                    onChange={(e) => updateHeader(i, 0, e.target.value)}
                    className="flex-1"
                  />
                  <Input
                    placeholder={t('mcpStore.headerValue')}
                    aria-label={t('mcpStore.headerValue')}
                    value={v}
                    onChange={(e) => updateHeader(i, 1, e.target.value)}
                    className="flex-1"
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    aria-label={t('common.remove')}
                    title={t('common.remove')}
                    onClick={() => removeHeader(i)}
                  >
                    <Trash2 className="h-4 w-4" />
                  </Button>
                </div>
              ))}
            </div>

            <DialogFooter>
              <Button
                type="submit"
                disabled={installing}
              >
                {installing ? (
                  <>
                    <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                    {t('mcpStore.installing')}
                  </>
                ) : (
                  <>
                    <Download className="mr-2 h-4 w-4" />
                    {t('mcpStore.install')}
                  </>
                )}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function TemplateAuthBadge({
  template,
  t,
}: {
  template: StoreTemplate;
  t: (key: string) => string;
}) {
  const hasOauth = !!template.oauth_issuer;
  const hasStatic = template.allow_static_token;
  if (hasOauth) {
    return (
      <Badge variant="outline" className="gap-1">
        <Lock className="h-3 w-3" /> OAuth
      </Badge>
    );
  }
  if (hasStatic) {
    return (
      <Badge variant="outline" className="gap-1">
        <KeyRound className="h-3 w-3" /> {t('mcpStore.staticToken')}
      </Badge>
    );
  }
  return (
    <Badge variant="outline" className="gap-1">
      <Globe className="h-3 w-3" /> {t('mcpStore.noAuth')}
    </Badge>
  );
}

function TemplateCard({
  template,
  onInstall,
  t,
  lang,
}: {
  template: StoreTemplate;
  lang: string;
  onInstall: () => void;
  t: (key: string) => string;
}) {
  return (
    <Card className="card-interactive flex flex-col justify-between">
      <CardHeader className="pb-2">
        <div className="flex items-start justify-between gap-2">
          <div className="flex min-w-0 items-center gap-1.5">
            {template.installed && (
              <span
                title={t('mcpStore.installedTooltip')}
                className="inline-flex h-1.5 w-1.5 shrink-0 rounded-full bg-emerald-500 shadow-[0_0_6px_theme(colors.emerald.500/0.7)]"
              />
            )}
            <CardTitle className="truncate text-base">{template.name}</CardTitle>
          </div>
          {template.category && (
            <Badge variant="secondary" className="shrink-0 text-xs">
              {t(`mcpStore.category.${template.category}`)}
            </Badge>
          )}
        </div>
        {template.author && (
          <p className="text-xs text-muted-foreground">{template.author}</p>
        )}
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <p className="line-clamp-2 text-sm text-muted-foreground">
          {i18nText(template.description, lang)}
        </p>
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2">
            <TemplateAuthBadge template={template} t={t} />
            <span className="text-xs text-muted-foreground">
              {template.install_count} installs
            </span>
          </div>
          <Button
            size="sm"
            variant={template.installed ? 'outline' : 'default'}
            onClick={onInstall}
            disabled={!hasPermission('mcp_servers:create')}
            title={template.installed ? t('mcpStore.installAgainHint') : undefined}
          >
            {template.installed ? (
              <>
                <CheckCircle2 className="mr-1 h-3 w-3 text-emerald-500" />
                {t('mcpStore.installAgain')}
              </>
            ) : (
              <>
                <Download className="mr-1 h-3 w-3" />
                {t('mcpStore.install')}
              </>
            )}
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
