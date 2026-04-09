import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Switch } from '@/components/ui/switch';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { AlertCircle, Brain, Pencil, Plus, Trash2 } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { api, apiDelete, apiPatch, apiPost } from '@/lib/api';
import { toast } from 'sonner';

interface ModelRow {
  id: string;
  provider_id: string;
  provider_name: string;
  model_id: string;
  display_name: string;
  input_price: string | null;
  output_price: string | null;
  input_multiplier: string;
  output_multiplier: string;
  is_active: boolean;
}

interface Provider {
  id: string;
  name: string;
  display_name: string;
  provider_type: string;
}

const providerColors: Record<string, 'default' | 'secondary' | 'outline'> = {
  openai: 'default',
  anthropic: 'secondary',
  google: 'outline',
  azure_openai: 'default',
  bedrock: 'secondary',
  custom: 'outline',
};

function detectProviderType(name: string): string {
  const lower = name.toLowerCase();
  if (lower.includes('openai') || lower.includes('gpt')) return 'openai';
  if (lower.includes('anthropic') || lower.includes('claude')) return 'anthropic';
  if (lower.includes('google') || lower.includes('gemini')) return 'google';
  return 'custom';
}

interface FormState {
  provider_id: string;
  model_id: string;
  display_name: string;
  input_price: string;
  output_price: string;
  input_multiplier: string;
  output_multiplier: string;
  is_active: boolean;
}

const emptyForm: FormState = {
  provider_id: '',
  model_id: '',
  display_name: '',
  input_price: '',
  output_price: '',
  input_multiplier: '1.0',
  output_multiplier: '1.0',
  is_active: true,
};

export function ModelsPage() {
  const { t } = useTranslation();
  const [models, setModels] = useState<ModelRow[]>([]);
  const [providers, setProviders] = useState<Provider[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');

  const [dialogOpen, setDialogOpen] = useState(false);
  const [editing, setEditing] = useState<ModelRow | null>(null);
  const [form, setForm] = useState<FormState>(emptyForm);
  const [formError, setFormError] = useState('');
  const [saving, setSaving] = useState(false);
  const [deleteTargetId, setDeleteTargetId] = useState<string | null>(null);

  const fetchAll = async () => {
    setLoading(true);
    try {
      const [m, p] = await Promise.all([
        api<ModelRow[]>('/api/admin/models'),
        api<Provider[]>('/api/admin/providers'),
      ]);
      setModels(m);
      setProviders(p);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load models');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void fetchAll();
  }, []);

  const openCreate = () => {
    setEditing(null);
    setForm({ ...emptyForm, provider_id: providers[0]?.id ?? '' });
    setFormError('');
    setDialogOpen(true);
  };

  const openEdit = (m: ModelRow) => {
    setEditing(m);
    setForm({
      provider_id: m.provider_id,
      model_id: m.model_id,
      display_name: m.display_name,
      input_price: m.input_price ?? '',
      output_price: m.output_price ?? '',
      input_multiplier: m.input_multiplier,
      output_multiplier: m.output_multiplier,
      is_active: m.is_active,
    });
    setFormError('');
    setDialogOpen(true);
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    const inMult = Number(form.input_multiplier);
    const outMult = Number(form.output_multiplier);
    if (!Number.isFinite(inMult) || inMult <= 0 || !Number.isFinite(outMult) || outMult <= 0) {
      setFormError(t('models.errors.multiplierMustBePositive'));
      return;
    }
    const body = {
      ...(editing ? {} : { provider_id: form.provider_id, model_id: form.model_id }),
      display_name: form.display_name,
      input_price: form.input_price === '' ? null : form.input_price,
      output_price: form.output_price === '' ? null : form.output_price,
      input_multiplier: form.input_multiplier,
      output_multiplier: form.output_multiplier,
      is_active: form.is_active,
    };
    setSaving(true);
    try {
      if (editing) {
        await apiPatch(`/api/admin/models/${editing.id}`, body);
        toast.success(t('models.toast.updated'));
      } else {
        await apiPost('/api/admin/models', body);
        toast.success(t('models.toast.created'));
      }
      setDialogOpen(false);
      await fetchAll();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setSaving(false);
    }
  };

  const confirmDelete = async () => {
    if (!deleteTargetId) return;
    try {
      await apiDelete(`/api/admin/models/${deleteTargetId}`);
      toast.success(t('models.toast.deleted'));
      setDeleteTargetId(null);
      await fetchAll();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to delete');
    }
  };

  const groupedByProvider = useMemo(() => {
    const map = new Map<string, ModelRow[]>();
    for (const m of models) {
      if (!map.has(m.provider_name)) map.set(m.provider_name, []);
      map.get(m.provider_name)!.push(m);
    }
    return Array.from(map.entries()).sort(([a], [b]) => a.localeCompare(b));
  }, [models]);

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('models.title')}</h1>
          <p className="text-muted-foreground">{t('models.subtitle')}</p>
        </div>
        <Button onClick={openCreate} disabled={providers.length === 0}>
          <Plus className="mr-2 h-4 w-4" />
          {t('models.addModel')}
        </Button>
      </div>

      {error && (
        <Alert variant="destructive">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      {providers.length === 0 && !loading && (
        <Alert>
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{t('models.noProvidersHint')}</AlertDescription>
        </Alert>
      )}

      {loading ? (
        <div className="space-y-4">
          {[...Array(3)].map((_, i) => (
            <Skeleton key={i} className="h-24 w-full" />
          ))}
        </div>
      ) : models.length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center justify-center py-12 text-center">
            <Brain className="mb-3 h-10 w-10 text-muted-foreground" />
            <p className="text-sm text-muted-foreground">{t('models.noModels')}</p>
            <p className="mt-1 text-xs text-muted-foreground">{t('models.noModelsHint')}</p>
          </CardContent>
        </Card>
      ) : (
        groupedByProvider.map(([providerName, rows]) => {
          const ptype = detectProviderType(providerName);
          return (
            <Card key={providerName}>
              <CardHeader>
                <CardTitle className="flex items-center gap-2 text-base">
                  <span>{providerName}</span>
                  <Badge variant={providerColors[ptype]}>{ptype}</Badge>
                </CardTitle>
              </CardHeader>
              <CardContent>
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>{t('models.col.modelId')}</TableHead>
                      <TableHead>{t('models.col.displayName')}</TableHead>
                      <TableHead className="text-right">{t('models.col.inputPrice')}</TableHead>
                      <TableHead className="text-right">{t('models.col.outputPrice')}</TableHead>
                      <TableHead className="text-right">{t('models.col.inputMult')}</TableHead>
                      <TableHead className="text-right">{t('models.col.outputMult')}</TableHead>
                      <TableHead className="text-center">{t('models.col.active')}</TableHead>
                      <TableHead className="text-right">{t('common.actions')}</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {rows.map((m) => (
                      <TableRow key={m.id}>
                        <TableCell className="font-mono text-xs">{m.model_id}</TableCell>
                        <TableCell>{m.display_name}</TableCell>
                        <TableCell className="text-right font-mono text-xs">
                          {m.input_price ?? '—'}
                        </TableCell>
                        <TableCell className="text-right font-mono text-xs">
                          {m.output_price ?? '—'}
                        </TableCell>
                        <TableCell className="text-right font-mono text-xs">
                          {m.input_multiplier}
                        </TableCell>
                        <TableCell className="text-right font-mono text-xs">
                          {m.output_multiplier}
                        </TableCell>
                        <TableCell className="text-center">
                          {m.is_active ? (
                            <Badge variant="default">{t('common.yes')}</Badge>
                          ) : (
                            <Badge variant="outline">{t('common.no')}</Badge>
                          )}
                        </TableCell>
                        <TableCell className="text-right">
                          <Button variant="ghost" size="icon" onClick={() => openEdit(m)}>
                            <Pencil className="h-4 w-4" />
                          </Button>
                          <Button
                            variant="ghost"
                            size="icon"
                            onClick={() => setDeleteTargetId(m.id)}
                          >
                            <Trash2 className="h-4 w-4 text-destructive" />
                          </Button>
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </CardContent>
            </Card>
          );
        })
      )}

      <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
        <DialogContent className="sm:max-w-lg">
          <form onSubmit={submit}>
            <DialogHeader>
              <DialogTitle>
                {editing ? t('models.editTitle') : t('models.createTitle')}
              </DialogTitle>
              <DialogDescription>{t('models.formHint')}</DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-4">
              {!editing && (
                <>
                  <div className="space-y-2">
                    <Label htmlFor="provider">{t('models.field.provider')}</Label>
                    <Select
                      value={form.provider_id}
                      onValueChange={(v) => setForm({ ...form, provider_id: v })}
                    >
                      <SelectTrigger id="provider">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        {providers.map((p) => (
                          <SelectItem key={p.id} value={p.id}>
                            {p.display_name} ({p.name})
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                  <div className="space-y-2">
                    <Label htmlFor="model_id">{t('models.field.modelId')}</Label>
                    <Input
                      id="model_id"
                      value={form.model_id}
                      onChange={(e) => setForm({ ...form, model_id: e.target.value })}
                      placeholder="gpt-4o"
                      required
                    />
                  </div>
                </>
              )}
              <div className="space-y-2">
                <Label htmlFor="display_name">{t('models.field.displayName')}</Label>
                <Input
                  id="display_name"
                  value={form.display_name}
                  onChange={(e) => setForm({ ...form, display_name: e.target.value })}
                  placeholder="GPT-4o"
                  required
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="input_price">{t('models.field.inputPrice')}</Label>
                  <Input
                    id="input_price"
                    value={form.input_price}
                    onChange={(e) => setForm({ ...form, input_price: e.target.value })}
                    placeholder="0.0025"
                    inputMode="decimal"
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="output_price">{t('models.field.outputPrice')}</Label>
                  <Input
                    id="output_price"
                    value={form.output_price}
                    onChange={(e) => setForm({ ...form, output_price: e.target.value })}
                    placeholder="0.01"
                    inputMode="decimal"
                  />
                </div>
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div className="space-y-2">
                  <Label htmlFor="input_multiplier">{t('models.field.inputMult')}</Label>
                  <Input
                    id="input_multiplier"
                    value={form.input_multiplier}
                    onChange={(e) => setForm({ ...form, input_multiplier: e.target.value })}
                    inputMode="decimal"
                    required
                  />
                </div>
                <div className="space-y-2">
                  <Label htmlFor="output_multiplier">{t('models.field.outputMult')}</Label>
                  <Input
                    id="output_multiplier"
                    value={form.output_multiplier}
                    onChange={(e) => setForm({ ...form, output_multiplier: e.target.value })}
                    inputMode="decimal"
                    required
                  />
                </div>
              </div>
              <p className="text-xs text-muted-foreground">{t('models.multiplierHint')}</p>
              <div className="flex items-center gap-2">
                <Switch
                  id="is_active"
                  checked={form.is_active}
                  onCheckedChange={(v) => setForm({ ...form, is_active: v })}
                />
                <Label htmlFor="is_active">{t('models.field.active')}</Label>
              </div>
              {formError && (
                <Alert variant="destructive">
                  <AlertCircle className="h-4 w-4" />
                  <AlertDescription>{formError}</AlertDescription>
                </Alert>
              )}
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setDialogOpen(false)}>
                {t('common.cancel')}
              </Button>
              <Button type="submit" disabled={saving}>
                {saving ? t('common.saving') : t('common.save')}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={deleteTargetId !== null}
        onOpenChange={(o) => !o && setDeleteTargetId(null)}
        title={t('models.deleteTitle')}
        description={t('models.deleteConfirm')}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmDelete}
      />
    </div>
  );
}
