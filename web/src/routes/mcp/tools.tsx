import { Fragment, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Wrench, AlertCircle, Search, ChevronRight, ChevronDown } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { ServiceLogo } from '@/components/ui/service-logo';
import { Button } from '@/components/ui/button';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Skeleton } from '@/components/ui/skeleton';
import { api } from '@/lib/api';
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
  const [expanded, setExpanded] = useState<string | null>(null);

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

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="mb-4">
        <h1 className="text-2xl font-semibold tracking-tight">{t('mcpTools.title')}</h1>
        <p className="text-muted-foreground">{t('mcpTools.subtitle')}</p>
      </div>

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <div className="mb-4 flex flex-wrap items-center gap-2">
        <div className="relative w-full max-w-sm">
          <Search className="pointer-events-none absolute left-2 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
          <Input
            placeholder={t('mcpTools.searchPlaceholder')}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            className="pl-8"
          />
        </div>
        <Select
          value={filterServer || '__all__'}
          onValueChange={(v) => setFilterServer(v === '__all__' ? '' : v)}
        >
          <SelectTrigger className="w-52">
            <SelectValue placeholder={t('mcpTools.allServers')} />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="__all__">{t('mcpTools.allServers')}</SelectItem>
            {servers.map((s) => (
              <SelectItem key={s.id} value={s.id}>
                {s.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <span className="ml-auto text-xs text-muted-foreground">
          {filtered.length} {t('mcpTools.toolsFound')}
        </span>
      </div>

      <Card className="flex flex-col min-h-0 flex-1 py-0 gap-0">
        <CardContent className="p-0 overflow-auto flex-1 [&>[data-slot=table-container]]:overflow-visible">
          {loading ? (
            <div className="space-y-3 p-4">
              {[...Array(5)].map((_, i) => (
                <div key={i} className="flex items-center gap-4">
                  <Skeleton className="h-4 w-40" />
                  <Skeleton className="h-4 w-24" />
                  <Skeleton className="h-4 w-48" />
                  <Skeleton className="h-4 w-64" />
                </div>
              ))}
            </div>
          ) : filtered.length === 0 ? (
            <div className="flex h-full flex-col items-center justify-center py-12 text-center">
              <Wrench className="h-10 w-10 text-muted-foreground mb-3" />
              <p className="text-sm text-muted-foreground">{t('mcpTools.noTools')}</p>
              <p className="mt-1 text-xs text-muted-foreground">
                {t('mcpTools.noToolsHint')}
              </p>
            </div>
          ) : (
            <Table>
              <TableHeader className="sticky top-0 z-10 bg-card [&_tr]:border-b shadow-[inset_0_-1px_0_var(--border)]">
                <TableRow>
                  <TableHead className="w-8" />
                  <TableHead>{t('mcpTools.col.tool')}</TableHead>
                  <TableHead>{t('mcpTools.col.server')}</TableHead>
                  <TableHead>{t('mcpTools.col.args')}</TableHead>
                  <TableHead>{t('mcpTools.col.description')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {filtered.map((tool) => {
                  const isOpen = expanded === tool.id;
                  const hasSchema =
                    tool.input_schema &&
                    typeof tool.input_schema === 'object' &&
                    Object.keys(tool.input_schema).length > 0;
                  return (
                    <Fragment key={tool.id}>
                      <TableRow>
                        <TableCell className="w-8">
                          {hasSchema ? (
                            <Button
                              variant="ghost"
                              size="icon"
                              className="h-6 w-6"
                              onClick={() => setExpanded(isOpen ? null : tool.id)}
                              aria-label="Expand"
                            >
                              {isOpen ? (
                                <ChevronDown className="h-3 w-3" />
                              ) : (
                                <ChevronRight className="h-3 w-3" />
                              )}
                            </Button>
                          ) : null}
                        </TableCell>
                        <TableCell className="align-top">
                          <div className="flex flex-col">
                            <code className="font-mono text-xs font-medium">
                              {tool.name}
                            </code>
                            <span
                              className="truncate font-mono text-[10px] text-muted-foreground/70"
                              title={tool.namespaced_name}
                            >
                              {tool.namespaced_name}
                            </span>
                          </div>
                        </TableCell>
                        <TableCell className="align-top">
                          <div className="flex items-center gap-1.5">
                            <ServiceLogo service={tool.server_name} className="size-4" />
                            <span className="text-xs">{tool.server_name}</span>
                          </div>
                        </TableCell>
                        <TableCell className="align-top">
                          <ArgsCell schema={tool.input_schema} />
                        </TableCell>
                        <TableCell className="align-top max-w-[32rem]">
                          <p
                            className="line-clamp-2 text-xs text-muted-foreground"
                            title={tool.description ?? undefined}
                          >
                            {tool.description || t('mcpTools.noDescription')}
                          </p>
                        </TableCell>
                      </TableRow>
                      {isOpen && hasSchema && (
                        <TableRow>
                          <TableCell colSpan={5} className="bg-muted/30 p-0">
                            <pre className="max-h-72 overflow-auto p-3 font-mono text-[11px] leading-tight">
                              {JSON.stringify(tool.input_schema, null, 2)}
                            </pre>
                          </TableCell>
                        </TableRow>
                      )}
                    </Fragment>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

/// Extracts up to 4 param chips (required → amber, optional → muted)
/// from an MCP JSON schema. Full schema is available by expanding the
/// row.
function ArgsCell({ schema }: { schema: Record<string, unknown> | null }) {
  const props = useMemo(() => {
    const raw = schema as
      | {
          properties?: Record<string, { type?: string; description?: string }>;
          required?: string[];
        }
      | null;
    if (!raw?.properties) return [];
    const required = new Set(raw.required ?? []);
    return Object.entries(raw.properties).map(([name, spec]) => ({
      name,
      type: spec?.type ?? 'any',
      required: required.has(name),
      description: spec?.description,
    }));
  }, [schema]);

  if (props.length === 0) {
    return <span className="text-[10px] italic text-muted-foreground">—</span>;
  }
  // Single-line chip strip with horizontal overflow — keeps the row
  // one line tall regardless of param count. Tools with 10+ params
  // previously stretched a row to 4-5 lines and blew the table
  // vertical budget.
  return (
    <div
      className="max-w-[22rem] overflow-x-auto whitespace-nowrap"
      // Hide the scrollbar chrome; the overflow is still scrollable
      // via trackpad / shift-wheel and exposes its scrollable state
      // via cursor. A visible bar adds noise for what is usually a
      // 1-param row.
      style={{ scrollbarWidth: 'none' }}
    >
      <div className="inline-flex gap-1">
        {props.map((p) => (
          <span
            key={p.name}
            className={cn(
              'shrink-0 rounded border px-1.5 py-0.5 font-mono text-[10px]',
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
      </div>
    </div>
  );
}
