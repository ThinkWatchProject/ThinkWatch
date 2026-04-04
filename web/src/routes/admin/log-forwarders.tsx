import { useEffect, useState, useCallback } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Separator } from '@/components/ui/separator';
import { ConfirmDialog } from '@/components/confirm-dialog';
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
  log_types: string[];
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
  { value: 'syslog', label: 'Syslog' },
  { value: 'kafka', label: 'Kafka (REST Proxy)' },
  { value: 'webhook', label: 'Webhook (HTTP)' },
];

function typeLabel(type_: string): string {
  if (type_ === 'udp_syslog') return 'Syslog (UDP)';
  if (type_ === 'tcp_syslog') return 'Syslog (TCP)';
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
  const [error, setError] = useState('');
  const [dialogOpen, setDialogOpen] = useState(false);
  const [testing, setTesting] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<TestResult | null>(null);

  // Form state
  const [formName, setFormName] = useState('');
  const [formType, setFormType] = useState('syslog');
  const [formSyslogProto, setFormSyslogProto] = useState<'udp' | 'tcp'>('udp');
  const [formAddress, setFormAddress] = useState('');
  const [formFacility, setFormFacility] = useState('16');
  const [formBrokerUrl, setFormBrokerUrl] = useState('');
  const [formTopic, setFormTopic] = useState('');
  const [formWebhookUrl, setFormWebhookUrl] = useState('');
  const [formAuthHeader, setFormAuthHeader] = useState('');
  const [formLogTypes, setFormLogTypes] = useState<Set<string>>(new Set(['audit']));
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
  const [editLogTypes, setEditLogTypes] = useState<Set<string>>(new Set());
  const [saving, setSaving] = useState(false);

  // Delete confirmation state
  const [deleteDialogOpen, setDeleteDialogOpen] = useState(false);
  const [deleteTargetId, setDeleteTargetId] = useState<string | null>(null);

  const loadForwarders = useCallback(async () => {
    try {
      const data = await api<LogForwarder[]>('/api/admin/log-forwarders');
      setForwarders(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadForwarders();
  }, [loadForwarders]);

  const resetForm = () => {
    setFormName('');
    setFormType('syslog');
    setFormSyslogProto('udp');
    setFormAddress('');
    setFormFacility('16');
    setFormBrokerUrl('');
    setFormTopic('');
    setFormWebhookUrl('');
    setFormAuthHeader('');
    setFormLogTypes(new Set(['audit']));
  };

  const buildConfig = (): Record<string, string> => {
    switch (formType) {
      case 'syslog':
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
        forwarder_type: formType === 'syslog' ? `${formSyslogProto}_syslog` : formType,
        config: buildConfig(),
        log_types: Array.from(formLogTypes),
      });
      setDialogOpen(false);
      resetForm();
      loadForwarders();
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
    } finally {
      setCreating(false);
    }
  };

  const handleToggle = async (id: string) => {
    try {
      await apiPost(`/api/admin/log-forwarders/${id}/toggle`, {});
      loadForwarders();
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
    }
  };

  const handleDelete = async (id: string) => {
    try {
      await apiDelete(`/api/admin/log-forwarders/${id}`);
      setDeleteDialogOpen(false);
      setDeleteTargetId(null);
      loadForwarders();
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
    }
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
    try {
      await apiPost(`/api/admin/log-forwarders/${id}/reset-stats`, {});
      loadForwarders();
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
    }
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
    setEditLogTypes(new Set(f.log_types || ['audit']));
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
        log_types: Array.from(editLogTypes),
      });
      setEditDialogOpen(false);
      setEditForwarder(null);
      loadForwarders();
    } catch (err) {
      setError(err instanceof Error ? err.message : t('common.error'));
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
          <DialogTrigger
            render={
              <Button onClick={() => { resetForm(); setDialogOpen(true); }}>
                <Plus className="mr-2 h-4 w-4" />
                {t('logForwarders.addForwarder')}
              </Button>
            }
          />
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
                <div className="flex gap-2 mt-1">
                  {FORWARDER_TYPES.map((ft) => (
                    <Button
                      key={ft.value}
                      type="button"
                      variant={formType === ft.value ? 'default' : 'outline'}
                      size="sm"
                      onClick={() => setFormType(ft.value)}
                    >
                      {ft.label}
                    </Button>
                  ))}
                </div>
              </div>

              {formType === 'syslog' && (
                <>
                  <div>
                    <Label>传输协议</Label>
                    <div className="flex gap-4 mt-1">
                      {(['udp', 'tcp'] as const).map((proto) => (
                        <label key={proto} className="flex items-center gap-1.5 text-sm cursor-pointer">
                          <input
                            type="radio"
                            name="syslog-proto"
                            checked={formSyslogProto === proto}
                            onChange={() => setFormSyslogProto(proto)}
                          />
                          {proto.toUpperCase()}
                        </label>
                      ))}
                    </div>
                  </div>
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

              <div>
                <Label>{t('logForwarders.logTypes')}</Label>
                <div className="flex flex-wrap gap-2 mt-1">
                  {['audit', 'gateway', 'mcp', 'platform'].map((lt) => (
                    <label key={lt} className="flex items-center gap-1.5 text-sm cursor-pointer">
                      <input
                        type="checkbox"
                        checked={formLogTypes.has(lt)}
                        onChange={() => {
                          const next = new Set(formLogTypes);
                          if (next.has(lt)) next.delete(lt); else next.add(lt);
                          setFormLogTypes(next);
                        }}
                        className="rounded"
                      />
                      {lt}
                    </label>
                  ))}
                </div>
              </div>
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

      {/* Error banner */}
      {error && (
        <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive flex items-center justify-between">
          <span>{error}</span>
          <Button variant="ghost" size="sm" onClick={() => setError('')}>
            {t('common.done')}
          </Button>
        </div>
      )}

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
                          onClick={() => { setDeleteTargetId(f.id); setDeleteDialogOpen(true); }}
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
                  <Label>传输协议</Label>
                  <p className="text-sm text-muted-foreground mt-1">{editForwarder.forwarder_type === 'udp_syslog' ? 'UDP' : 'TCP'}</p>
                </div>
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

            <div>
              <Label>{t('logForwarders.logTypes')}</Label>
              <div className="flex flex-wrap gap-2 mt-1">
                {['audit', 'gateway', 'mcp', 'platform'].map((lt) => (
                  <label key={lt} className="flex items-center gap-1.5 text-sm cursor-pointer">
                    <input
                      type="checkbox"
                      checked={editLogTypes.has(lt)}
                      onChange={() => {
                        const next = new Set(editLogTypes);
                        if (next.has(lt)) next.delete(lt); else next.add(lt);
                        setEditLogTypes(next);
                      }}
                      className="rounded"
                    />
                    {lt}
                  </label>
                ))}
              </div>
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setEditDialogOpen(false)}>{t('common.cancel')}</Button>
            <Button onClick={handleEdit} disabled={saving || !editName}>
              {saving ? t('common.loading') : t('common.save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={deleteDialogOpen}
        onOpenChange={(open) => { setDeleteDialogOpen(open); if (!open) setDeleteTargetId(null); }}
        title={t('common.delete')}
        description={t('logForwarders.deleteConfirm')}
        variant="destructive"
        confirmLabel={t('common.delete')}
        onConfirm={() => { if (deleteTargetId) handleDelete(deleteTargetId); }}
      />
    </div>
  );
}
