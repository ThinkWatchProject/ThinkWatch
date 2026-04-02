import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Separator } from '@/components/ui/separator';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import {
  Plus,
  Pause,
  Play,
  Trash2,
  TestTube,
  RotateCcw,
  Send,
  AlertCircle,
  Pencil,
} from 'lucide-react';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';

interface LogForwarder {
  id: string;
  name: string;
  forwarder_type: string;
  config: Record<string, string>;
  enabled: boolean;
  sent_count: number;
  error_count: number;
  last_sent_at: string | null;
  last_error: string | null;
  created_at: string;
  updated_at: string;
}

interface TestResult {
  success: boolean;
  message: string;
}

const FORWARDER_TYPES = [
  { value: 'udp_syslog', label: 'UDP Syslog' },
  { value: 'tcp_syslog', label: 'TCP Syslog' },
  { value: 'kafka', label: 'Kafka (REST Proxy)' },
  { value: 'webhook', label: 'Webhook (HTTP)' },
];

function typeLabel(type_: string): string {
  return FORWARDER_TYPES.find((t) => t.value === type_)?.label ?? type_;
}

function formatNumber(n: number): string {
  return n.toLocaleString();
}

function formatTime(ts: string | null): string {
  if (!ts) return '—';
  return new Date(ts).toLocaleString();
}

export function LogForwardersPage() {
  const { t } = useTranslation();
  const [forwarders, setForwarders] = useState<LogForwarder[]>([]);
  const [loading, setLoading] = useState(true);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [testing, setTesting] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  // Form state
  const [formName, setFormName] = useState('');
  const [formType, setFormType] = useState('udp_syslog');
  const [formAddress, setFormAddress] = useState('');
  const [formFacility, setFormFacility] = useState('16');
  const [formBrokerUrl, setFormBrokerUrl] = useState('');
  const [formTopic, setFormTopic] = useState('');
  const [formWebhookUrl, setFormWebhookUrl] = useState('');
  const [formAuthHeader, setFormAuthHeader] = useState('');
  const [creating, setCreating] = useState(false);

  // Edit state
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [editForwarder, setEditForwarder] = useState<LogForwarder | null>(null);
  const [editName, setEditName] = useState('');
  const [editAddress, setEditAddress] = useState('');
  const [editFacility, setEditFacility] = useState('16');
  const [editBrokerUrl, setEditBrokerUrl] = useState('');
  const [editTopic, setEditTopic] = useState('');
  const [editWebhookUrl, setEditWebhookUrl] = useState('');
  const [editAuthHeader, setEditAuthHeader] = useState('');
  const [saving, setSaving] = useState(false);

  const loadForwarders = useCallback(async () => {
    try {
      const data = await api<LogForwarder[]>('/api/admin/log-forwarders');
      setForwarders(data);
    } catch {
      // ignore
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadForwarders();
  }, [loadForwarders]);

  const resetForm = () => {
    setFormName('');
    setFormType('udp_syslog');
    setFormAddress('');
    setFormFacility('16');
    setFormBrokerUrl('');
    setFormTopic('');
    setFormWebhookUrl('');
    setFormAuthHeader('');
  };

  const buildConfig = (): Record<string, string> => {
    switch (formType) {
      case 'udp_syslog':
      case 'tcp_syslog':
        return { address: formAddress, facility: formFacility };
      case 'kafka':
        return { broker_url: formBrokerUrl, topic: formTopic };
      case 'webhook': {
        const cfg: Record<string, string> = { url: formWebhookUrl };
        if (formAuthHeader) cfg.auth_header = formAuthHeader;
        return cfg;
      }
      default:
        return {};
    }
  };

  const handleCreate = async () => {
    setCreating(true);
    try {
      await apiPost('/api/admin/log-forwarders', {
        name: formName,
        forwarder_type: formType,
        config: buildConfig(),
      });
      setDialogOpen(false);
      resetForm();
      loadForwarders();
    } catch {
      // ignore
    } finally {
      setCreating(false);
    }
  };

  const handleToggle = async (id: string) => {
    await apiPost(`/api/admin/log-forwarders/${id}/toggle`, {});
    loadForwarders();
  };

  const handleDelete = async (id: string) => {
    if (!confirm(t('logForwarders.deleteConfirm'))) return;
    await apiDelete(`/api/admin/log-forwarders/${id}`);
    loadForwarders();
  };

  const handleTest = async (id: string) => {
    setTesting(id);
    setTestResult(null);
    try {
      const result = await apiPost<TestResult>(`/api/admin/log-forwarders/${id}/test`, {});
      setTestResult(result);
    } catch {
      setTestResult({ success: false, message: 'Request failed' });
    } finally {
      setTesting(null);
    }
  };

  const handleResetStats = async (id: string) => {
    await apiPost(`/api/admin/log-forwarders/${id}/reset-stats`, {});
    loadForwarders();
  };

  const openEditDialog = (f: LogForwarder) => {
    setEditForwarder(f);
    setEditName(f.name);
    setEditAddress(f.config.address || '');
    setEditFacility(f.config.facility || '16');
    setEditBrokerUrl(f.config.broker_url || '');
    setEditTopic(f.config.topic || '');
    setEditWebhookUrl(f.config.url || '');
    setEditAuthHeader(f.config.auth_header || '');
    setEditDialogOpen(true);
  };

  const buildEditConfig = (): Record<string, string> => {
    if (!editForwarder) return {};
    switch (editForwarder.forwarder_type) {
      case 'udp_syslog':
      case 'tcp_syslog':
        return { address: editAddress, facility: editFacility };
      case 'kafka':
        return { broker_url: editBrokerUrl, topic: editTopic };
      case 'webhook': {
        const cfg: Record<string, string> = { url: editWebhookUrl };
        if (editAuthHeader) cfg.auth_header = editAuthHeader;
        return cfg;
      }
      default:
        return {};
    }
  };

  const handleEdit = async () => {
    if (!editForwarder) return;
    setSaving(true);
    try {
      await apiPatch(`/api/admin/log-forwarders/${editForwarder.id}`, {
        name: editName,
        config: buildEditConfig(),
      });
      setEditDialogOpen(false);
      setEditForwarder(null);
      loadForwarders();
    } catch {
      // ignore
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('logForwarders.title')}</h1>
          <p className="text-muted-foreground">{t('logForwarders.subtitle')}</p>
        </div>
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger asChild>
            <Button onClick={() => { resetForm(); setDialogOpen(true); }}>
              <Plus className="mr-2 h-4 w-4" />
              {t('logForwarders.addForwarder')}
            </Button>
          </DialogTrigger>
          <DialogContent className="max-w-md">
            <DialogHeader>
              <DialogTitle>{t('logForwarders.addForwarder')}</DialogTitle>
              <DialogDescription>{t('logForwarders.dialogDescription')}</DialogDescription>
            </DialogHeader>
            <div className="space-y-4">
              <div>
                <Label>{t('common.name')}</Label>
                <Input value={formName} onChange={(e) => setFormName(e.target.value)} placeholder="Production SIEM" />
              </div>
              <div>
                <Label>{t('logForwarders.type')}</Label>
                <Select value={formType} onValueChange={setFormType}>
                  <SelectTrigger><SelectValue /></SelectTrigger>
                  <SelectContent>
                    {FORWARDER_TYPES.map((ft) => (
                      <SelectItem key={ft.value} value={ft.value}>{ft.label}</SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>

              {(formType === 'udp_syslog' || formType === 'tcp_syslog') && (
                <>
                  <div>
                    <Label>{t('logForwarders.address')}</Label>
                    <Input value={formAddress} onChange={(e) => setFormAddress(e.target.value)} placeholder="127.0.0.1:514" />
                  </div>
                  <div>
                    <Label>{t('logForwarders.facility')}</Label>
                    <Input type="number" value={formFacility} onChange={(e) => setFormFacility(e.target.value)} placeholder="16" />
                  </div>
                </>
              )}

              {formType === 'kafka' && (
                <>
                  <div>
                    <Label>{t('logForwarders.brokerUrl')}</Label>
                    <Input value={formBrokerUrl} onChange={(e) => setFormBrokerUrl(e.target.value)} placeholder="http://kafka-rest:8082" />
                  </div>
                  <div>
                    <Label>{t('logForwarders.topic')}</Label>
                    <Input value={formTopic} onChange={(e) => setFormTopic(e.target.value)} placeholder="audit-logs" />
                  </div>
                </>
              )}

              {formType === 'webhook' && (
                <>
                  <div>
                    <Label>{t('logForwarders.webhookUrl')}</Label>
                    <Input value={formWebhookUrl} onChange={(e) => setFormWebhookUrl(e.target.value)} placeholder="https://siem.example.com/ingest" />
                  </div>
                  <div>
                    <Label>{t('logForwarders.authHeader')}</Label>
                    <Input value={formAuthHeader} onChange={(e) => setFormAuthHeader(e.target.value)} placeholder="Bearer xxx (optional)" />
                  </div>
                </>
              )}
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setDialogOpen(false)}>{t('common.cancel')}</Button>
              <Button onClick={handleCreate} disabled={creating || !formName}>
                {creating ? t('common.loading') : t('common.create')}
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      </div>

      {/* Test result toast */}
      {testResult && (
        <Card className={testResult.success ? 'border-green-500/50 bg-green-500/5' : 'border-red-500/50 bg-red-500/5'}>
          <CardContent className="flex items-center gap-3 py-3">
            {testResult.success ? (
              <Send className="h-4 w-4 text-green-600" />
            ) : (
              <AlertCircle className="h-4 w-4 text-red-600" />
            )}
            <span className="text-sm">{testResult.message}</span>
            <Button variant="ghost" size="sm" className="ml-auto" onClick={() => setTestResult(null)}>
              {t('common.done')}
            </Button>
          </CardContent>
        </Card>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('logForwarders.allForwarders')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <p className="text-sm text-muted-foreground">{t('common.loading')}</p>
          ) : forwarders.length === 0 ? (
            <div className="py-8 text-center">
              <p className="text-sm text-muted-foreground">{t('logForwarders.noForwarders')}</p>
              <p className="mt-1 text-xs text-muted-foreground">{t('logForwarders.noForwardersHint')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('common.name')}</TableHead>
                  <TableHead>{t('logForwarders.type')}</TableHead>
                  <TableHead>{t('logForwarders.destination')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead className="text-right">{t('logForwarders.sent')}</TableHead>
                  <TableHead className="text-right">{t('logForwarders.errors')}</TableHead>
                  <TableHead>{t('logForwarders.lastSent')}</TableHead>
                  <TableHead>{t('common.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {forwarders.map((f) => (
                  <TableRow key={f.id}>
                    <TableCell className="font-medium">{f.name}</TableCell>
                    <TableCell>
                      <Badge variant="outline">{typeLabel(f.forwarder_type)}</Badge>
                    </TableCell>
                    <TableCell className="font-mono text-xs text-muted-foreground max-w-48 truncate">
                      {f.config.address || f.config.broker_url || f.config.url || '—'}
                    </TableCell>
                    <TableCell>
                      <Badge variant={f.enabled ? 'default' : 'secondary'}>
                        {f.enabled ? t('logForwarders.running') : t('logForwarders.paused')}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-right tabular-nums">{formatNumber(f.sent_count)}</TableCell>
                    <TableCell className="text-right tabular-nums">
                      {f.error_count > 0 ? (
                        <span className="text-red-600" title={f.last_error ?? undefined}>
                          {formatNumber(f.error_count)}
                        </span>
                      ) : (
                        '0'
                      )}
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">{formatTime(f.last_sent_at)}</TableCell>
                    <TableCell>
                      <div className="flex items-center gap-1">
                        <Button
                          variant="ghost"
                          size="icon"
                          title={t('common.edit')}
                          onClick={() => openEditDialog(f)}
                        >
                          <Pencil className="h-4 w-4" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon"
                          title={f.enabled ? t('logForwarders.pause') : t('logForwarders.resume')}
                          onClick={() => handleToggle(f.id)}
                        >
                          {f.enabled ? <Pause className="h-4 w-4" /> : <Play className="h-4 w-4" />}
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon"
                          title={t('logForwarders.test')}
                          disabled={testing === f.id}
                          onClick={() => handleTest(f.id)}
                        >
                          <TestTube className="h-4 w-4" />
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon"
                          title={t('logForwarders.resetStats')}
                          onClick={() => handleResetStats(f.id)}
                        >
                          <RotateCcw className="h-4 w-4" />
                        </Button>
                        <Separator orientation="vertical" className="mx-1 h-4" />
                        <Button
                          variant="ghost"
                          size="icon"
                          className="text-destructive hover:text-destructive"
                          title={t('common.delete')}
                          onClick={() => handleDelete(f.id)}
                        >
                          <Trash2 className="h-4 w-4" />
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>

      {/* Edit Dialog */}
      <Dialog open={editDialogOpen} onOpenChange={setEditDialogOpen}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t('logForwarders.editForwarder')}</DialogTitle>
            <DialogDescription>{t('logForwarders.editDescription')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4">
            <div>
              <Label>{t('common.name')}</Label>
              <Input value={editName} onChange={(e) => setEditName(e.target.value)} />
            </div>

            {editForwarder && (editForwarder.forwarder_type === 'udp_syslog' || editForwarder.forwarder_type === 'tcp_syslog') && (
              <>
                <div>
                  <Label>{t('logForwarders.address')}</Label>
                  <Input value={editAddress} onChange={(e) => setEditAddress(e.target.value)} placeholder="127.0.0.1:514" />
                </div>
                <div>
                  <Label>{t('logForwarders.facility')}</Label>
                  <Input type="number" value={editFacility} onChange={(e) => setEditFacility(e.target.value)} />
                </div>
              </>
            )}

            {editForwarder?.forwarder_type === 'kafka' && (
              <>
                <div>
                  <Label>{t('logForwarders.brokerUrl')}</Label>
                  <Input value={editBrokerUrl} onChange={(e) => setEditBrokerUrl(e.target.value)} />
                </div>
                <div>
                  <Label>{t('logForwarders.topic')}</Label>
                  <Input value={editTopic} onChange={(e) => setEditTopic(e.target.value)} />
                </div>
              </>
            )}

            {editForwarder?.forwarder_type === 'webhook' && (
              <>
                <div>
                  <Label>{t('logForwarders.webhookUrl')}</Label>
                  <Input value={editWebhookUrl} onChange={(e) => setEditWebhookUrl(e.target.value)} />
                </div>
                <div>
                  <Label>{t('logForwarders.authHeader')}</Label>
                  <Input value={editAuthHeader} onChange={(e) => setEditAuthHeader(e.target.value)} />
                </div>
              </>
            )}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setEditDialogOpen(false)}>{t('common.cancel')}</Button>
            <Button onClick={handleEdit} disabled={saving || !editName}>
              {saving ? t('common.loading') : t('common.save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
