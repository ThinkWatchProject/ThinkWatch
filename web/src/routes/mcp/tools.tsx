import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Wrench, AlertCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { ScrollArea } from '@/components/ui/scroll-area';
import { api } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';

interface McpTool {
  id: string;
  name: string;
  namespaced_name: string;
  description: string;
  input_schema: Record<string, unknown>;
  server_id: string;
  server_name: string;
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

  const filteredTools = filterServer
    ? tools.filter((t) => t.server_id === filterServer)
    : tools;

  const grouped = filteredTools.reduce<Record<string, McpTool[]>>((acc, tool) => {
    const key = tool.server_name || tool.server_id;
    if (!acc[key]) acc[key] = [];
    acc[key].push(tool);
    return acc;
  }, {});

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('mcpTools.title')}</h1>
        <p className="text-muted-foreground">{t('mcpTools.subtitle')}</p>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <div className="flex items-center gap-3">
        <Label>{t('mcpTools.filterByServer')}</Label>
        <Select value={filterServer || '__all__'} onValueChange={(v) => setFilterServer(v === '__all__' ? '' : v)}>
          <SelectTrigger className="w-64"><SelectValue placeholder={t('mcpTools.allServers')} /></SelectTrigger>
          <SelectContent>
            <SelectItem value="__all__">{t('mcpTools.allServers')}</SelectItem>
            {servers.map((s) => (
              <SelectItem key={s.id} value={s.id}>{s.name}</SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {loading ? (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {[...Array(6)].map((_, i) => (
            <Card key={i}>
              <CardHeader className="pb-2"><Skeleton className="h-4 w-32" /></CardHeader>
              <CardContent><Skeleton className="h-3 w-full" /><Skeleton className="h-3 w-2/3 mt-2" /></CardContent>
            </Card>
          ))}
        </div>
      ) : filteredTools.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center justify-center py-12 text-center">
            <Wrench className="h-10 w-10 text-muted-foreground mb-3" />
            <p className="text-sm text-muted-foreground">{t('mcpTools.noTools')}</p>
            <p className="text-xs text-muted-foreground mt-1">{t('mcpTools.noToolsHint')}</p>
          </CardContent>
        </Card>
      ) : (
        Object.entries(grouped).map(([serverName, serverTools]) => (
          <div key={serverName} className="space-y-3">
            <h2 className="text-lg font-medium">{serverName}</h2>
            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
              {serverTools.map((tool) => (
                <Card key={tool.id}>
                  <CardHeader className="pb-2">
                    <CardTitle className="text-sm font-medium">
                      <code>{tool.namespaced_name || tool.name}</code>
                    </CardTitle>
                  </CardHeader>
                  <CardContent className="space-y-2">
                    <p className="text-xs text-muted-foreground">{tool.description || t('mcpTools.noDescription')}</p>
                    {tool.input_schema && Object.keys(tool.input_schema).length > 0 && (
                      <div>
                        <Badge variant="outline" className="text-[10px]">{t('mcpTools.inputSchema')}</Badge>
                        <ScrollArea className="mt-1 max-h-24">
                          <pre className="rounded bg-muted p-2 text-[10px] leading-tight">
                            {JSON.stringify(tool.input_schema, null, 2)}
                          </pre>
                        </ScrollArea>
                      </div>
                    )}
                  </CardContent>
                </Card>
              ))}
            </div>
          </div>
        ))
      )}
    </div>
  );
}
