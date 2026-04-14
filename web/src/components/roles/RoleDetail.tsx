import { type ReactNode, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import {
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { FileJson, Lock, Shield } from 'lucide-react';
import { RoleHistory } from './RoleHistory';
import { RoleMembers } from './RoleMembers';
import type {
  McpServer,
  PermissionDef,
  RoleResponse,
} from '@/routes/admin/roles/types';

interface RoleDetailProps {
  role: RoleResponse;
  /** Permission catalog grouped by resource — used to render only the
   *  resources the role actually grants something in. */
  grouped: Map<string, PermissionDef[]>;
  dangerousKeys: Set<string>;
  availableServers: McpServer[];
  teamsById: Map<string, { id: string; name: string }>;
  onMembersChanged: () => void;
}

/**
 * Read-only inspector for a role. Shown as a dialog when the admin
 * clicks a row in the roles list. Two tabs:
 *  - Overview: user count, permissions (or raw policy_document for
 *    policy-mode roles), model/tool constraints, and members.
 *  - History: delegated to <RoleHistory>.
 */
export function RoleDetail({
  role,
  grouped,
  dangerousKeys,
  availableServers,
  teamsById,
  onMembersChanged,
}: RoleDetailProps) {
  const { t } = useTranslation();
  const selected = new Set(role.permissions);
  const [detailTab, setDetailTab] = useState<'overview' | 'history'>('overview');

  return (
    <>
      <DialogHeader>
        <DialogTitle className="flex items-center gap-2">
          {role.is_system ? (
            <Lock className="h-4 w-4 text-muted-foreground" />
          ) : (
            <Shield className="h-4 w-4 text-muted-foreground" />
          )}
          <span className="font-mono">{role.name}</span>
          {role.is_system && (
            <Badge variant="secondary" className="text-[10px]">
              {t('roles.systemRole')}
            </Badge>
          )}
        </DialogTitle>
        <DialogDescription>{role.description || '—'}</DialogDescription>
      </DialogHeader>
      <Tabs
        value={detailTab}
        onValueChange={(v) => setDetailTab(v as 'overview' | 'history')}
      >
        <TabsList className="grid w-full grid-cols-2">
          <TabsTrigger value="overview">{t('roles.detailOverview')}</TabsTrigger>
          <TabsTrigger value="history">{t('roles.detailHistory')}</TabsTrigger>
        </TabsList>
        <TabsContent value="overview" className="space-y-4 py-2">
          <div>
            <div className="mb-2 text-xs uppercase tracking-wider text-muted-foreground">
              {t('roles.colUsers')}
            </div>
            <div className="font-mono text-2xl tabular-nums">{role.user_count}</div>
          </div>
          {role.policy_document ? (
            <div>
              <div className="mb-2 flex items-center gap-1.5 text-xs uppercase tracking-wider text-muted-foreground">
                <FileJson className="h-3.5 w-3.5" />
                {t('roles.policyDocument')}
              </div>
              <pre className="max-h-72 overflow-auto rounded-md border bg-muted/30 p-3 font-mono text-[10px]">
                {JSON.stringify(role.policy_document, null, 2)}
              </pre>
            </div>
          ) : (
            <div>
              <div className="mb-2 text-xs uppercase tracking-wider text-muted-foreground">
                {t('roles.permissions')}
              </div>
              <div className="space-y-2 rounded-md border p-3">
                {Array.from(grouped.entries()).map(([resource, perms]) => {
                  const granted = perms.filter((p) => selected.has(p.key));
                  if (granted.length === 0) return null;
                  return (
                    <div key={resource}>
                      <div className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
                        {t(`permissions.resource.${resource}` as const, {
                          defaultValue: resource,
                        })}
                      </div>
                      <div className="mt-1 flex flex-wrap gap-1">
                        {granted.map((p) => (
                          <Badge
                            key={p.key}
                            variant={dangerousKeys.has(p.key) ? 'destructive' : 'outline'}
                            className="text-[10px]"
                          >
                            {t(`permissions.action.${p.action}` as const, {
                              defaultValue: p.action,
                            })}
                          </Badge>
                        ))}
                      </div>
                    </div>
                  );
                })}
                {role.permissions.length === 0 && (
                  <span className="text-xs text-muted-foreground italic">
                    {t('common.none')}
                  </span>
                )}
              </div>
            </div>
          )}
          {(role.allowed_models !== null || role.allowed_mcp_tools !== null) && (
            <div className="space-y-2">
              {role.allowed_models !== null && (
                <ConstraintRow
                  label={t('roles.allowedModels')}
                  items={role.allowed_models}
                  resolveLabel={(s) => s}
                />
              )}
              {role.allowed_mcp_tools !== null && (
                <ConstraintRow
                  label={t('roles.allowedMcpTools')}
                  items={role.allowed_mcp_tools}
                  resolveLabel={(id) =>
                    availableServers.find((s) => s.id === id)?.name ?? id.slice(0, 8)
                  }
                />
              )}
            </div>
          )}
          {/* Members — shared with the edit-wizard "Members" step. */}
          <RoleMembers role={role} teamsById={teamsById} onMembersChanged={onMembersChanged} />
        </TabsContent>
        <TabsContent value="history" className="py-2">
          {/* History lazy-fetches on mount. Since the parent keeps
              this tab unmounted until the user clicks it, the fetch
              still only fires on first reveal. */}
          {detailTab === 'history' && <RoleHistory roleId={role.id} />}
        </TabsContent>
      </Tabs>
    </>
  );
}

function ConstraintRow({
  label,
  items,
  resolveLabel,
}: {
  label: ReactNode;
  items: string[];
  resolveLabel: (s: string) => string;
}) {
  const { t } = useTranslation();
  return (
    <div>
      <div className="text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className="mt-1 flex flex-wrap gap-1">
        {items.length === 0 ? (
          <span className="text-xs italic text-muted-foreground">{t('common.none')}</span>
        ) : (
          items.map((s) => (
            <Badge key={s} variant="outline" className="text-[10px]">
              {resolveLabel(s)}
            </Badge>
          ))
        )}
      </div>
    </div>
  );
}
