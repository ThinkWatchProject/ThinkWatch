import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
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
import { Plus, Trash2, Pencil } from 'lucide-react';
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';

interface Provider {
  id: string;
  name: string;
  display_name: string;
  provider_type: string;
  base_url: string;
  is_active: boolean;
  created_at: string;
}

const providerTypeColors: Record<string, 'default' | 'secondary' | 'outline'> = {
  openai: 'default',
  anthropic: 'secondary',
  google: 'outline',
  custom: 'outline',
};

export function ProvidersPage() {
  const { t } = useTranslation();
  const [providers, setProviders] = useState<Provider[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [dialogOpen, setDialogOpen] = useState(false);
  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);

  const [name, setName] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [providerType, setProviderType] = useState('openai');
  const [baseUrl, setBaseUrl] = useState('');
  const [apiKey, setApiKey] = useState('');

  // Edit state
  const [editDialogOpen, setEditDialogOpen] = useState(false);
  const [editProvider, setEditProvider] = useState<Provider | null>(null);
  const [editDisplayName, setEditDisplayName] = useState('');
  const [editBaseUrl, setEditBaseUrl] = useState('');
  const [editApiKey, setEditApiKey] = useState('');
  const [editSaving, setEditSaving] = useState(false);
  const [editError, setEditError] = useState('');

  const fetchProviders = async () => {
    try {
      const data = await api<Provider[]>('/api/admin/providers');
      setProviders(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load providers');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { fetchProviders(); }, []);

  const resetForm = () => {
    setName('');
    setDisplayName('');
    setProviderType('openai');
    setBaseUrl('');
    setApiKey('');
    setFormError('');
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    setSubmitting(true);
    try {
      await apiPost('/api/admin/providers', {
        name,
        display_name: displayName,
        provider_type: providerType,
        base_url: baseUrl,
        api_key: apiKey,
      });
      setDialogOpen(false);
      resetForm();
      await fetchProviders();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to create provider');
    } finally {
      setSubmitting(false);
    }
  };

  const handleDelete = async (id: string) => {
    if (!confirm(t('providers.deleteConfirm'))) return;
    try {
      await apiDelete(`/api/admin/providers/${id}`);
      await fetchProviders();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to delete provider');
    }
  };

  const openEditDialog = (p: Provider) => {
    setEditProvider(p);
    setEditDisplayName(p.display_name);
    setEditBaseUrl(p.base_url);
    setEditApiKey('');
    setEditError('');
    setEditDialogOpen(true);
  };

  const handleEdit = async () => {
    if (!editProvider) return;
    setEditError('');
    setEditSaving(true);
    try {
      const body: Record<string, string> = {
        display_name: editDisplayName,
        base_url: editBaseUrl,
      };
      if (editApiKey) body.api_key = editApiKey;
      await apiPatch(`/api/admin/providers/${editProvider.id}`, body);
      setEditDialogOpen(false);
      setEditProvider(null);
      await fetchProviders();
    } catch (err) {
      setEditError(err instanceof Error ? err.message : 'Failed to update provider');
    } finally {
      setEditSaving(false);
    }
  };

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('providers.title')}</h1>
          <p className="text-muted-foreground">{t('providers.subtitle')}</p>
        </div>
        <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
          <DialogTrigger render={<Button />}>
            <Plus className="h-4 w-4" />
            {t('providers.addProvider')}
          </DialogTrigger>
          <DialogContent className="sm:max-w-md">
            <DialogHeader>
              <DialogTitle>{t('providers.addProvider')}</DialogTitle>
              <DialogDescription>{t('providers.dialogDescription')}</DialogDescription>
            </DialogHeader>
            <form onSubmit={handleCreate} className="space-y-4">
              {formError && (
                <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">{formError}</div>
              )}
              <div className="space-y-2">
                <Label htmlFor="prov-name">{t('common.name')}</Label>
                <Input id="prov-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="my-openai" required />
              </div>
              <div className="space-y-2">
                <Label htmlFor="prov-display">{t('providers.displayName')}</Label>
                <Input id="prov-display" value={displayName} onChange={(e) => setDisplayName(e.target.value)} placeholder="OpenAI Production" />
              </div>
              <div className="space-y-2">
                <Label htmlFor="prov-type">{t('providers.providerType')}</Label>
                <select
                  id="prov-type"
                  value={providerType}
                  onChange={(e) => setProviderType(e.target.value)}
                  className="flex h-8 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-sm"
                >
                  <option value="openai">OpenAI</option>
                  <option value="anthropic">Anthropic</option>
                  <option value="google">Google</option>
                  <option value="custom">Custom</option>
                </select>
              </div>
              <div className="space-y-2">
                <Label htmlFor="prov-url">{t('providers.baseUrl')}</Label>
                <Input id="prov-url" value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder="https://api.openai.com/v1" required />
              </div>
              <div className="space-y-2">
                <Label htmlFor="prov-key">{t('providers.apiKey')}</Label>
                <Input id="prov-key" type="password" value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder="sk-..." required />
              </div>
              <DialogFooter>
                <Button type="submit" disabled={submitting}>
                  {submitting ? t('providers.creating') : t('providers.createProvider')}
                </Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      </div>

      {error && (
        <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">{error}</div>
      )}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('providers.allProviders')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <p className="text-sm text-muted-foreground">{t('providers.loadingProviders')}</p>
          ) : providers.length === 0 ? (
            <div className="flex flex-col items-center justify-center py-12 text-center">
              <p className="text-sm text-muted-foreground">{t('providers.noProviders')}</p>
              <p className="text-xs text-muted-foreground mt-1">{t('providers.noProvidersHint')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>{t('common.name')}</TableHead>
                  <TableHead>{t('providers.type')}</TableHead>
                  <TableHead>{t('providers.baseUrl')}</TableHead>
                  <TableHead>{t('common.status')}</TableHead>
                  <TableHead>{t('providers.created')}</TableHead>
                  <TableHead className="w-10" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {providers.map((p) => (
                  <TableRow key={p.id}>
                    <TableCell className="font-medium">{p.display_name || p.name}</TableCell>
                    <TableCell>
                      <Badge variant={providerTypeColors[p.provider_type] ?? 'outline'}>
                        {p.provider_type}
                      </Badge>
                    </TableCell>
                    <TableCell className="font-mono text-xs">{p.base_url}</TableCell>
                    <TableCell>
                      <Badge variant={p.is_active ? 'default' : 'destructive'}>
                        {p.is_active ? t('common.active') : t('common.inactive')}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-xs text-muted-foreground">
                      {new Date(p.created_at).toLocaleDateString()}
                    </TableCell>
                    <TableCell>
                      <div className="flex gap-1">
                        <Button variant="ghost" size="icon-sm" onClick={() => openEditDialog(p)} title={t('common.edit')}>
                          <Pencil className="h-4 w-4" />
                        </Button>
                        <Button variant="ghost" size="icon-sm" onClick={() => handleDelete(p.id)} title={t('common.delete')}>
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

      {/* Edit Provider Dialog */}
      <Dialog open={editDialogOpen} onOpenChange={setEditDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{t('providers.editProvider')}</DialogTitle>
            <DialogDescription>{t('providers.editDescription')}</DialogDescription>
          </DialogHeader>
          <div className="space-y-4">
            {editError && (
              <div className="rounded-md bg-destructive/10 p-3 text-sm text-destructive">{editError}</div>
            )}
            <div className="space-y-2">
              <Label>{t('providers.displayName')}</Label>
              <Input value={editDisplayName} onChange={(e) => setEditDisplayName(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label>{t('providers.baseUrl')}</Label>
              <Input value={editBaseUrl} onChange={(e) => setEditBaseUrl(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label>{t('providers.apiKey')}</Label>
              <Input type="password" value={editApiKey} onChange={(e) => setEditApiKey(e.target.value)} placeholder={t('providers.apiKeyUnchanged')} />
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setEditDialogOpen(false)}>{t('common.cancel')}</Button>
            <Button onClick={handleEdit} disabled={editSaving}>
              {editSaving ? t('common.loading') : t('common.save')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
