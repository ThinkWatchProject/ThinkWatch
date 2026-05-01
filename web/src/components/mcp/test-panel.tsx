import { useTranslation } from 'react-i18next';
import { CheckCircle2, Loader2, Lock, XCircle } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';

export interface McpTestResult {
  success: boolean;
  /** True when the upstream returned 401/403 — soft success: server is
   *  reachable, but anonymous probe wasn't allowed in. Per-user auth is
   *  validated when the user connects via /connections. */
  requires_auth?: boolean;
  message: string;
  latency_ms?: number;
  tools_count?: number;
  tools?: { name: string; description?: string }[];
}

interface McpTestPanelProps {
  testing: boolean;
  result: McpTestResult | null;
  onRetry?: () => void;
}

export function McpTestPanel({ testing, result, onRetry }: McpTestPanelProps) {
  const { t } = useTranslation();
  if (testing || !result) {
    return (
      <div className="flex items-center gap-2 rounded-md border p-4 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
        {t('mcpServers.wizard.testing')}
      </div>
    );
  }

  // Three rendering branches:
  //   1. requires_auth: soft success — server reachable but gated.
  //   2. success with tools: full success.
  //   3. failure: hard failure (network, 5xx, malformed response).
  if (result.requires_auth) {
    return (
      <Alert>
        <Lock className="h-4 w-4" />
        <AlertDescription>
          <div className="font-medium">{t('mcpServers.wizard.requiresAuthTitle')}</div>
          <div className="text-xs text-muted-foreground mt-0.5">
            {t('mcpServers.wizard.requiresAuthHint')}
            {result.latency_ms != null && ` (${result.latency_ms}ms)`}
          </div>
        </AlertDescription>
      </Alert>
    );
  }

  if (!result.success) {
    return (
      <div className="space-y-2">
        <Alert variant="destructive">
          <XCircle className="h-4 w-4" />
          <AlertDescription>
            {result.message}
            {result.latency_ms != null && ` (${result.latency_ms}ms)`}
          </AlertDescription>
        </Alert>
        {onRetry && (
          <Button type="button" variant="outline" size="sm" onClick={onRetry}>
            {t('mcpServers.wizard.retryTest')}
          </Button>
        )}
      </div>
    );
  }

  // Full success — emphasize tools_count + latency as the primary
  // signal. The raw probe message ("Connected — 3 tools available") is
  // redundant once we render the stat row, so we drop it.
  const toolsCount = result.tools_count ?? result.tools?.length ?? 0;
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-3 rounded-md border border-emerald-500/30 bg-emerald-500/10 p-3">
        <CheckCircle2 className="h-5 w-5 shrink-0 text-emerald-600 dark:text-emerald-400" />
        <div className="flex flex-1 items-baseline gap-2">
          <span className="text-2xl font-semibold tabular-nums">{toolsCount}</span>
          <span className="text-sm text-muted-foreground">
            {t('mcpServers.wizard.toolsDiscovered')}
          </span>
          {result.latency_ms != null && (
            <span className="ml-auto text-xs text-muted-foreground tabular-nums">
              {result.latency_ms}ms
            </span>
          )}
        </div>
      </div>
      {result.tools && result.tools.length > 0 && (
        <ScrollArea className="h-40 rounded-md border p-2">
          <ul className="space-y-1 text-xs">
            {result.tools.map((tool) => (
              <li key={tool.name} className="flex items-baseline gap-2">
                <code className="font-medium">{tool.name}</code>
                {tool.description && (
                  <span className="text-muted-foreground truncate">{tool.description}</span>
                )}
              </li>
            ))}
          </ul>
        </ScrollArea>
      )}
    </div>
  );
}
