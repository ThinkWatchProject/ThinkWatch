import { useMemo, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Checkbox } from '@/components/ui/checkbox';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Copy, Check, AlertCircle } from 'lucide-react';
import { apiPost, apiPatch, apiDelete } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

// Must match ALLOWED_SURFACES in crates/server/src/handlers/api_keys.rs.
const ALL_SURFACES = ['ai_gateway', 'mcp_gateway', 'console'] as const;
type Surface = (typeof ALL_SURFACES)[number];

export interface ApiKey {
  id: string;
  name: string;
  key_prefix: string;
  user_id: string | null;
  surfaces: Surface[];
  allowed_models: string[] | null;
  expires_at: string | null;
  is_active: boolean;
  last_used_at: string | null;
  created_at: string;
  deleted_at: string | null;
  rotation_period_days: number | null;
  rotated_from_id: string | null;
  grace_period_ends_at: string | null;
  inactivity_timeout_days: number | null;
  disabled_reason: string | null;
  last_rotation_at: string | null;
  cost_center: string | null;
}

export interface ModelRow {
  id: string;
  model_id: string;
  display_name: string;
  is_active: boolean;
}

interface CreateKeyResponse {
  id: string;
  api_key: string;
}

interface RotateKeyResponse {
  id: string;
  key: string;
  name: string;
  key_prefix: string;
}

// ---------------------------------------------------------------------------
// AllowedModelsEditor
// ---------------------------------------------------------------------------

interface AllowedModelsEditorProps {
  mode: 'all' | 'specific';
  onModeChange: (m: 'all' | 'specific') => void;
  selected: string[];
  onSelectedChange: (ids: string[]) => void;
  available: ModelRow[];
}

function AllowedModelsEditor({
  mode,
  onModeChange,
  selected,
  onSelectedChange,
  available,
}: AllowedModelsEditorProps) {
  const { t } = useTranslation();
  const [search, setSearch] = useState('');

  // Active, search-filtered model list. Provider grouping was dropped
  // with the route-centric redesign: a model can route to multiple
  // providers, so `/api/admin/models` no longer carries a single
  // `provider_name` field. We now render a flat scrollable list with
  // a search box — grouping by a field that doesn't exist produced
  // one giant "undefined" bucket.
  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    return available
      .filter((m) => m.is_active)
      .filter((m) => {
        if (!q) return true;
        return (
          m.model_id.toLowerCase().includes(q) ||
          m.display_name.toLowerCase().includes(q)
        );
      })
      .sort((a, b) => a.model_id.localeCompare(b.model_id));
  }, [available, search]);

  const toggleModel = (modelId: string) => {
    if (selected.includes(modelId)) {
      onSelectedChange(selected.filter((id) => id !== modelId));
    } else {
      onSelectedChange([...selected, modelId]);
    }
  };

  return (
    <div className="space-y-2">
      <Label>{t('apiKeys.allowedModels')}</Label>
      <div
        role="radiogroup"
        aria-label={t('apiKeys.allowedModels')}
        className="flex gap-4 text-sm"
      >
        <label className="flex cursor-pointer items-center gap-2">
          <input
            type="radio"
            name="allowed-models-mode"
            checked={mode === 'all'}
            onChange={() => onModeChange('all')}
          />
          {t('apiKeys.allowedModels_allModels')}
        </label>
        <label className="flex cursor-pointer items-center gap-2">
          <input
            type="radio"
            name="allowed-models-mode"
            checked={mode === 'specific'}
            onChange={() => onModeChange('specific')}
          />
          {t('apiKeys.allowedModels_specificModels')}
        </label>
      </div>
      {mode === 'specific' && (
        <div className="space-y-2 rounded-md border p-3 text-sm">
          <p className="text-xs text-muted-foreground">
            {t('apiKeys.allowedModels_pickModelsHint')}
          </p>
          {available.length === 0 ? (
            <p className="text-xs text-muted-foreground">
              {t('apiKeys.allowedModels_noModels')}
            </p>
          ) : (
            <>
              <Input
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder={t('models.searchPlaceholder')}
                className="h-8 text-xs"
              />
              <div className="max-h-64 overflow-y-auto rounded-md border bg-muted/20">
                {filtered.length === 0 ? (
                  <p className="p-3 text-xs text-muted-foreground">
                    {t('apiKeys.allowedModels_noModels')}
                  </p>
                ) : (
                  <div className="grid grid-cols-1 gap-1 p-2 sm:grid-cols-2">
                    {filtered.map((m) => (
                      <label
                        key={m.id}
                        className="flex cursor-pointer items-center gap-2 rounded px-1 py-0.5 hover:bg-muted/50"
                      >
                        <Checkbox
                          checked={selected.includes(m.model_id)}
                          onCheckedChange={() => toggleModel(m.model_id)}
                        />
                        <span className="truncate">{m.display_name}</span>
                        <code className="ml-auto truncate text-[10px] text-muted-foreground">
                          {m.model_id}
                        </code>
                      </label>
                    ))}
                  </div>
                )}
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function toggleSurface(
  list: Surface[],
  setList: (v: Surface[]) => void,
  surface: Surface,
) {
  if (list.includes(surface)) {
    setList(list.filter((s) => s !== surface));
  } else {
    setList([...list, surface]);
  }
}

// ---------------------------------------------------------------------------
// CreateApiKeyDialog
// ---------------------------------------------------------------------------

interface CreateApiKeyDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSuccess: () => void;
  availableModels: ModelRow[];
  costCenterOptions: string[];
  onCostCenterAdded: (tag: string) => void;
}

export function CreateApiKeyDialog({
  open,
  onOpenChange,
  onSuccess,
  availableModels,
  costCenterOptions,
  onCostCenterAdded,
}: CreateApiKeyDialogProps) {
  const { t } = useTranslation();

  const [name, setName] = useState('');
  const [allowedModelsMode, setAllowedModelsMode] = useState<'all' | 'specific'>('all');
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
  const [createSurfaces, setCreateSurfaces] = useState<Surface[]>(['ai_gateway', 'mcp_gateway']);
  const [expiresInDays, setExpiresInDays] = useState('');
  const [createCostCenter, setCreateCostCenter] = useState('');

  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [createdKey, setCreatedKey] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const resetForm = () => {
    setName('');
    setAllowedModelsMode('all');
    setSelectedModels([]);
    setCreateSurfaces(['ai_gateway', 'mcp_gateway']);
    setExpiresInDays('');
    setCreateCostCenter('');
    setFormError('');
    setCreatedKey(null);
    setCopied(false);
  };

  const handleDialogChange = (nextOpen: boolean) => {
    onOpenChange(nextOpen);
    if (!nextOpen) resetForm();
  };

  const handleCreate = async (e: FormEvent) => {
    e.preventDefault();
    setFormError('');
    if (createSurfaces.length === 0) {
      setFormError(t('apiKeys.surfacesRequired'));
      return;
    }
    setCreatedKey(null);
    setCopied(false);
    setSubmitting(true);
    try {
      const aiEnabled = createSurfaces.includes('ai_gateway');
      const allowedModels =
        aiEnabled && allowedModelsMode === 'specific' && selectedModels.length > 0
          ? selectedModels
          : null;
      const res = await apiPost<CreateKeyResponse>('/api/keys', {
        name,
        surfaces: createSurfaces,
        allowed_models: allowedModels,
        expires_in_days: expiresInDays ? parseInt(expiresInDays, 10) : undefined,
        cost_center: createCostCenter.trim() ? createCostCenter.trim() : undefined,
      });
      if (createCostCenter.trim()) {
        onCostCenterAdded(createCostCenter.trim());
      }
      setCreatedKey(res.api_key);
      onSuccess();
    } catch (err) {
      setFormError(err instanceof Error ? err.message : 'Failed to create key');
    } finally {
      setSubmitting(false);
    }
  };

  const handleCopy = async () => {
    if (createdKey) {
      await navigator.clipboard.writeText(createdKey);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    }
  };

  return (
    <Dialog open={open} onOpenChange={handleDialogChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{createdKey ? t('apiKeys.keyCreated') : t('apiKeys.createKey')}</DialogTitle>
          <DialogDescription>
            {createdKey ? t('apiKeys.keyCreatedHint') : t('apiKeys.dialogDescription')}
          </DialogDescription>
        </DialogHeader>
        {createdKey ? (
          <div className="space-y-4">
            <div className="rounded-md border bg-muted p-3">
              <code className="text-sm break-all">{createdKey}</code>
            </div>
            <Button variant="outline" className="w-full" onClick={handleCopy}>
              {copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
              {copied ? t('common.copied') : t('apiKeys.copyToClipboard')}
            </Button>
            <DialogFooter>
              <Button onClick={() => handleDialogChange(false)}>{t('common.done')}</Button>
            </DialogFooter>
          </div>
        ) : (
          <form onSubmit={handleCreate} className="space-y-4">
            {formError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{formError}</AlertDescription>
              </Alert>
            )}
            <div className="space-y-2">
              <Label htmlFor="key-name">{t('common.name')}</Label>
              <Input id="key-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="my-service-key" required />
            </div>
            <div className="space-y-2">
              <Label>{t('apiKeys.surfaces')}</Label>
              <p className="text-xs text-muted-foreground">
                {t('apiKeys.surfacesHint')}
              </p>
              <div className="space-y-2 rounded-md border p-3">
                {ALL_SURFACES.map((s) => (
                  <label
                    key={s}
                    className="flex cursor-pointer items-center gap-2 text-sm"
                  >
                    <Checkbox
                      checked={createSurfaces.includes(s)}
                      onCheckedChange={() => toggleSurface(createSurfaces, setCreateSurfaces, s)}
                    />
                    {t(`apiKeys.surface_${s}` as const)}
                  </label>
                ))}
              </div>
            </div>
            {createSurfaces.includes('ai_gateway') && (
              <AllowedModelsEditor
                mode={allowedModelsMode}
                onModeChange={setAllowedModelsMode}
                selected={selectedModels}
                onSelectedChange={setSelectedModels}
                available={availableModels}
              />
            )}
            <div className="space-y-2">
              <Label htmlFor="key-expires">{t('apiKeys.expiresInDays')}</Label>
              <Input id="key-expires" type="number" value={expiresInDays} onChange={(e) => setExpiresInDays(e.target.value)} placeholder="90" />
            </div>
            <div className="space-y-2">
              <Label htmlFor="key-cost-center">{t('apiKeys.costCenter')}</Label>
              <Input
                id="key-cost-center"
                list="cost-center-options"
                value={createCostCenter}
                onChange={(e) => setCreateCostCenter(e.target.value)}
                placeholder={t('apiKeys.costCenterPlaceholder')}
                maxLength={64}
              />
              <datalist id="cost-center-options">
                {costCenterOptions.map((opt) => (
                  <option key={opt} value={opt} />
                ))}
              </datalist>
              <p className="text-xs text-muted-foreground">{t('apiKeys.costCenterHint')}</p>
            </div>
            <DialogFooter>
              <Button type="submit" disabled={submitting}>
                {submitting ? t('apiKeys.creating') : t('apiKeys.createKeyBtn')}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// EditApiKeyDialog
// ---------------------------------------------------------------------------

interface EditApiKeyDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSuccess: () => void;
  apiKey: ApiKey | null;
  availableModels: ModelRow[];
  costCenterOptions: string[];
  onCostCenterAdded: (tag: string) => void;
}

export function EditApiKeyDialog({
  open,
  onOpenChange,
  onSuccess,
  apiKey,
  availableModels,
  costCenterOptions,
  onCostCenterAdded,
}: EditApiKeyDialogProps) {
  const { t } = useTranslation();

  const [editAllowedModelsMode, setEditAllowedModelsMode] = useState<'all' | 'specific'>('all');
  const [editSelectedModels, setEditSelectedModels] = useState<string[]>([]);
  const [editSurfaces, setEditSurfaces] = useState<Surface[]>([]);
  const [editExpiresInDays, setEditExpiresInDays] = useState('');
  const [editRotationPeriod, setEditRotationPeriod] = useState('');
  const [editInactivityTimeout, setEditInactivityTimeout] = useState('');
  const [editCostCenter, setEditCostCenter] = useState('');
  const [editSubmitting, setEditSubmitting] = useState(false);
  const [editError, setEditError] = useState('');

  // Sync local state when the dialog opens with a new key
  const [lastKeyId, setLastKeyId] = useState<string | null>(null);
  if (apiKey && apiKey.id !== lastKeyId) {
    setLastKeyId(apiKey.id);
    if (apiKey.allowed_models && apiKey.allowed_models.length > 0) {
      setEditAllowedModelsMode('specific');
      setEditSelectedModels(apiKey.allowed_models);
    } else {
      setEditAllowedModelsMode('all');
      setEditSelectedModels([]);
    }
    setEditSurfaces(apiKey.surfaces ?? []);
    setEditExpiresInDays('');
    setEditRotationPeriod(apiKey.rotation_period_days?.toString() ?? '');
    setEditInactivityTimeout(apiKey.inactivity_timeout_days?.toString() ?? '');
    setEditCostCenter(apiKey.cost_center ?? '');
    setEditError('');
  }

  // Reset tracking when dialog closes
  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) {
      setLastKeyId(null);
    }
    onOpenChange(nextOpen);
  };

  const handleEdit = async (e: FormEvent) => {
    e.preventDefault();
    if (!apiKey) return;
    if (editSurfaces.length === 0) {
      setEditError(t('apiKeys.surfacesRequired'));
      return;
    }
    setEditSubmitting(true);
    setEditError('');
    try {
      const aiEnabled = editSurfaces.includes('ai_gateway');
      const allowedModels =
        aiEnabled && editAllowedModelsMode === 'specific' && editSelectedModels.length > 0
          ? editSelectedModels
          : null;
      await apiPatch(`/api/keys/${apiKey.id}`, {
        allowed_models: allowedModels,
        surfaces: editSurfaces,
        expires_in_days: editExpiresInDays ? parseInt(editExpiresInDays, 10) : undefined,
        rotation_period_days: editRotationPeriod ? parseInt(editRotationPeriod, 10) : null,
        inactivity_timeout_days: editInactivityTimeout ? parseInt(editInactivityTimeout, 10) : null,
        cost_center: editCostCenter.trim(),
      });
      if (editCostCenter.trim()) {
        onCostCenterAdded(editCostCenter.trim());
      }
      onOpenChange(false);
      onSuccess();
    } catch (err) {
      setEditError(err instanceof Error ? err.message : 'Failed to update key');
    } finally {
      setEditSubmitting(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t('apiKeys.editKey')}</DialogTitle>
          <DialogDescription>{apiKey?.name ?? ''}</DialogDescription>
        </DialogHeader>
        <form onSubmit={handleEdit} className="space-y-4">
          {editError && (
            <Alert variant="destructive">
              <AlertCircle className="h-4 w-4" />
              <AlertDescription>{editError}</AlertDescription>
            </Alert>
          )}
          <div className="space-y-2">
            <Label>{t('apiKeys.surfaces')}</Label>
            <div className="space-y-2 rounded-md border p-3">
              {ALL_SURFACES.map((s) => (
                <label
                  key={s}
                  className="flex cursor-pointer items-center gap-2 text-sm"
                >
                  <Checkbox
                    checked={editSurfaces.includes(s)}
                    onCheckedChange={() => toggleSurface(editSurfaces, setEditSurfaces, s)}
                  />
                  {t(`apiKeys.surface_${s}` as const)}
                </label>
              ))}
            </div>
          </div>
          {editSurfaces.includes('ai_gateway') && (
            <AllowedModelsEditor
              mode={editAllowedModelsMode}
              onModeChange={setEditAllowedModelsMode}
              selected={editSelectedModels}
              onSelectedChange={setEditSelectedModels}
              available={availableModels}
            />
          )}
          <div className="space-y-2">
            <Label>{t('apiKeys.expiresIn')}</Label>
            <Input type="number" value={editExpiresInDays} onChange={(e) => setEditExpiresInDays(e.target.value)} placeholder="90" min={1} />
            <p className="text-xs text-muted-foreground">{t('apiKeys.expiresInHint')}</p>
          </div>
          <div className="space-y-2">
            <Label>{t('apiKeys.rotationPeriod')}</Label>
            <Input type="number" value={editRotationPeriod} onChange={(e) => setEditRotationPeriod(e.target.value)} placeholder="0" min={0} />
          </div>
          <div className="space-y-2">
            <Label>{t('apiKeys.inactivityTimeout')}</Label>
            <Input type="number" value={editInactivityTimeout} onChange={(e) => setEditInactivityTimeout(e.target.value)} placeholder="0" min={0} />
          </div>
          <div className="space-y-2">
            <Label htmlFor="edit-cost-center">{t('apiKeys.costCenter')}</Label>
            <Input
              id="edit-cost-center"
              list="edit-cost-center-options"
              value={editCostCenter}
              onChange={(e) => setEditCostCenter(e.target.value)}
              placeholder={t('apiKeys.costCenterPlaceholder')}
              maxLength={64}
            />
            <datalist id="edit-cost-center-options">
              {costCenterOptions.map((opt) => (
                <option key={opt} value={opt} />
              ))}
            </datalist>
            <p className="text-xs text-muted-foreground">{t('apiKeys.costCenterHint')}</p>
          </div>
          <DialogFooter>
            <Button type="submit" disabled={editSubmitting}>
              {editSubmitting ? t('common.loading') : t('common.save')}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// RotateApiKeyDialog
// ---------------------------------------------------------------------------

interface RotateApiKeyDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSuccess: () => void;
  apiKey: ApiKey | null;
}

export function RotateApiKeyDialog({
  open,
  onOpenChange,
  onSuccess,
  apiKey,
}: RotateApiKeyDialogProps) {
  const { t } = useTranslation();

  const [rotatedNewKey, setRotatedNewKey] = useState<string | null>(null);
  const [rotateSubmitting, setRotateSubmitting] = useState(false);
  const [rotateError, setRotateError] = useState('');
  const [rotateCopied, setRotateCopied] = useState(false);

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) {
      setRotatedNewKey(null);
      setRotateError('');
      setRotateCopied(false);
    }
    onOpenChange(nextOpen);
  };

  const handleRotate = async () => {
    if (!apiKey) return;
    setRotateSubmitting(true);
    setRotateError('');
    try {
      const res = await apiPost<RotateKeyResponse>(`/api/keys/${apiKey.id}/rotate`, {});
      setRotatedNewKey(res.key);
      onSuccess();
    } catch (err) {
      setRotateError(err instanceof Error ? err.message : 'Failed to rotate key');
    } finally {
      setRotateSubmitting(false);
    }
  };

  const handleRotateCopy = async () => {
    if (rotatedNewKey) {
      await navigator.clipboard.writeText(rotatedNewKey);
      setRotateCopied(true);
      setTimeout(() => setRotateCopied(false), 2000);
    }
  };

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{t('apiKeys.rotate')}</DialogTitle>
          <DialogDescription>
            {rotatedNewKey ? t('apiKeys.rotateSuccess') : t('apiKeys.rotateConfirm')}
          </DialogDescription>
        </DialogHeader>
        {rotatedNewKey ? (
          <div className="space-y-4">
            <div className="rounded-md border bg-muted p-3">
              <code className="text-sm break-all">{rotatedNewKey}</code>
            </div>
            <Button variant="outline" className="w-full" onClick={handleRotateCopy}>
              {rotateCopied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
              {rotateCopied ? t('common.copied') : t('apiKeys.copyToClipboard')}
            </Button>
            <DialogFooter>
              <Button onClick={() => handleOpenChange(false)}>{t('common.done')}</Button>
            </DialogFooter>
          </div>
        ) : (
          <div className="space-y-4">
            {rotateError && (
              <Alert variant="destructive">
                <AlertCircle className="h-4 w-4" />
                <AlertDescription>{rotateError}</AlertDescription>
              </Alert>
            )}
            <p className="text-sm text-muted-foreground">
              Key: <code className="rounded bg-muted px-1.5 py-0.5 text-xs">{apiKey?.key_prefix}</code> ({apiKey?.name})
            </p>
            <DialogFooter>
              <Button variant="outline" onClick={() => handleOpenChange(false)}>
                {t('common.cancel')}
              </Button>
              <Button onClick={handleRotate} disabled={rotateSubmitting}>
                {rotateSubmitting ? t('common.loading') : t('apiKeys.rotate')}
              </Button>
            </DialogFooter>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// DeleteApiKeyDialog
// ---------------------------------------------------------------------------

interface DeleteApiKeyDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSuccess: () => void;
  keyId: string | null;
}

export function DeleteApiKeyDialog({
  open,
  onOpenChange,
  onSuccess,
  keyId,
}: DeleteApiKeyDialogProps) {
  const { t } = useTranslation();

  const handleRevoke = async () => {
    if (!keyId) return;
    try {
      await apiDelete(`/api/keys/${keyId}`);
      onOpenChange(false);
      onSuccess();
    } catch (err) {
      // Toast is handled by the parent — just close and report via throw
      throw err;
    }
  };

  return (
    <ConfirmDialog
      open={open}
      onOpenChange={onOpenChange}
      title={t('common.revoke')}
      description={t('apiKeys.revokeConfirm')}
      variant="destructive"
      confirmLabel={t('common.revoke')}
      onConfirm={handleRevoke}
    />
  );
}
