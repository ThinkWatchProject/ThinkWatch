import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import {
  Table,
  TableBody,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { FileText } from 'lucide-react';

export function McpLogsPage() {
  const { t } = useTranslation();

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('mcpLogs.title')}</h1>
          <p className="text-muted-foreground">{t('mcpLogs.subtitle')}</p>
        </div>
        <Badge variant="secondary">{t('mcpLogs.comingSoon')}</Badge>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('mcpLogs.allCalls')}</CardTitle>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t('mcpLogs.timestamp')}</TableHead>
                <TableHead>{t('mcpLogs.tool')}</TableHead>
                <TableHead>{t('mcpLogs.server')}</TableHead>
                <TableHead>{t('mcpLogs.user')}</TableHead>
                <TableHead>{t('mcpLogs.duration')}</TableHead>
                <TableHead>{t('mcpLogs.status')}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody />
          </Table>
          <div className="flex flex-col items-center justify-center py-12 text-center">
            <FileText className="h-10 w-10 text-muted-foreground mb-3" />
            <p className="text-sm text-muted-foreground">{t('mcpLogs.placeholder')}</p>
            <p className="text-xs text-muted-foreground mt-1">{t('mcpLogs.placeholderHint')}</p>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
