import { useTranslation } from 'react-i18next';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { SIMPLE_TEMPLATES } from '@/routes/admin/roles/types';
import type { PermissionDef, RoleResponse, SimpleTemplate } from '@/routes/admin/roles/types';

export interface RoleScopeState {
  perms: Set<string>;
  setPerms: (s: Set<string>) => void;
  /** `null` = unrestricted (all models allowed), Set = restrict to listed. */
  models: Set<string> | null;
  setModels: (s: Set<string> | null) => void;
  /** `null` = unrestricted, Set = restrict to listed namespaced tool keys. */
  mcpTools: Set<string> | null;
  setMcpTools: (s: Set<string> | null) => void;
}

interface StepBasicsProps {
  name: string;
  onNameChange: (v: string) => void;
  description: string;
  onDescriptionChange: (v: string) => void;
  /** Edit mode hides the clone pickers and may disable the name field. */
  mode: 'create' | 'edit';
  nameDisabled?: boolean;
  /** Only used in create mode — list of existing roles to clone from. */
  roles?: RoleResponse[];
  /** Only used in create mode — permission catalog (to filter stale template keys). */
  permissions?: PermissionDef[];
  /** Only used in create mode — writes all scope state atomically when a
   *  clone source or simple-template is picked. */
  scopeState?: RoleScopeState;
  /** Edit mode: surfaced as small read-only metadata at the bottom of
   *  the step so the admin sees who/when before they make changes. */
  metadata?: {
    created_at?: string;
    updated_at?: string;
  };
}

/**
 * Wizard step 1: name, description, and (in create mode) a "start from"
 * picker that clones an existing role or drops in a curated template.
 */
export function StepBasics({
  name,
  onNameChange,
  description,
  onDescriptionChange,
  mode,
  nameDisabled,
  roles,
  permissions,
  scopeState,
  metadata,
}: StepBasicsProps) {
  const { t } = useTranslation();

  const handleCloneFrom = (roleId: string) => {
    if (!scopeState || !roles) return;
    const src = roles.find((r) => r.id === roleId);
    if (!src) return;
    scopeState.setPerms(new Set(src.permissions));
    scopeState.setModels(src.allowed_models === null ? null : new Set(src.allowed_models));
    scopeState.setMcpTools(src.allowed_mcp_tools === null ? null : new Set(src.allowed_mcp_tools));
  };

  const handleApplyTemplate = (tplId: string) => {
    if (!scopeState || !permissions) return;
    const tpl: SimpleTemplate | undefined = SIMPLE_TEMPLATES.find((x) => x.id === tplId);
    if (!tpl) return;
    const valid = new Set(permissions.map((p) => p.key));
    scopeState.setPerms(new Set(tpl.permissions.filter((k) => valid.has(k))));
    // Templates may pin a model/tool allowlist; undefined = leave open.
    scopeState.setModels(tpl.models === undefined ? null : new Set(tpl.models));
    scopeState.setMcpTools(tpl.mcpTools === undefined ? null : new Set(tpl.mcpTools));
  };

  return (
    <div className="space-y-4">
      <div className="grid gap-3 md:grid-cols-2">
        <div>
          <Label htmlFor={`${mode}-role-name`}>{t('common.name')}</Label>
          <Input
            id={`${mode}-role-name`}
            value={name}
            onChange={(e) => onNameChange(e.target.value)}
            required
            disabled={nameDisabled}
            className={nameDisabled ? 'font-mono' : undefined}
          />
        </div>
        <div>
          <Label htmlFor={`${mode}-role-desc`}>{t('common.description')}</Label>
          <Input
            id={`${mode}-role-desc`}
            value={description}
            onChange={(e) => onDescriptionChange(e.target.value)}
          />
        </div>
      </div>

      {mode === 'create' && scopeState && roles && permissions && (
        <div className="grid gap-3 md:grid-cols-2">
          <div>
            <Label className="text-sm font-medium">{t('roles.cloneFrom')}</Label>
            <p className="mb-1.5 text-xs text-muted-foreground">{t('roles.cloneFromDesc')}</p>
            <Select value="" onValueChange={handleCloneFrom}>
              <SelectTrigger>
                <SelectValue placeholder={t('roles.cloneFromPlaceholder')} />
              </SelectTrigger>
              <SelectContent>
                {roles.map((r) => (
                  <SelectItem key={r.id} value={r.id}>
                    <span className="font-mono text-xs">{r.name}</span>
                    {r.is_system && (
                      <span className="ml-2 text-[10px] text-muted-foreground">
                        {t('roles.systemRole')}
                      </span>
                    )}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div>
            <Label className="text-sm font-medium">{t('roles.startFromTemplate')}</Label>
            <p className="mb-1.5 text-xs text-muted-foreground">
              {t('roles.startFromTemplateDesc')}
            </p>
            <Select value="" onValueChange={handleApplyTemplate}>
              <SelectTrigger>
                <SelectValue placeholder={t('roles.pickTemplate')} />
              </SelectTrigger>
              <SelectContent>
                {SIMPLE_TEMPLATES.map((tpl) => (
                  <SelectItem key={tpl.id} value={tpl.id}>
                    {t(`roles.template_${tpl.id}` as const, { defaultValue: tpl.id })}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </div>
      )}

      {metadata && (metadata.created_at || metadata.updated_at) && (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-t pt-3 text-[10px] text-muted-foreground">
          {metadata.created_at && (
            <span>
              {t('roles.metaCreatedAt')}:{' '}
              <span className="font-mono">{new Date(metadata.created_at).toLocaleString()}</span>
            </span>
          )}
          {metadata.updated_at && metadata.updated_at !== metadata.created_at && (
            <span>
              {t('roles.metaUpdatedAt')}:{' '}
              <span className="font-mono">{new Date(metadata.updated_at).toLocaleString()}</span>
            </span>
          )}
        </div>
      )}
    </div>
  );
}
