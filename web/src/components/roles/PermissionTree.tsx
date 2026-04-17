import * as React from 'react';
import { useTranslation } from 'react-i18next';
import { AlertTriangle, ChevronDown, X } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { ScrollArea } from '@/components/ui/scroll-area';
import type { PermissionDef } from '@/routes/admin/roles/types';

/** Compile a glob-style pattern (`*` wildcard) to a RegExp. Escapes
 *  all other regex metacharacters so literal dots / plus / brackets
 *  in model IDs aren't treated as regex syntax. */
function globToRegExp(pattern: string): RegExp {
  const escaped = pattern.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*');
  return new RegExp(`^${escaped}$`);
}

/** Does any wildcard pattern in `patterns` match `id`? Exact matches
 *  are handled by the checkbox state — this is only for showing the
 *  "covered by openai/*" indicator on models the user did not click
 *  individually. */
function coveringPattern(id: string, patterns: Iterable<string>): string | null {
  for (const p of patterns) {
    if (!p.includes('*')) continue;
    if (p === id) continue;
    try {
      if (globToRegExp(p).test(id)) return p;
    } catch {
      // malformed regex — ignore
    }
  }
  return null;
}

export type McpToolsByServer = Map<
  string,
  {
    serverName: string;
    /** `namespace_prefix` of the server — used for the `<prefix>__*`
     *  wildcard so "select all" matches the same keys as individual
     *  tool checkboxes (their key is `<prefix>__<tool_name>`). */
    prefix: string;
    tools: { key: string; toolName: string }[];
  }
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
  /** Slot rendered inside each resource group's body after the perm
   *  checkbox list and the scope dropdown. Used to inline surface-level
   *  constraints (rate limits, budgets) directly under the `ai_gateway`
   *  and `mcp_gateway` groups. */
  renderGroupExtra?: (groupKey: string) => React.ReactNode;
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
  renderGroupExtra,
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

                {(((resource === 'ai_gateway' || resource === 'mcp_gateway') &&
                  (allOn || someOn)) ||
                  renderGroupExtra) && (
                  <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-2 pl-6 text-xs">
                    {renderGroupExtra?.(resource)}
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
  const [query, setQuery] = React.useState('');
  const [open, setOpen] = React.useState(false);

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
  const commitQuery = () => {
    const v = query.trim();
    if (!v) return;
    const next = new Set(selected ?? []);
    next.add(v);
    onChange(next);
    setQuery('');
  };
  const removeItem = (item: string) => {
    const next = new Set(selected ?? []);
    next.delete(item);
    onChange(next);
  };

  // Single input doubles as search AND pattern entry. Typing filters
  // the list below; pressing Enter commits the text as-is (wildcard
  // or exact id), which is what the admin wants when the list doesn't
  // contain the thing they're looking for.
  const queryTrim = query.trim();
  const queryLower = queryTrim.toLowerCase();
  const filteredProviders = React.useMemo(() => {
    if (!queryLower) return Array.from(modelsByProvider.entries());
    return Array.from(modelsByProvider.entries())
      .map(([p, ms]) => {
        // `p` can be empty (callers that lost the provider→model
        // relation collapse everything into one unlabeled bucket) or
        // undefined from bad data — guard both.
        const pLower = (p ?? '').toLowerCase();
        if (pLower && pLower.includes(queryLower)) return [p, ms] as const;
        const kept = ms.filter(
          (m) =>
            m.modelId.toLowerCase().includes(queryLower) ||
            m.displayName.toLowerCase().includes(queryLower),
        );
        return [p, kept] as const;
      })
      .filter(([, ms]) => ms.length > 0);
  }, [modelsByProvider, queryLower]);

  // Show the "add as pattern / exact" suggestion only when the typed
  // string isn't already selected and doesn't exactly match a known
  // model (that case gets handled by a checkbox instead).
  const allModelIds = React.useMemo(() => {
    const s = new Set<string>();
    for (const [, ms] of modelsByProvider) for (const m of ms) s.add(m.modelId);
    return s;
  }, [modelsByProvider]);
  const showAddHint =
    queryTrim.length > 0 &&
    !(selected?.has(queryTrim) ?? false) &&
    (queryTrim.includes('*') || !allModelIds.has(queryTrim));

  return (
    <>
      <span className="text-muted-foreground">{label}:</span>
      <Popover open={open} onOpenChange={setOpen}>
        <div className="flex items-center gap-1 rounded-md border p-0.5">
          <Button
            type="button"
            variant={selected === null ? 'secondary' : 'ghost'}
            size="sm"
            className="h-5 px-2 text-xs"
            onClick={() => {
              onChange(null);
              setOpen(false);
            }}
          >
            {t('roles.unrestricted')}
          </Button>
          <PopoverTrigger asChild>
            <Button
              type="button"
              variant={selected !== null ? 'secondary' : 'ghost'}
              size="sm"
              className="h-5 gap-1 px-2 text-xs"
              onClick={() => {
                // Selecting "Custom" from the unrestricted state should
                // flip the mode AND open the picker in one click.
                if (selected === null) {
                  onChange(new Set());
                  setOpen(true);
                }
              }}
            >
              {t('roles.custom')}
              {selected !== null && selected.size > 0 && (
                <span className="text-muted-foreground">({selected.size})</span>
              )}
              <ChevronDown className="h-3 w-3 opacity-60" />
            </Button>
          </PopoverTrigger>
        </div>
        <PopoverContent className="w-[42rem] p-0" align="start">
          {selected !== null && (
            <>
              {/* Selected chips — wildcards get the "default" (filled)
                  variant so rule-shaped entries stand out from exact
                  ids at a glance. Chip row is scrollable so picking 50
                  models doesn't push the search + list offscreen. */}
              {selected.size > 0 && (
                <div className="max-h-20 overflow-y-auto border-b px-2 py-1.5">
                  <div className="flex flex-wrap gap-1">
                    {Array.from(selected)
                      .sort()
                      .map((item) => (
                        <Badge
                          key={item}
                          variant={item.includes('*') ? 'default' : 'secondary'}
                          className="h-5 gap-0.5 pl-1.5 pr-0.5 font-mono text-[10px] font-normal"
                        >
                          {item}
                          <button
                            type="button"
                            className="rounded-sm p-0.5 opacity-60 hover:opacity-100"
                            onClick={() => removeItem(item)}
                            aria-label={t('common.remove')}
                          >
                            <X className="h-2.5 w-2.5" />
                          </button>
                        </Badge>
                      ))}
                  </div>
                </div>
              )}

              {/* Unified search + add input. Borderless to blend with
                  the popover; Enter commits whatever's typed. */}
              <div className="border-b px-2 py-1">
                <Input
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      commitQuery();
                    }
                  }}
                  placeholder={t('roles.modelPatternPlaceholder')}
                  className="h-7 border-0 bg-transparent px-0 font-mono text-xs shadow-none focus-visible:ring-0"
                />
              </div>

              <div className="max-h-64 overflow-y-auto">
                <div className="px-2 py-1.5">
                  {showAddHint && (
                    <button
                      type="button"
                      className="mb-1 flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-left text-xs hover:bg-muted"
                      onClick={commitQuery}
                    >
                      <span className="rounded bg-primary/10 px-1 text-[10px] text-primary">
                        {queryTrim.includes('*') ? t('roles.addPattern') : t('roles.addExact')}
                      </span>
                      <code className="truncate">{queryTrim}</code>
                      <kbd className="ml-auto rounded border bg-muted px-1 text-[9px] text-muted-foreground">
                        ↵
                      </kbd>
                    </button>
                  )}

                  {filteredProviders.length === 0 ? (
                    <p className="py-2 text-center text-xs italic text-muted-foreground">
                      {modelsByProvider.size === 0 ? t('common.noData') : t('common.noResults')}
                    </p>
                  ) : (
                    <div className="space-y-2">
                      {filteredProviders.map(([provider, ms], groupIdx) => {
                        const checkedCount = ms.filter((m) => selected.has(m.modelId)).length;
                        const allOn = checkedCount === ms.length;
                        const someOn = checkedCount > 0 && !allOn;
                        // When there's no provider label (one collapsed
                        // bucket), hide the group header and indent —
                        // the list looks flat, as if it never had a
                        // grouping to begin with.
                        const showGroupHeader = Boolean(provider);
                        return (
                          <div key={provider || `group-${groupIdx}`}>
                            {showGroupHeader && (
                              <label className="flex cursor-pointer items-center gap-1.5 text-xs font-medium">
                                <Checkbox
                                  checked={allOn}
                                  data-state={
                                    someOn ? 'indeterminate' : allOn ? 'checked' : 'unchecked'
                                  }
                                  onCheckedChange={() => toggleProvider(ms)}
                                />
                                <span className="font-mono text-muted-foreground">{provider}</span>
                                <span className="text-[10px] font-normal text-muted-foreground">
                                  {checkedCount}/{ms.length}
                                </span>
                              </label>
                            )}
                            <div
                              className={`grid grid-cols-3 gap-x-3 gap-y-0.5 ${
                                showGroupHeader ? 'mt-0.5 pl-5' : ''
                              }`}
                            >
                              {ms.map((m) => {
                                const covered = coveringPattern(m.modelId, selected);
                                const coveredOnly = covered !== null && !selected.has(m.modelId);
                                return (
                                  <label
                                    key={m.modelId}
                                    className="flex min-w-0 cursor-pointer items-center gap-1.5 text-xs"
                                    title={
                                      covered
                                        ? `${m.modelId} — ${t('roles.coveredBy')} ${covered}`
                                        : m.modelId
                                    }
                                  >
                                    <Checkbox
                                      checked={selected.has(m.modelId) || covered !== null}
                                      disabled={coveredOnly}
                                      onCheckedChange={() => toggleModel(m.modelId)}
                                    />
                                    <span
                                      className={`min-w-0 truncate ${
                                        coveredOnly ? 'italic text-muted-foreground' : ''
                                      }`}
                                    >
                                      {m.displayName}
                                    </span>
                                  </label>
                                );
                              })}
                            </div>
                          </div>
                        );
                      })}
                    </div>
                  )}
                </div>
              </div>
            </>
          )}
        </PopoverContent>
      </Popover>
    </>
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
  const [query, setQuery] = React.useState('');
  const [open, setOpen] = React.useState(false);

  const toggleTool = (key: string) => {
    const next = new Set(selected ?? []);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    onChange(next);
  };
  /// Toggle the per-server wildcard `<prefix>__*`. Mirrors the model
  /// picker's "provider" checkbox: turning on the wildcard wipes any
  /// individual keys (they're redundant once the wildcard is in).
  const toggleServer = (prefix: string, tools: { key: string }[]) => {
    const next = new Set(selected ?? []);
    const wildcard = `${prefix}__*`;
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
  const commitQuery = () => {
    const v = query.trim();
    if (!v) return;
    const next = new Set(selected ?? []);
    next.add(v);
    onChange(next);
    setQuery('');
  };
  const removeItem = (item: string) => {
    const next = new Set(selected ?? []);
    next.delete(item);
    onChange(next);
  };

  const queryTrim = query.trim();
  const queryLower = queryTrim.toLowerCase();

  // Filter servers + tools by the typed query (server name, tool
  // display name, or namespaced tool key).
  const filteredServers = React.useMemo(() => {
    if (!queryLower) return Array.from(mcpToolsByServer.entries());
    return Array.from(mcpToolsByServer.entries())
      .map(([server, group]) => {
        const serverHit = server.toLowerCase().includes(queryLower);
        if (serverHit) return [server, group] as const;
        const keptTools = group.tools.filter(
          (x) =>
            x.key.toLowerCase().includes(queryLower) ||
            x.toolName.toLowerCase().includes(queryLower),
        );
        return [server, { ...group, tools: keptTools }] as const;
      })
      .filter(([, group]) => group.tools.length > 0);
  }, [mcpToolsByServer, queryLower]);

  // Known keys let us hide the "add exact" hint when the user typed
  // something that already matches a tool (they should click instead).
  const allKeys = React.useMemo(() => {
    const s = new Set<string>();
    for (const [, group] of mcpToolsByServer) {
      s.add(`${group.prefix}__*`);
      for (const x of group.tools) s.add(x.key);
    }
    return s;
  }, [mcpToolsByServer]);
  const showAddHint =
    queryTrim.length > 0 &&
    !(selected?.has(queryTrim) ?? false) &&
    (queryTrim.includes('*') || !allKeys.has(queryTrim));

  return (
    <>
      <span className="text-muted-foreground">{label}:</span>
      <Popover open={open} onOpenChange={setOpen}>
        <div className="flex items-center gap-1 rounded-md border p-0.5">
          <Button
            type="button"
            variant={selected === null ? 'secondary' : 'ghost'}
            size="sm"
            className="h-5 px-2 text-xs"
            onClick={() => {
              onChange(null);
              setOpen(false);
            }}
          >
            {t('roles.unrestricted')}
          </Button>
          <PopoverTrigger asChild>
            <Button
              type="button"
              variant={selected !== null ? 'secondary' : 'ghost'}
              size="sm"
              className="h-5 gap-1 px-2 text-xs"
              onClick={() => {
                if (selected === null) {
                  onChange(new Set());
                  setOpen(true);
                }
              }}
            >
              {t('roles.custom')}
              {selected !== null && selected.size > 0 && (
                <span className="text-muted-foreground">({selected.size})</span>
              )}
              <ChevronDown className="h-3 w-3 opacity-60" />
            </Button>
          </PopoverTrigger>
        </div>
        <PopoverContent className="w-[42rem] p-0" align="start">
          {selected !== null && (
            <>
              {selected.size > 0 && (
                <div className="max-h-20 overflow-y-auto border-b px-2 py-1.5">
                  <div className="flex flex-wrap gap-1">
                    {Array.from(selected)
                      .sort()
                      .map((item) => (
                        <Badge
                          key={item}
                          variant={item.endsWith('__*') ? 'default' : 'secondary'}
                          className="h-5 gap-0.5 pl-1.5 pr-0.5 font-mono text-[10px] font-normal"
                        >
                          {item}
                          <button
                            type="button"
                            className="rounded-sm p-0.5 opacity-60 hover:opacity-100"
                            onClick={() => removeItem(item)}
                            aria-label={t('common.remove')}
                          >
                            <X className="h-2.5 w-2.5" />
                          </button>
                        </Badge>
                      ))}
                  </div>
                </div>
              )}

              <div className="border-b px-2 py-1">
                <Input
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      e.preventDefault();
                      commitQuery();
                    }
                  }}
                  placeholder={t('roles.toolPatternPlaceholder')}
                  className="h-7 border-0 bg-transparent px-0 font-mono text-xs shadow-none focus-visible:ring-0"
                />
              </div>

              <div className="max-h-64 overflow-y-auto">
                <div className="px-2 py-1.5">
                  {showAddHint && (
                    <button
                      type="button"
                      className="mb-1 flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-left text-xs hover:bg-muted"
                      onClick={commitQuery}
                    >
                      <span className="rounded bg-primary/10 px-1 text-[10px] text-primary">
                        {queryTrim.includes('*') ? t('roles.addPattern') : t('roles.addExact')}
                      </span>
                      <code className="truncate">{queryTrim}</code>
                      <kbd className="ml-auto rounded border bg-muted px-1 text-[9px] text-muted-foreground">
                        ↵
                      </kbd>
                    </button>
                  )}

                  {filteredServers.length === 0 ? (
                    <p className="py-2 text-center text-xs italic text-muted-foreground">
                      {mcpToolsByServer.size === 0 ? t('common.noData') : t('common.noResults')}
                    </p>
                  ) : (
                    <div className="space-y-2">
                      {filteredServers.map(([server, group]) => {
                        const wildcard = `${group.prefix}__*`;
                        const hasWildcard = selected.has(wildcard);
                        const checkedCount = hasWildcard
                          ? group.tools.length
                          : group.tools.filter((x) => selected.has(x.key)).length;
                        const allOn = hasWildcard || checkedCount === group.tools.length;
                        const someOn = !allOn && checkedCount > 0;
                        return (
                          <div key={server}>
                            <label className="flex cursor-pointer items-center gap-1.5 text-xs font-medium">
                              <Checkbox
                                checked={allOn}
                                data-state={
                                  someOn ? 'indeterminate' : allOn ? 'checked' : 'unchecked'
                                }
                                onCheckedChange={() => toggleServer(group.prefix, group.tools)}
                              />
                              <span className="font-mono text-muted-foreground">{server}</span>
                              <span className="text-[10px] font-normal text-muted-foreground">
                                {hasWildcard
                                  ? t('roles.allIncludingFuture')
                                  : `${checkedCount}/${group.tools.length}`}
                              </span>
                            </label>
                            <div className="mt-0.5 grid grid-cols-3 gap-x-3 gap-y-0.5 pl-5">
                              {group.tools.map((x) => {
                                const coveredByWildcard = hasWildcard && !selected.has(x.key);
                                return (
                                  <label
                                    key={x.key}
                                    className="flex min-w-0 cursor-pointer items-center gap-1.5 text-xs"
                                    title={
                                      coveredByWildcard
                                        ? `${x.key} — ${t('roles.coveredBy')} ${wildcard}`
                                        : x.key
                                    }
                                  >
                                    <Checkbox
                                      checked={selected.has(x.key) || hasWildcard}
                                      disabled={coveredByWildcard}
                                      onCheckedChange={() => toggleTool(x.key)}
                                    />
                                    <span
                                      className={`min-w-0 truncate ${
                                        coveredByWildcard ? 'italic text-muted-foreground' : ''
                                      }`}
                                    >
                                      {x.toolName}
                                    </span>
                                  </label>
                                );
                              })}
                            </div>
                          </div>
                        );
                      })}
                    </div>
                  )}
                </div>
              </div>
            </>
          )}
        </PopoverContent>
      </Popover>
    </>
  );
}
