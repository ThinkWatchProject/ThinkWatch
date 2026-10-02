import { Fragment, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Copy, Eye, FlaskConical, MoreHorizontal, Pencil, Plus, Power, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Switch } from '@/components/ui/switch';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Segmented } from '@/components/segmented';
import { cn } from '@/lib/utils';
import type { Guard, GuardDetail, GuardMode, SecurityRuleView } from '@/lib/security-types';
import { Code, Codepoints, MatcherText } from './matcher-text';
import {
  actionName,
  actionTone,
  guardName,
  kindName,
  modeDot,
  modeName,
  ruleWhy,
  viewName,
} from './names';
import { hasAction, patternOf, placeholder } from './policy';

const MODES: readonly GuardMode[] = ['off', 'observe', 'enforce'];

/** What the rule table can do. The page carries all of it out: each one writes or opens a dialog. */
export interface RuleActions {
  mode: (mode: GuardMode) => void;
  toggle: (r: SecurityRuleView, enabled: boolean) => void;
  /** A built-in rule: view it (and set its action); a custom rule: edit it. */
  open: (r: SecurityRuleView) => void;
  /** Copy a built-in tool or content rule as a custom rule. */
  copy: (r: SecurityRuleView) => void;
  remove: (r: SecurityRuleView) => void;
  create: () => void;
  test: () => void;
}

/**
 * One guard: its mode, with what the current mode does and what the third
 * mode would do, and below it every rule.
 */
export function GuardPanel({
  guard,
  detail,
  canWrite,
  canTest,
  actions,
}: {
  guard: Guard;
  detail: GuardDetail;
  /** Write permission for this guard: modes, switches, rule edits. */
  canWrite: boolean;
  /** Read permission for this guard: the test dialog. */
  canTest: boolean;
  actions: RuleActions;
}) {
  const { t } = useTranslation();
  const on = detail.rules.filter((r) => r.enabled).length;
  return (
    <div className="space-y-4">
      <ModeCard guard={guard} mode={detail.mode} canWrite={canWrite} onMode={actions.mode} />
      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-end justify-between gap-3">
            <div className="space-y-1">
              <CardTitle>{t('contentSecurity.rules')}</CardTitle>
              <p className="text-xs text-muted-foreground">
                {t('contentSecurity.ruleCount', { on, total: detail.rules.length })}
              </p>
            </div>
            <div className="flex gap-2">
              <Button variant="outline" size="sm" onClick={actions.test} disabled={!canTest}>
                <FlaskConical />
                {t('contentSecurity.test')}
              </Button>
              <Button size="sm" onClick={actions.create} disabled={!canWrite}>
                <Plus />
                {t('contentSecurity.newRule')}
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent>
          <RuleTable guard={guard} rules={detail.rules} canWrite={canWrite} actions={actions} />
        </CardContent>
      </Card>
    </div>
  );
}

/**
 * The mode: three joined buttons, then one line on what the current mode
 * does — the dot is the same colour as on the tab — and the cost of the
 * third mode, said before anyone switches to it.
 */
function ModeCard({
  guard,
  mode,
  canWrite,
  onMode,
}: {
  guard: Guard;
  mode: GuardMode;
  canWrite: boolean;
  onMode: (mode: GuardMode) => void;
}) {
  const { t } = useTranslation();
  const name = guardName(t, guard);
  const now =
    mode === 'off'
      ? t('contentSecurity.nowOff')
      : mode === 'observe'
        ? t(`contentSecurity.guard.${guard}.nowObserve`)
        : t(`contentSecurity.guard.${guard}.nowEnforce`);
  const risk = t(`contentSecurity.guard.${guard}.risk`);
  return (
    <Card>
      <CardContent className="space-y-3">
        <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
          <div className="min-w-0 flex-1 space-y-1">
            <h2 className="text-base font-medium">{name}</h2>
            <p className="text-sm text-muted-foreground">{t(`contentSecurity.guard.${guard}.lead`)}</p>
          </div>
          <Segmented
            label={t('contentSecurity.modeFor', { guard: name })}
            value={mode}
            options={MODES.map((m) => ({ value: m, label: modeName(t, guard, m) }))}
            onChange={onMode}
            disabled={!canWrite}
          />
        </div>
        <div className="space-y-1 border-t pt-3">
          <p className="flex items-start gap-2 text-sm">
            <span aria-hidden className={cn('mt-1.5 size-2 shrink-0 rounded-full', modeDot(mode))} />
            <span>{now}</span>
          </p>
          <p className="pl-4 text-xs text-muted-foreground">
            {mode === 'enforce'
              ? risk
              : t('contentSecurity.ifEnforced', {
                  mode: modeName(t, guard, 'enforce'),
                  effect: t(`contentSecurity.guard.${guard}.effect`),
                  risk,
                })}
          </p>
        </div>
      </CardContent>
    </Card>
  );
}

/** Splits rules into consecutive runs by key. The server's order is the display order. */
function groups(rules: SecurityRuleView[], key: (r: SecurityRuleView) => string) {
  const out: { key: string; rules: SecurityRuleView[] }[] = [];
  for (const r of rules) {
    const k = key(r);
    const last = out[out.length - 1];
    if (last && last.key === k) last.rules.push(r);
    else out.push({ key: k, rules: [r] });
  }
  return out;
}

/** Clicks on the switch and the menu are not clicks on the row. */
const stop = { onClick: (e: { stopPropagation: () => void }) => e.stopPropagation() };

/**
 * The rules, grouped: redaction by what the rule finds, tool calls built-in
 * then custom, content by kind (hidden characters, instruction override,
 * identity and prompts, Chinese phrasing) then custom.
 *
 * Columns: the rule, what it matches, what it does in the third mode (for
 * redaction: the placeholder a hit becomes), whether it is on. A row opens
 * the rule; the menu at its end has the rest.
 */
function RuleTable({
  guard,
  rules,
  canWrite,
  actions,
}: {
  guard: Guard;
  rules: SecurityRuleView[];
  canWrite: boolean;
  actions: RuleActions;
}) {
  const { t } = useTranslation();
  const groupKey = (r: SecurityRuleView) =>
    r.custom ? 'custom' : guard === 'inspect_tools' ? 'builtin' : r.kind;
  const groupTitle = (key: string) =>
    key === 'builtin' ? t('contentSecurity.builtinGroup') : kindName(t, key);

  return (
    <Table className="min-w-[720px] table-fixed">
      <colgroup>
        <col className="w-[34%]" />
        <col />
        <col className={guard === 'redact' ? 'w-[180px]' : 'w-[96px]'} />
        <col className="w-16" />
        <col className="w-12" />
      </colgroup>
      <TableHeader>
        <TableRow>
          <TableHead>{t('contentSecurity.col.rule')}</TableHead>
          <TableHead>
            {guard === 'inspect_tools' ? t('contentSecurity.col.regex') : t('contentSecurity.col.match')}
          </TableHead>
          <TableHead>
            {guard === 'redact' ? t('contentSecurity.col.replaceWith') : t('contentSecurity.col.action')}
          </TableHead>
          <TableHead>{t('contentSecurity.col.enabled')}</TableHead>
          <TableHead>
            <span className="sr-only">{t('common.actions')}</span>
          </TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {groups(rules, groupKey).map((g) => (
          <Fragment key={g.key}>
            <TableRow className="hover:bg-transparent">
              <TableCell colSpan={5} className="h-8 bg-muted/40 py-0 text-xs text-muted-foreground">
                <span className="font-medium text-foreground/80">{groupTitle(g.key)}</span>
                <span className="ml-2 tabular-nums">{g.rules.length}</span>
              </TableCell>
            </TableRow>
            {g.rules.map((r) => (
              <RuleRow
                key={`${r.custom ? 'c' : 'b'}:${r.id}`}
                guard={guard}
                r={r}
                canWrite={canWrite}
                actions={actions}
              />
            ))}
          </Fragment>
        ))}
      </TableBody>
    </Table>
  );
}

function MatchCell({ guard, r }: { guard: Guard; r: SecurityRuleView }) {
  const m = r.matcher;
  if (guard === 'redact') return <MatcherText m={m} />;
  // Tool and content rules show the pattern itself; that it ignores case
  // holds for every rule and is said in the rule's dialog.
  switch (m.kind) {
    case 'regex':
      return <Code>{m.pattern}</Code>;
    case 'contains':
      return <Code>{m.text}</Code>;
    case 'codepoints':
      return <Codepoints ranges={m.ranges} />;
    default:
      return <MatcherText m={m} />;
  }
}

function RuleRow({
  guard,
  r,
  canWrite,
  actions,
}: {
  guard: Guard;
  r: SecurityRuleView;
  canWrite: boolean;
  actions: RuleActions;
}) {
  const { t } = useTranslation();
  const name = viewName(t, guard, r);
  const why = ruleWhy(t, guard, r);
  const pattern = patternOf(r)?.pattern;
  const open = () => actions.open(r);
  const onKeyDown = (e: KeyboardEvent<HTMLTableRowElement>) => {
    if (e.target !== e.currentTarget) return;
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      open();
    }
  };
  // Built-in redaction rules can only be switched; tool and content rules
  // also take an action, so their dialog edits.
  const editable = canWrite && (r.custom || hasAction(guard));
  return (
    <TableRow
      tabIndex={0}
      className="cursor-pointer outline-none focus-visible:bg-muted/60"
      onClick={open}
      onKeyDown={onKeyDown}
    >
      <TableCell className="py-2">
        <div className={cn('truncate', !r.enabled && 'text-muted-foreground')}>{name}</div>
        {why && (
          <div className="truncate text-xs text-muted-foreground" title={why}>
            {why}
          </div>
        )}
      </TableCell>
      <TableCell className="truncate text-muted-foreground" title={pattern}>
        <MatchCell guard={guard} r={r} />
      </TableCell>
      <TableCell className="truncate">
        {guard === 'redact' ? (
          <span className="font-mono text-xs text-muted-foreground">{placeholder(r.label)}</span>
        ) : (
          <span className={actionTone(r.action)}>{actionName(t, r.action ?? 'record')}</span>
        )}
      </TableCell>
      <TableCell {...stop}>
        <Switch
          size="sm"
          checked={r.enabled}
          onCheckedChange={(v) => actions.toggle(r, v)}
          disabled={!canWrite}
          aria-label={t('contentSecurity.toggleFor', { name })}
        />
      </TableCell>
      <TableCell {...stop}>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" size="icon-sm" aria-label={t('contentSecurity.actionsFor', { name })}>
              <MoreHorizontal />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem onClick={open}>
              {editable ? <Pencil /> : <Eye />}
              {editable ? t('common.edit') : t('contentSecurity.menu.view')}
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => actions.toggle(r, !r.enabled)} disabled={!canWrite}>
              <Power />
              {r.enabled ? t('contentSecurity.menu.turnOff') : t('contentSecurity.menu.turnOn')}
            </DropdownMenuItem>
            {!r.custom && hasAction(guard) && pattern !== undefined && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuItem onClick={() => actions.copy(r)} disabled={!canWrite}>
                  <Copy />
                  {t('contentSecurity.menu.copyAsCustom')}
                </DropdownMenuItem>
              </>
            )}
            {r.custom && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  variant="destructive"
                  onClick={() => actions.remove(r)}
                  disabled={!canWrite}
                >
                  <Trash2 />
                  {t('common.delete')}
                </DropdownMenuItem>
              </>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </TableCell>
    </TableRow>
  );
}
