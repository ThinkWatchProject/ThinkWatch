import { useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from '@tanstack/react-router';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Textarea } from '@/components/ui/textarea';
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
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Skeleton } from '@/components/ui/skeleton';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTablePagination } from '@/components/data-table-pagination';
import { useClientPagination } from '@/hooks/use-client-pagination';
import { AlertCircle, MoreHorizontal, Plus, Trash2, Users } from 'lucide-react';
import { api, apiDelete, apiPost, hasPermission } from '@/lib/api';
import type { Team } from '@/lib/types';
import { toast } from 'sonner';

export function TeamsPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [teams, setTeams] = useState<Team[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const teamsPager = useClientPagination(teams, 20);

  // Create
  const [dialogOpen, setDialogOpen] = useState(false);
  const [formName, setFormName] = useState('');
  const [formDesc, setFormDesc] = useState('');
  const [formError, setFormError] = useState('');
  const [saving, setSaving] = useState(false);

  // Delete
  const [deleteTarget, setDeleteTarget] = useState<Team | null>(null);

  const fetchTeams = async () => {
    setLoading(true);
    try {
      const data = await api<Team[]>('/api/admin/teams');
      setTeams(data);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to load teams');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void fetchTeams();
  }, []);

  const openCreate = () => {
    setFormName('');
    setFormDesc('');
    setFormError('');
    setDialogOpen(true);
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    if (!formName.trim()) {
      setFormError(t('teams.errors.nameRequired'));
      return;
    }
    setSaving(true);
    try {
      await apiPost('/api/admin/teams', { name: formName, description: formDesc || null });
      toast.success(t('teams.toast.created'));
      setDialogOpen(false);
      await fetchTeams();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to save');
    } finally {
      setSaving(false);
    }
  };

  const confirmDelete = async () => {
    if (!deleteTarget) return;
    try {
      await apiDelete(`/api/admin/teams/${deleteTarget.id}`);
      toast.success(t('teams.toast.deleted'));
      setDeleteTarget(null);
      await fetchTeams();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : 'Failed to delete');
    }
  };

  return (
    <div className="flex flex-col flex-1 min-h-0">
      <div className="flex items-center justify-between mb-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('teams.title')}</h1>
          <p className="text-muted-foreground">{t('teams.subtitle')}</p>
        </div>
        <Button onClick={openCreate} disabled={!hasPermission('teams:create')}>
          <Plus className="mr-2 h-4 w-4" />
          {t('teams.addTeam')}
        </Button>
      </div>

      {error && (
        <Alert variant="destructive" className="mb-4">
          <AlertCircle className="h-4 w-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}

      <Card className="flex flex-col min-h-0 flex-1 py-0 gap-0">
        <CardContent className="p-0 overflow-auto flex-1 [&>[data-slot=table-container]]:overflow-visible">
          {loading ? (
            <div className="space-y-4 p-4">
              {[...Array(3)].map((_, i) => (
                <Skeleton key={i} className="h-16 w-full" />
              ))}
            </div>
          ) : teams.length === 0 ? (
            <div className="flex h-full flex-col items-center justify-center text-center">
              <Users className="mb-3 h-10 w-10 text-muted-foreground" />
              <p className="text-sm text-muted-foreground">{t('teams.noTeams')}</p>
              <p className="mt-1 text-xs text-muted-foreground">{t('teams.noTeamsHint')}</p>
            </div>
          ) : (
            <Table>
              <TableHeader className="sticky top-0 z-10 bg-card [&_tr]:border-b shadow-[inset_0_-1px_0_var(--border)]">
                <TableRow>
                  <TableHead>{t('teams.col.name')}</TableHead>
                  <TableHead>{t('teams.col.description')}</TableHead>
                  <TableHead className="text-center">{t('teams.col.members')}</TableHead>
                  <TableHead className="text-right">{t('common.actions')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {teamsPager.paginated.map((team) => (
                  <TableRow key={team.id}>
                    <TableCell
                      className="cursor-pointer font-medium hover:underline"
                      onClick={() => navigate({ to: '/admin/teams/$id', params: { id: team.id } })}
                    >
                      {team.name}
                    </TableCell>
                    <TableCell className="text-sm text-muted-foreground">
                      {team.description || '—'}
                    </TableCell>
                    <TableCell className="text-center">
                      <Badge variant="secondary">{team.member_count}</Badge>
                    </TableCell>
                    <TableCell className="text-right">
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => navigate({ to: '/admin/teams/$id', params: { id: team.id } })}
                        title={t('teams.viewDetail')}
                      >
                        <MoreHorizontal className="h-4 w-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        onClick={() => setDeleteTarget(team)}
                        title={t('common.delete')}
                        disabled={!hasPermission('teams:delete')}
                      >
                        <Trash2 className="h-4 w-4 text-destructive" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
        <div data-slot="card-footer" className="border-t">
          <DataTablePagination
            total={teamsPager.total}
            page={teamsPager.page}
            pageSize={teamsPager.pageSize}
            onPageChange={teamsPager.setPage}
            onPageSizeChange={teamsPager.setPageSize}
          />
        </div>
      </Card>

      {/* --- Create dialog --- */}
      <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <form onSubmit={submit}>
            <DialogHeader>
              <DialogTitle>{t('teams.createTitle')}</DialogTitle>
              <DialogDescription>{t('teams.formHint')}</DialogDescription>
            </DialogHeader>
            <div className="space-y-4 py-4">
              <div className="space-y-2">
                <Label htmlFor="team-name">{t('teams.field.name')}</Label>
                <Input
                  id="team-name"
                  value={formName}
                  onChange={(e) => setFormName(e.target.value)}
                  placeholder="engineering"
                  required
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="team-desc">{t('teams.field.description')}</Label>
                <Textarea
                  id="team-desc"
                  value={formDesc}
                  onChange={(e) => setFormDesc(e.target.value)}
                  rows={3}
                />
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
        open={deleteTarget !== null}
        onOpenChange={(o) => !o && setDeleteTarget(null)}
        title={t('teams.deleteTitle')}
        description={t('teams.deleteConfirm', { team: deleteTarget?.name ?? '' })}
        confirmLabel={t('common.delete')}
        variant="destructive"
        onConfirm={confirmDelete}
      />
    </div>
  );
}
