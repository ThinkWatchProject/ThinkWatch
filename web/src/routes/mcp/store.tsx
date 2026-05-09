import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from '@tanstack/react-router';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import {
  Search,
  Download,
  CheckCircle2,
  Loader2,
  Star,
  RefreshCw,
  Globe,
  Lock,
  KeyRound,
} from 'lucide-react';
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
  oauth_issuer: string | null;
  oauth_token_endpoint: string | null;
  oauth_userinfo_endpoint: string | null;
  auth_shape: 'anonymous' | 'oauth' | 'static';
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

const CATEGORIES = [
  'developer',
  'database',
  'communication',
  'cloud',
  'utility',
  'knowledge',
  'productivity',
] as const;

/** Pick the right language from a bilingual string stored as "en\n---\nzh". */
function i18nText(text: string | null | undefined, lang: string): string {
  if (!text) return '';
  const parts = text.split('\n---\n');
  if (parts.length < 2) return text;
  return lang.startsWith('zh') ? parts[1] || parts[0] : parts[0];
}

export function McpStorePage() {
  const { t, i18n } = useTranslation();
  const [templates, setTemplates] = useState<StoreTemplate[]>([]);
  const [categories, setCategories] = useState<CategoryCount[]>([]);
  const [loading, setLoading] = useState(true);
  const [searchQuery, setSearchQuery] = useState('');
  const [activeCategory, setActiveCategory] = useState<string | null>(null);
  const [syncing, setSyncing] = useState(false);

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
            {syncing ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <RefreshCw className="h-4 w-4" />
            )}
            {syncing ? t('mcpStore.syncing') : t('mcpStore.syncRegistry')}
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
                  t={t}
                  lang={i18n.language}
                />
              ))}
            </div>
          )}
        </>
      )}
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
  if (template.auth_shape === 'oauth') {
    return (
      <Badge variant="outline" className="gap-1">
        <Lock className="h-3 w-3" /> OAuth
      </Badge>
    );
  }
  if (template.auth_shape === 'static') {
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
  t,
  lang,
}: {
  template: StoreTemplate;
  lang: string;
  t: (key: string) => string;
}) {
  // Install handed off to the registration wizard at
  // /mcp/servers/new?template={slug}. The wizard fetches the template,
  // prefills Step 1 (URL + auth shape + OAuth + header defaults), and
  // ships `template_slug` back to POST /api/mcp/servers so the
  // mcp_store_installs audit row + install_count bump happen in the
  // same TX as the server INSERT. No more dedicated install dialog.
  const canInstall = hasPermission('mcp_servers:create');
  const buttonContent = template.installed ? (
    <>
      <CheckCircle2 className="mr-1 h-3 w-3 text-emerald-500" />
      {t('mcpStore.installAgain')}
    </>
  ) : (
    <>
      <Download className="mr-1 h-3 w-3" />
      {t('mcpStore.install')}
    </>
  );
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
          {canInstall ? (
            <Button
              asChild
              size="sm"
              variant={template.installed ? 'outline' : 'default'}
              title={template.installed ? t('mcpStore.installAgainHint') : undefined}
            >
              <Link to="/mcp/servers/new" search={{ template: template.slug }}>
                {buttonContent}
              </Link>
            </Button>
          ) : (
            <Button
              size="sm"
              variant={template.installed ? 'outline' : 'default'}
              disabled
            >
              {buttonContent}
            </Button>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
