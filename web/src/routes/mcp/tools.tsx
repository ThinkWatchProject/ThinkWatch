import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Wrench, AlertCircle, Search, ChevronDown, Code2 } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from '@/components/ui/collapsible';
import { ServiceLogo } from '@/components/ui/service-logo';
import { api } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';
import { cn } from '@/lib/utils';

interface McpTool {
  id: string;
  server_id: string;
  server_name: string;
  name: string;
  namespaced_name: string;
  description: string | null;
  input_schema: Record<string, unknown> | null;
}

interface McpServer {
  id: string;
  name: string;
}

export function McpToolsPage() {
  const { t } = useTranslation();
  const [tools, setTools] = useState<McpTool[]>([]);
  const [servers, setServers] = useState<McpServer[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [filterServer, setFilterServer] = useState('');
  const [query, setQuery] = useState('');

  useEffect(() => {
    Promise.all([
      api<McpTool[]>('/api/mcp/tools'),
      api<McpServer[]>('/api/mcp/servers'),
    ])
      .then(([toolsData, serversData]) => {
        setTools(toolsData);
        setServers(serversData);
      })
      .catch((err) => setError(err instanceof Error ? err.message : 'Failed to load tools'))
      .finally(() => setLoading(false));
  }, []);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return tools.filter((tool) => {
      if (filterServer && tool.server_id !== filterServer) return false;
      if (!q) return true;
      return (
        tool.name.toLowerCase().includes(q) ||
        tool.namespaced_name.toLowerCase().includes(q) ||
        (tool.description ?? '').toLowerCase().includes(q)
      );
    });
  }, [tools, filterServer, query]);

  const grouped = useMemo(() => {
    return filtered.reduce<Record<string, McpTool[]>>((acc, tool) => {
      const key = tool.server_name;
      if (!acc[key]) acc[key] = [];
      acc[key].push(tool);
      return acc;
    }, {});
  }, [filtered]);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('mcpTools.title')}</h1>
        <p className="text-sm text-muted-foreground">{t('mcpTools.subtitle')}</p>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <div className="flex flex-wrap items-center gap-3">
        <div className="relative max-w-xs flex-1">
          <Search className="pointer-events-none absolute left-3 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            placeholder={t('mcpTools.searchPlaceholder')}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            className="pl-9"
          />
        </div>
        <Select value={filterServer || '__all__'} onValueChange={(v) => setFilterServer(v === '__all__' ? '' : v)}>
          <SelectTrigger className="w-52"><SelectValue placeholder={t('mcpTools.allServers')} /></SelectTrigger>
          <SelectContent>
            <SelectItem value="__all__">{t('mcpTools.allServers')}</SelectItem>
            {servers.map((s) => (
              <SelectItem key={s.id} value={s.id}>{s.name}</SelectItem>
            ))}
          </SelectContent>
        </Select>
        <span className="ml-auto text-xs text-muted-foreground">
          {filtered.length} {t('mcpTools.toolsFound')}
        </span>
      </div>

      {loading ? (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {[...Array(6)].map((_, i) => (
            <Card key={i}>
              <CardHeader className="pb-2"><Skeleton className="h-4 w-32" /></CardHeader>
              <CardContent><Skeleton className="h-3 w-full" /><Skeleton className="mt-2 h-3 w-2/3" /></CardContent>
            </Card>
          ))}
        </div>
      ) : filtered.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center justify-center py-12 text-center">
            <Wrench className="mb-3 h-10 w-10 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">{t('mcpTools.noTools')}</p>
            <p className="mt-1 text-xs text-muted-foreground">{t('mcpTools.noToolsHint')}</p>
          </CardContent>
        </Card>
      ) : (
        <div className="space-y-6">
          {Object.entries(grouped).map(([serverName, serverTools]) => (
            <section key={serverName} className="space-y-3">
              <header className="flex items-center gap-2.5">
                <ServiceLogo service={serverName} />
                <h2 className="text-base font-semibold">{serverName}</h2>
                <span className="rounded-full bg-muted px-2 py-0.5 font-mono text-[10px] text-muted-foreground">
                  {serverTools.length}
                </span>
              </header>
              <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                {serverTools.map((tool) => (
                  <ToolCard key={tool.id} tool={tool} noDescriptionLabel={t('mcpTools.noDescription')} />
                ))}
              </div>
            </section>
          ))}
        </div>
      )}
    </div>
  );
}

function ToolCard({ tool, noDescriptionLabel }: { tool: McpTool; noDescriptionLabel: string }) {
  const [open, setOpen] = useState(false);
  const hasSchema =
    tool.input_schema &&
    typeof tool.input_schema === 'object' &&
    Object.keys(tool.input_schema).length > 0;
  // Parse schema props for a pretty signature
  const props = useMemo(() => {
    const raw = tool.input_schema as { properties?: Record<string, { type?: string; description?: string }>; required?: string[] } | null;
    if (!raw?.properties) return [];
    const required = new Set(raw.required ?? []);
    return Object.entries(raw.properties).map(([name, spec]) => ({
      name,
      type: spec?.type ?? 'any',
      required: required.has(name),
      description: spec?.description,
    }));
  }, [tool.input_schema]);

  return (
    <Card className="card-interactive flex flex-col">
      <CardHeader className="pb-2">
        <CardTitle className="flex items-start gap-1.5 text-sm">
          <Wrench className="mt-0.5 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          <code className="truncate font-mono text-xs font-medium">{tool.name}</code>
        </CardTitle>
        <p className="truncate font-mono text-[10px] text-muted-foreground/70">{tool.namespaced_name}</p>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col gap-3 pb-3">
        <p className="line-clamp-3 text-xs leading-relaxed text-muted-foreground">
          {tool.description || noDescriptionLabel}
        </p>

        {props.length > 0 && (
          <div className="flex flex-wrap gap-1">
            {props.slice(0, 4).map((p) => (
              <span
                key={p.name}
                className={cn(
                  'rounded border px-1.5 py-0.5 font-mono text-[10px]',
                  p.required
                    ? 'border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400'
                    : 'border-border/60 bg-muted/40 text-muted-foreground',
                )}
                title={p.description}
              >
                {p.name}
                <span className="ml-1 opacity-60">:{p.type}</span>
              </span>
            ))}
            {props.length > 4 && (
              <span className="rounded border border-border/60 bg-muted/40 px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">
                +{props.length - 4}
              </span>
            )}
          </div>
        )}

        {hasSchema && (
          <Collapsible open={open} onOpenChange={setOpen}>
            <CollapsibleTrigger className="flex items-center gap-1 text-[10px] text-muted-foreground hover:text-foreground">
              <Code2 className="h-3 w-3" />
              Schema
              <ChevronDown className={cn('h-3 w-3 transition-transform', open && 'rotate-180')} />
            </CollapsibleTrigger>
            <CollapsibleContent>
              <pre className="mt-1 max-h-48 overflow-auto rounded bg-muted/60 p-2 text-[10px] leading-tight">
                {JSON.stringify(tool.input_schema, null, 2)}
              </pre>
            </CollapsibleContent>
          </Collapsible>
        )}
      </CardContent>
    </Card>
  );
}
