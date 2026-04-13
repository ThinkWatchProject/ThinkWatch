import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from '@tanstack/react-router';
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
import { Search, Download, CheckCircle2, Plus, Trash2, Loader2, Star, RefreshCw } from 'lucide-react';
import { api, apiPost, hasPermission } from '@/lib/api';
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
  transport_type: string;
  auth_type: string | null;
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

const CATEGORIES = ['developer', 'database', 'communication', 'cloud', 'utility'] as const;

export function McpStorePage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [templates, setTemplates] = useState<StoreTemplate[]>([]);
  const [categories, setCategories] = useState<CategoryCount[]>([]);
  const [loading, setLoading] = useState(true);
  const [searchQuery, setSearchQuery] = useState('');
  const [activeCategory, setActiveCategory] = useState<string | null>(null);

  // Install dialog state
  const [installTemplate, setInstallTemplate] = useState<StoreTemplate | null>(null);
  const [endpointUrl, setEndpointUrl] = useState('');
  const [authSecret, setAuthSecret] = useState('');
  const [customHeaders, setCustomHeaders] = useState<[string, string][]>([]);
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
    setAuthSecret('');
    setCustomHeaders([]);
  };

  const handleInstall = async (e: FormEvent) => {
    e.preventDefault();
    if (!installTemplate) return;
    setInstalling(true);
    try {
      await apiPost(`/api/mcp/store/${installTemplate.slug}/install`, {
        endpoint_url: endpointUrl || undefined,
        auth_secret: authSecret || undefined,
        custom_headers:
          customHeaders.length > 0
            ? Object.fromEntries(customHeaders.filter(([k]) => k.trim()))
            : undefined,
      });
      toast.success(t('mcpStore.installSuccess'));
      setInstallTemplate(null);
      void navigate({ to: '/mcp/servers' });
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
              {installTemplate?.description}
            </DialogDescription>
          </DialogHeader>
          <form onSubmit={handleInstall} className="space-y-4">
            {/* Auth / deploy instructions */}
            {installTemplate?.auth_instructions && (
              <div className="rounded-md border bg-muted/50 p-3 text-sm">
                <p className="mb-1 font-medium">
                  {isHosted
                    ? installTemplate?.auth_type !== 'none'
                      ? t('mcpStore.authSecret')
                      : ''
                    : t('mcpStore.deployInstructions')}
                </p>
                <p className="text-muted-foreground whitespace-pre-wrap">
                  {installTemplate.auth_instructions}
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

            {/* Auth secret — shown when auth_type is bearer or api_key */}
            {installTemplate?.auth_type &&
              installTemplate.auth_type !== 'none' && (
                <div className="space-y-1.5">
                  <Label>{t('mcpStore.authSecret')}</Label>
                  <Input
                    type="password"
                    value={authSecret}
                    onChange={(e) => setAuthSecret(e.target.value)}
                    placeholder={
                      installTemplate.auth_type === 'bearer'
                        ? 'Bearer token'
                        : 'API key'
                    }
                  />
                </div>
              )}

            {/* Custom headers */}
            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <Label className="text-sm">Custom Headers</Label>
                <Button type="button" variant="ghost" size="sm" onClick={addHeader}>
                  <Plus className="mr-1 h-3 w-3" /> {t('common.add')}
                </Button>
              </div>
              {customHeaders.map(([k, v], i) => (
                <div key={i} className="flex items-center gap-2">
                  <Input
                    placeholder="Header name"
                    value={k}
                    onChange={(e) => updateHeader(i, 0, e.target.value)}
                    className="flex-1"
                  />
                  <Input
                    placeholder="Value"
                    value={v}
                    onChange={(e) => updateHeader(i, 1, e.target.value)}
                    className="flex-1"
                  />
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    onClick={() => removeHeader(i)}
                  >
                    <Trash2 className="h-4 w-4" />
                  </Button>
                </div>
              ))}
            </div>

            <DialogFooter>
              <Button type="submit" disabled={installing}>
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

function TemplateCard({
  template,
  onInstall,
  t,
}: {
  template: StoreTemplate;
  onInstall: () => void;
  t: (key: string) => string;
}) {
  return (
    <Card className="flex flex-col justify-between">
      <CardHeader className="pb-2">
        <div className="flex items-start justify-between">
          <CardTitle className="text-base">{template.name}</CardTitle>
          {template.category && (
            <Badge variant="secondary" className="text-xs">
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
          {template.description}
        </p>
        <div className="flex items-center justify-between">
          <span className="text-xs text-muted-foreground">
            {template.install_count} installs
          </span>
          {template.installed ? (
            <Badge variant="outline" className="gap-1">
              <CheckCircle2 className="h-3 w-3" />
              {t('mcpStore.installed')}
            </Badge>
          ) : (
            <Button size="sm" onClick={onInstall}>
              <Download className="mr-1 h-3 w-3" />
              {t('mcpStore.install')}
            </Button>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
