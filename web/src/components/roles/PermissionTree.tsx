import * as React from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, ChevronDown } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Label } from '@/components/ui/label';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import { ScrollArea } from '@/components/ui/scroll-area';
import type { PermissionDef } from '@/routes/admin/roles/types';

export type McpToolsByServer = Map<
  string,
  { serverName: string; tools: { key: string; toolName: string }[] }
>;
export type ModelsByProvider = Map<string, { modelId: string; displayName: string }[]>;

interface PermissionTreeProps {
  grouped: Map<string, PermissionDef[]>;
  selected: Set<string>;
  onTogglePerm: (key: string) => void;
  onToggleGroup: (perms: PermissionDef[]) => void;
  onSelectAll: () => void;
  onClear: () => void;
  /** `null` = unrestricted; any Set = restrict to these model IDs. */
  models: Set<string> | null;
  onModelsChange: (next: Set<string> | null) => void;
  modelsByProvider: ModelsByProvider;
  /** `null` = unrestricted; any Set = restrict to these namespaced tools. */
  mcpTools: Set<string> | null;
  onMcpToolsChange: (next: Set<string> | null) => void;
  mcpToolsByServer: McpToolsByServer;
}

/**
 * Tree-style permission picker grouped by resource. Each resource has
 * a parent checkbox that toggles all actions. For `ai_gateway` and
 * `mcp_gateway` an inline popover lets the admin restrict the role
 * to specific models or MCP tools.
 */
export function PermissionTree({
  grouped,
  selected,
  onTogglePerm,
  onToggleGroup,
  onSelectAll,
  onClear,
  models,
  onModelsChange,
  modelsByProvider,
  mcpTools,
  onMcpToolsChange,
  mcpToolsByServer,
}: PermissionTreeProps) {
  const { t } = useTranslation();
  const groups = Array.from(grouped.entries());

  return (
    <div>
      <div className="flex items-center justify-between">
        <Label className="text-sm font-medium">{t('roles.permissions')}</Label>
        <div className="flex items-center gap-1">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-xs"
            onClick={onSelectAll}
          >
            {t('common.selectAll')}
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-6 px-2 text-xs"
            onClick={onClear}
          >
            {t('common.clearAll')}
          </Button>
        </div>
      </div>
      <ScrollArea className="mt-2 h-[22rem] rounded-md border">
        <div className="divide-y">
          {groups.map(([resource, perms]) => {
            const allOn = perms.every((p) => selected.has(p.key));
            const someOn = !allOn && perms.some((p) => selected.has(p.key));
            return (
              <div key={resource} className="px-3 py-2">
                <label className="flex cursor-pointer items-center gap-2 text-xs font-medium">
                  <Checkbox
                    checked={allOn}
                    data-state={someOn ? 'indeterminate' : allOn ? 'checked' : 'unchecked'}
                    onCheckedChange={() => onToggleGroup(perms)}
                  />
                  <span className="font-mono uppercase tracking-wider text-muted-foreground">
                    {t(`permissions.resource.${resource}` as const, { defaultValue: resource })}
                  </span>
                </label>
                <div className="mt-1.5 grid grid-cols-2 gap-x-4 gap-y-1 pl-6 lg:grid-cols-3">
                  {perms.map((p) => (
                    <label
                      key={p.key}
                      className="flex cursor-pointer items-center gap-1.5 text-xs"
                      title={p.key}
                    >
                      <Checkbox
                        checked={selected.has(p.key)}
                        onCheckedChange={() => onTogglePerm(p.key)}
                      />
                      <span className={p.dangerous ? 'text-destructive' : ''}>
                        {t(`permissions.action.${p.action}` as const, { defaultValue: p.action })}
                      </span>
                      {p.dangerous && (
                        <AlertTriangle
                          className="h-3 w-3 shrink-0 text-destructive"
                          aria-label={t('roles.dangerous')}
                        />
                      )}
                    </label>
                  ))}
                </div>

                {resource === 'ai_gateway' && (allOn || someOn) && (
                  <ScopeDropdown
                    label={t('roles.modelsLabel')}
                    selected={models}
                    onChange={onModelsChange}
                    modelsByProvider={modelsByProvider}
                  />
                )}
                {resource === 'mcp_gateway' && (allOn || someOn) && (
                  <ToolScopeDropdown
                    label={t('roles.mcpToolsLabel')}
                    selected={mcpTools}
                    onChange={onMcpToolsChange}
                    mcpToolsByServer={mcpToolsByServer}
                  />
                )}
              </div>
            );
          })}
        </div>
      </ScrollArea>
    </div>
  );
}

function ScopeDropdown({
  label,
  selected,
  onChange,
  modelsByProvider,
}: {
  label: string;
  selected: Set<string> | null;
  onChange: (next: Set<string> | null) => void;
  modelsByProvider: ModelsByProvider;
}) {
  const { t } = useTranslation();
  const id = React.useId();
  const triggerText =
    selected === null ? t('roles.unrestricted') : `${selected.size} ${t('roles.selected')}`;

  const toggleModel = (modelId: string) => {
    const next = new Set(selected ?? []);
    if (next.has(modelId)) next.delete(modelId);
    else next.add(modelId);
    onChange(next);
  };
  const toggleProvider = (ms: { modelId: string }[]) => {
    const next = new Set(selected ?? []);
    const allOn = ms.every((m) => next.has(m.modelId));
    if (allOn) for (const m of ms) next.delete(m.modelId);
    else for (const m of ms) next.add(m.modelId);
    onChange(next);
  };

  return (
    <div className="mt-2 flex items-center gap-2 pl-6 text-xs">
      <span className="text-muted-foreground">{label}:</span>
      <Popover>
        <PopoverTrigger asChild>
          <Button type="button" variant="outline" size="sm" className="h-6 gap-1 text-xs">
            {triggerText}
            <ChevronDown className="h-3 w-3 opacity-60" />
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-[22rem] p-3">
          <RadioGroup
            value={selected === null ? 'unrestricted' : 'custom'}
            onValueChange={(v) => {
              if (v === 'unrestricted') onChange(null);
              else onChange(selected ?? new Set());
            }}
            className="mb-3 flex items-center gap-4 border-b pb-3"
          >
            <div className="flex items-center gap-1.5">
              <RadioGroupItem id={`${id}-unrestricted`} value="unrestricted" />
              <Label htmlFor={`${id}-unrestricted`} className="cursor-pointer text-xs">
                {t('roles.unrestricted')}
              </Label>
            </div>
            <div className="flex items-center gap-1.5">
              <RadioGroupItem id={`${id}-custom`} value="custom" />
              <Label htmlFor={`${id}-custom`} className="cursor-pointer text-xs">
                {t('roles.custom')}
              </Label>
            </div>
          </RadioGroup>
          {selected !== null && (
            <ScrollArea className="max-h-64">
              <div className="space-y-2 pr-2">
                {modelsByProvider.size === 0 ? (
                  <p className="px-1 text-xs italic text-muted-foreground">{t('common.noData')}</p>
                ) : (
                  Array.from(modelsByProvider.entries()).map(([provider, ms]) => {
                    const checkedCount = ms.filter((m) => selected.has(m.modelId)).length;
                    const allOn = checkedCount === ms.length;
                    const someOn = checkedCount > 0 && !allOn;
                    return (
                      <div key={provider}>
                        <label className="flex cursor-pointer items-center gap-2 text-xs font-medium">
                          <Checkbox
                            checked={allOn}
                            data-state={someOn ? 'indeterminate' : allOn ? 'checked' : 'unchecked'}
                            onCheckedChange={() => toggleProvider(ms)}
                          />
                          <span className="font-mono text-muted-foreground">{provider}</span>
                          <span className="text-[10px] font-normal text-muted-foreground">
                            ({checkedCount}/{ms.length})
                          </span>
                        </label>
                        <div className="mt-1 grid grid-cols-1 gap-x-4 gap-y-0.5 pl-6">
                          {ms.map((m) => (
                            <label
                              key={m.modelId}
                              className="flex cursor-pointer items-center gap-1.5 text-xs"
                              title={m.modelId}
                            >
                              <Checkbox
                                checked={selected.has(m.modelId)}
                                onCheckedChange={() => toggleModel(m.modelId)}
                              />
                              <span className="truncate">{m.displayName}</span>
                            </label>
                          ))}
                        </div>
                      </div>
                    );
                  })
                )}
              </div>
            </ScrollArea>
          )}
        </PopoverContent>
      </Popover>
    </div>
  );
}

function ToolScopeDropdown({
  label,
  selected,
  onChange,
  mcpToolsByServer,
}: {
  label: string;
  selected: Set<string> | null;
  onChange: (next: Set<string> | null) => void;
  mcpToolsByServer: McpToolsByServer;
}) {
  const { t } = useTranslation();
  const id = React.useId();
  const triggerText =
    selected === null ? t('roles.unrestricted') : `${selected.size} ${t('roles.selected')}`;

  const toggleTool = (key: string) => {
    const next = new Set(selected ?? []);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    onChange(next);
  };
  const toggleServer = (server: string, tools: { key: string }[]) => {
    const next = new Set(selected ?? []);
    const wildcard = `${server}__*`;
    const hasAll = next.has(wildcard) || tools.every((x) => next.has(x.key));
    if (hasAll) {
      next.delete(wildcard);
      for (const x of tools) next.delete(x.key);
    } else {
      next.add(wildcard);
      for (const x of tools) next.delete(x.key);
    }
    onChange(next);
  };

  return (
    <div className="mt-2 flex items-center gap-2 pl-6 text-xs">
      <span className="text-muted-foreground">{label}:</span>
      <Popover>
        <PopoverTrigger asChild>
          <Button type="button" variant="outline" size="sm" className="h-6 gap-1 text-xs">
            {triggerText}
            <ChevronDown className="h-3 w-3 opacity-60" />
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-[22rem] p-3">
          <RadioGroup
            value={selected === null ? 'unrestricted' : 'custom'}
            onValueChange={(v) => {
              if (v === 'unrestricted') onChange(null);
              else onChange(selected ?? new Set());
            }}
            className="mb-3 flex items-center gap-4 border-b pb-3"
          >
            <div className="flex items-center gap-1.5">
              <RadioGroupItem id={`${id}-unrestricted`} value="unrestricted" />
              <Label htmlFor={`${id}-unrestricted`} className="cursor-pointer text-xs">
                {t('roles.unrestricted')}
              </Label>
            </div>
            <div className="flex items-center gap-1.5">
              <RadioGroupItem id={`${id}-custom`} value="custom" />
              <Label htmlFor={`${id}-custom`} className="cursor-pointer text-xs">
                {t('roles.custom')}
              </Label>
            </div>
          </RadioGroup>
          {selected !== null && (
            <ScrollArea className="max-h-64">
              <div className="space-y-2 pr-2">
                {mcpToolsByServer.size === 0 ? (
                  <p className="px-1 text-xs italic text-muted-foreground">{t('common.noData')}</p>
                ) : (
                  Array.from(mcpToolsByServer.entries()).map(([server, group]) => {
                    const wildcard = `${server}__*`;
                    const hasWildcard = selected.has(wildcard);
                    const checkedCount = hasWildcard
                      ? group.tools.length
                      : group.tools.filter((x) => selected.has(x.key)).length;
                    const allOn = hasWildcard || checkedCount === group.tools.length;
                    const someOn = !allOn && checkedCount > 0;
                    return (
                      <div key={server}>
                        <label className="flex cursor-pointer items-center gap-2 text-xs font-medium">
                          <Checkbox
                            checked={allOn}
                            data-state={someOn ? 'indeterminate' : allOn ? 'checked' : 'unchecked'}
                            onCheckedChange={() => toggleServer(server, group.tools)}
                          />
                          <span className="font-mono text-muted-foreground">{server}</span>
                          <span className="text-[10px] font-normal text-muted-foreground">
                            {hasWildcard
                              ? `(${t('roles.allIncludingFuture')})`
                              : `(${checkedCount}/${group.tools.length})`}
                          </span>
                        </label>
                        {!hasWildcard && (
                          <div className="mt-1 grid grid-cols-1 gap-x-4 gap-y-0.5 pl-6">
                            {group.tools.map((x) => (
                              <label
                                key={x.key}
                                className="flex cursor-pointer items-center gap-1.5 text-xs"
                                title={x.key}
                              >
                                <Checkbox
                                  checked={selected.has(x.key)}
                                  onCheckedChange={() => toggleTool(x.key)}
                                />
                                <span className="truncate">{x.toolName}</span>
                              </label>
                            ))}
                          </div>
                        )}
                      </div>
                    );
                  })
                )}
              </div>
            </ScrollArea>
          )}
        </PopoverContent>
      </Popover>
    </div>
  );
}
