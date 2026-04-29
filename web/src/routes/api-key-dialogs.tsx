import { useEffect, useState, type FormEvent } from 'react';
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
import { api, apiPost, apiPatch, apiDelete } from '@/lib/api';
import { ConfirmDialog } from '@/components/confirm-dialog';
import {
  ScopeDropdown,
  ToolScopeDropdown,
  type ModelsByProvider,
  type McpToolsByServer,
} from '@/components/roles/PermissionTree';

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
  allowed_mcp_tools: string[] | null;
  /// Per-MCP-server account-label override map. Empty `{}` ⇒ the
  /// gateway always picks the user's default credential.
  mcp_account_overrides?: Record<string, string>;
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

interface CreateKeyResponse {
  id: string;
  // Backend (POST /api/keys) returns this field as `key`, matching the
  // rotate response. Using `api_key` here left createdKey undefined after
  // a successful create, so the dialog stayed on the form and repeated
  // submits created duplicate keys.
  key: string;
}

interface RotateKeyResponse {
  id: string;
  key: string;
  name: string;
  key_prefix: string;
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
  modelsByProvider: ModelsByProvider;
  mcpToolsByServer: McpToolsByServer;
  costCenterOptions: string[];
  onCostCenterAdded: (tag: string) => void;
}

export function CreateApiKeyDialog({
  open,
  onOpenChange,
  onSuccess,
  modelsByProvider,
  mcpToolsByServer,
  costCenterOptions,
  onCostCenterAdded,
}: CreateApiKeyDialogProps) {
  const { t } = useTranslation();

  const [name, setName] = useState('');
  // null = unrestricted (use every model/tool the role grants). Any Set
  // = explicit subset. Scope pickers collapse empty Set → null on
  // change, so "clear all" from the dropdown returns us to unrestricted.
  const [selectedModels, setSelectedModels] = useState<Set<string> | null>(null);
  const [selectedMcpTools, setSelectedMcpTools] = useState<Set<string> | null>(null);
  const [createSurfaces, setCreateSurfaces] = useState<Surface[]>(['ai_gateway', 'mcp_gateway']);
  const [expiresInDays, setExpiresInDays] = useState('');
  const [createCostCenter, setCreateCostCenter] = useState('');

  const [formError, setFormError] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [createdKey, setCreatedKey] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const resetForm = () => {
    setName('');
    setSelectedModels(null);
    setSelectedMcpTools(null);
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
      // A scope only applies if the matching surface is selected — an
      // mcp_gateway-less key carrying an allowed_mcp_tools list would
      // just be dead weight. Null = inherit role limits.
      const aiEnabled = createSurfaces.includes('ai_gateway');
      const mcpEnabled = createSurfaces.includes('mcp_gateway');
      const allowedModels =
        aiEnabled && selectedModels && selectedModels.size > 0
          ? Array.from(selectedModels)
          : null;
      const allowedMcpTools =
        mcpEnabled && selectedMcpTools && selectedMcpTools.size > 0
          ? Array.from(selectedMcpTools)
          : null;
      const res = await apiPost<CreateKeyResponse>('/api/keys', {
        name,
        surfaces: createSurfaces,
        allowed_models: allowedModels,
        allowed_mcp_tools: allowedMcpTools,
        expires_in_days: expiresInDays ? parseInt(expiresInDays, 10) : undefined,
        cost_center: createCostCenter.trim() ? createCostCenter.trim() : undefined,
      });
      if (createCostCenter.trim()) {
        onCostCenterAdded(createCostCenter.trim());
      }
      setCreatedKey(res.key);
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
              <div className="space-y-3 rounded-md border p-3">
                {ALL_SURFACES.map((s) => {
                  const checked = createSurfaces.includes(s);
                  return (
                    <div key={s} className="space-y-2">
                      <label className="flex cursor-pointer items-center gap-2 text-sm">
                        <Checkbox
                          checked={checked}
                          onCheckedChange={() =>
                            toggleSurface(createSurfaces, setCreateSurfaces, s)
                          }
                        />
                        {t(`apiKeys.surface_${s}` as const)}
                      </label>
                      {checked && s === 'ai_gateway' && (
                        <div className="flex flex-wrap items-center gap-2 pl-6 text-xs">
                          <ScopeDropdown
                            label={t('apiKeys.allowedModels')}
                            selected={selectedModels}
                            onChange={setSelectedModels}
                            modelsByProvider={modelsByProvider}
                          />
                        </div>
                      )}
                      {checked && s === 'mcp_gateway' && (
                        <div className="flex flex-wrap items-center gap-2 pl-6 text-xs">
                          <ToolScopeDropdown
                            label={t('apiKeys.allowedMcpTools')}
                            selected={selectedMcpTools}
                            onChange={setSelectedMcpTools}
                            mcpToolsByServer={mcpToolsByServer}
                          />
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
            <div className="space-y-2">
              <Label htmlFor="key-expires">{t('apiKeys.expiresInDays')}</Label>
              <Input id="key-expires" type="number" value={expiresInDays} onChange={(e) => setExpiresInDays(e.target.value)} placeholder="90" />
              <p className="text-xs text-muted-foreground">{t('apiKeys.expiresInDaysHint')}</p>
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
  modelsByProvider: ModelsByProvider;
  mcpToolsByServer: McpToolsByServer;
  costCenterOptions: string[];
  onCostCenterAdded: (tag: string) => void;
}

export function EditApiKeyDialog({
  open,
  onOpenChange,
  onSuccess,
  apiKey,
  modelsByProvider,
  mcpToolsByServer,
  costCenterOptions,
  onCostCenterAdded,
}: EditApiKeyDialogProps) {
  const { t } = useTranslation();

  const [editSelectedModels, setEditSelectedModels] = useState<Set<string> | null>(null);
  const [editSelectedMcpTools, setEditSelectedMcpTools] = useState<Set<string> | null>(null);
  const [editSurfaces, setEditSurfaces] = useState<Surface[]>([]);
  const [editExpiresInDays, setEditExpiresInDays] = useState('');
  const [editRotationPeriod, setEditRotationPeriod] = useState('');
  const [editInactivityTimeout, setEditInactivityTimeout] = useState('');
  const [editCostCenter, setEditCostCenter] = useState('');
  const [editMcpOverrides, setEditMcpOverrides] = useState<Record<string, string>>({});
  const [editSubmitting, setEditSubmitting] = useState(false);
  const [editError, setEditError] = useState('');

  // Sync local state when the dialog opens with a new key
  const [lastKeyId, setLastKeyId] = useState<string | null>(null);
  if (apiKey && apiKey.id !== lastKeyId) {
    setLastKeyId(apiKey.id);
    setEditSelectedModels(
      apiKey.allowed_models && apiKey.allowed_models.length > 0
        ? new Set(apiKey.allowed_models)
        : null,
    );
    setEditSelectedMcpTools(
      apiKey.allowed_mcp_tools && apiKey.allowed_mcp_tools.length > 0
        ? new Set(apiKey.allowed_mcp_tools)
        : null,
    );
    setEditSurfaces(apiKey.surfaces ?? []);
    setEditExpiresInDays('');
    setEditRotationPeriod(apiKey.rotation_period_days?.toString() ?? '');
    setEditInactivityTimeout(apiKey.inactivity_timeout_days?.toString() ?? '');
    setEditCostCenter(apiKey.cost_center ?? '');
    setEditMcpOverrides(apiKey.mcp_account_overrides ?? {});
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
      const mcpEnabled = editSurfaces.includes('mcp_gateway');
      const allowedModels =
        aiEnabled && editSelectedModels && editSelectedModels.size > 0
          ? Array.from(editSelectedModels)
          : null;
      const allowedMcpTools =
        mcpEnabled && editSelectedMcpTools && editSelectedMcpTools.size > 0
          ? Array.from(editSelectedMcpTools)
          : null;
      // Strip empty-string entries so the backend's "missing label"
      // validator doesn't reject them — empty just means "no override
      // for that server".
      const overrides = Object.fromEntries(
        Object.entries(editMcpOverrides).filter(([, label]) => label.trim().length > 0),
      );
      await apiPatch(`/api/keys/${apiKey.id}`, {
        allowed_models: allowedModels,
        allowed_mcp_tools: allowedMcpTools,
        surfaces: editSurfaces,
        expires_in_days: editExpiresInDays ? parseInt(editExpiresInDays, 10) : undefined,
        rotation_period_days: editRotationPeriod ? parseInt(editRotationPeriod, 10) : null,
        inactivity_timeout_days: editInactivityTimeout ? parseInt(editInactivityTimeout, 10) : null,
        cost_center: editCostCenter.trim(),
        mcp_account_overrides: overrides,
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
            <div className="space-y-3 rounded-md border p-3">
              {ALL_SURFACES.map((s) => {
                const checked = editSurfaces.includes(s);
                return (
                  <div key={s} className="space-y-2">
                    <label className="flex cursor-pointer items-center gap-2 text-sm">
                      <Checkbox
                        checked={checked}
                        onCheckedChange={() =>
                          toggleSurface(editSurfaces, setEditSurfaces, s)
                        }
                      />
                      {t(`apiKeys.surface_${s}` as const)}
                    </label>
                    {checked && s === 'ai_gateway' && (
                      <div className="flex flex-wrap items-center gap-2 pl-6 text-xs">
                        <ScopeDropdown
                          label={t('apiKeys.allowedModels')}
                          selected={editSelectedModels}
                          onChange={setEditSelectedModels}
                          modelsByProvider={modelsByProvider}
                        />
                      </div>
                    )}
                    {checked && s === 'mcp_gateway' && (
                      <div className="flex flex-wrap items-center gap-2 pl-6 text-xs">
                        <ToolScopeDropdown
                          label={t('apiKeys.allowedMcpTools')}
                          selected={editSelectedMcpTools}
                          onChange={setEditSelectedMcpTools}
                          mcpToolsByServer={mcpToolsByServer}
                        />
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          </div>
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
          <McpAccountOverridesField
            value={editMcpOverrides}
            onChange={setEditMcpOverrides}
            visible={editSurfaces.includes('mcp_gateway')}
          />
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
// McpAccountOverridesField — per-server account-label dropdown row
// ---------------------------------------------------------------------------

interface McpConnAccount {
  account_label: string;
  is_default: boolean;
}
interface McpServerConn {
  server_id: string;
  server_name: string;
  accounts: McpConnAccount[];
}

function McpAccountOverridesField({
  value,
  onChange,
  visible,
}: {
  value: Record<string, string>;
  onChange: (v: Record<string, string>) => void;
  visible: boolean;
}) {
  const { t } = useTranslation();
  const [conns, setConns] = useState<McpServerConn[] | null>(null);

  useEffect(() => {
    if (!visible) return;
    let cancelled = false;
    api<McpServerConn[]>('/api/mcp/connections')
      .then((data) => {
        if (!cancelled) setConns(data);
      })
      .catch(() => {
        if (!cancelled) setConns([]);
      });
    return () => {
      cancelled = true;
    };
  }, [visible]);

  if (!visible) return null;
  if (conns === null) {
    return (
      <p className="text-xs text-muted-foreground">{t('common.loading')}</p>
    );
  }
  // Only servers where the user has at least 2 accounts give us a real
  // choice — single-account servers always pick the default. Skip them
  // to keep the form short.
  const choosable = conns.filter((c) => c.accounts.length >= 2);
  if (choosable.length === 0) {
    return null;
  }
  return (
    <div className="space-y-2">
      <Label>{t('apiKeys.mcpOverrides')}</Label>
      <p className="text-xs text-muted-foreground">{t('apiKeys.mcpOverridesHint')}</p>
      <div className="space-y-2 rounded-md border p-3">
        {choosable.map((c) => (
          <div key={c.server_id} className="flex items-center justify-between gap-2">
            <span className="text-sm font-medium">{c.server_name}</span>
            <select
              className="h-8 rounded-md border bg-background px-2 text-sm"
              value={value[c.server_id] ?? ''}
              onChange={(e) => {
                const next = { ...value };
                if (e.target.value) {
                  next[c.server_id] = e.target.value;
                } else {
                  delete next[c.server_id];
                }
                onChange(next);
              }}
            >
              <option value="">{t('apiKeys.useDefaultAccount')}</option>
              {c.accounts.map((a) => (
                <option key={a.account_label} value={a.account_label}>
                  {a.account_label}
                  {a.is_default ? ` (${t('connections.default')})` : ''}
                </option>
              ))}
            </select>
          </div>
        ))}
      </div>
    </div>
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
