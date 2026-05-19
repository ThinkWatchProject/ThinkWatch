import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { TableCell, TableRow } from '@/components/ui/table';
import { Pencil, Trash2 } from 'lucide-react';
import { hasPermission } from '@/lib/api';
import { type ModelRow, modelStatus } from './types';

/// One row of the Models table. Lifted out of the route so the table
/// body's per-item render stays a one-liner and the cell's
/// click-target logic (open drawer vs toggle selection vs action
/// button) lives next to the markup it modifies.
export function ModelRowCell({
  model,
  selected,
  onToggleSelect,
  onOpen,
  onDelete,
}: {
  model: ModelRow;
  selected: boolean;
  onToggleSelect: () => void;
  onOpen: () => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const status = modelStatus(model);
  return (
    <TableRow
      className="cursor-pointer hover:bg-muted/30"
      data-state={selected ? 'selected' : undefined}
      onClick={(e) => {
        const target = e.target as HTMLElement;
        // Don't open the drawer when the user is interacting with row
        // controls (action buttons or the select checkbox).
        if (target.closest('button')) return;
        if (target.closest('[role="checkbox"]')) return;
        onOpen();
      }}
    >
      <TableCell
        className="w-10"
        onClick={(e) => {
          // Click anywhere in the cell toggles selection — gives a
          // generous hit target without making the whole row a no-op
          // for the drawer.
          e.stopPropagation();
          onToggleSelect();
        }}
      >
        <Checkbox
          checked={selected}
          onCheckedChange={onToggleSelect}
          aria-label={t('models.selectAll')}
        />
      </TableCell>
      <TableCell className="font-mono text-xs max-w-[260px] truncate" title={model.model_id}>
        {model.model_id}
      </TableCell>
      <TableCell className="text-sm">{model.display_name}</TableCell>
      <TableCell className="text-center">
        {status === 'active' ? (
          <Badge variant="default">{t('models.status.active')}</Badge>
        ) : status === 'disabled' ? (
          <Badge
            variant="outline"
            className="border-amber-500/60 text-amber-600 dark:text-amber-400"
          >
            {t('models.status.disabled')}
          </Badge>
        ) : (
          <Badge variant="outline" className="text-muted-foreground">
            {t('models.status.unrouted')}
          </Badge>
        )}
      </TableCell>
      <TableCell className="text-right font-mono text-xs tabular-nums">
        {model.enabled_route_count}
        {model.route_count > model.enabled_route_count && (
          <span className="text-muted-foreground">/{model.route_count}</span>
        )}
      </TableCell>
      <TableCell className="max-w-[260px]">
        {model.providers.length > 0 ? (
          <div className="flex flex-wrap gap-1">
            {model.providers.slice(0, 3).map((name, i) => (
              <Badge key={i} variant="secondary" className="text-[10px] font-normal">
                {name}
              </Badge>
            ))}
            {model.providers.length > 3 && (
              <span className="text-[10px] text-muted-foreground">
                +{model.providers.length - 3}
              </span>
            )}
          </div>
        ) : (
          <span className="text-xs italic text-muted-foreground">—</span>
        )}
      </TableCell>
      <TableCell className="text-right whitespace-nowrap">
        <Button
          variant="ghost"
          size="icon"
          onClick={onOpen}
          aria-label={t('common.edit')}
          disabled={!hasPermission('models:write')}
        >
          <Pencil className="h-4 w-4" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          onClick={onDelete}
          aria-label={t('common.delete')}
          disabled={!hasPermission('models:write')}
        >
          <Trash2 className="h-4 w-4 text-destructive" />
        </Button>
      </TableCell>
    </TableRow>
  );
}
