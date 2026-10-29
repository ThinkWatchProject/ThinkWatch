import { Fragment, useState, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Copy, Eye, FlaskConical, MoreHorizontal, Pencil, Plus, Power, Search, Trash2 } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader } from '@/components/ui/card';
import { Checkbox } from '@/components/ui/checkbox';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Input } from '@/components/ui/input';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
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
import type {
  Guard,
  GuardDetail,
  GuardMode,
  RuleAction,
  SecurityKindView,
  SecurityRuleView,
} from '@/lib/security-types';
import { About, Code, Codepoints, MatcherText } from './matcher-text';
import {
  actionName,
  actionTone,
  guardName,
  kindName,
  modeDot,
  modeName,
  ruleAbout,
  ruleWhy,
  scopeName,
  viewName,
} from './names';
import {
  ACTIONS,
  actionAllowed,
  enabledCount,
  groupState,
  matchesSearch,
  patternOf,
  placeholder,
  ruleScope,
} from './policy';

/** In the order the spec gives them: on, monitor, off. */
const MODES: readonly GuardMode[] = ['enforce', 'observe', 'off'];

/** What the rule table can do. The page carries all of it out: each one writes or opens a dialog. */
export interface RuleActions {
  mode: (mode: GuardMode) => void;
  toggle: (r: SecurityRuleView, enabled: boolean) => void;
  /** A group's checkbox: switch every rule shown in it. */
  toggleMany: (rules: SecurityRuleView[], enabled: boolean, group: string) => void;
  /** A rule's action, chosen in its row. */
  act: (r: SecurityRuleView, action: RuleAction) => void;
  /** A built-in rule: view it (and set its action); a custom rule: edit it. */
  open: (r: SecurityRuleView) => void;
  /** Copy a built-in rule that has a pattern as a custom rule. */
  copy: (r: SecurityRuleView) => void;
  remove: (r: SecurityRuleView) => void;
  create: () => void;
  test: () => void;
}

/**
 * One guard: its state (On, Monitor, Off) with one line on what that state
 * does and how many rules are on, then every rule, with a search.
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
  const [query, setQuery] = useState('');
  return (
    <div className="space-y-4">
      <ModeCard guard={guard} detail={detail} canWrite={canWrite} onMode={actions.mode} />
      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div className="relative w-64 max-w-full">
              <Search className="pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2 text-muted-foreground" />
              <Input
                type="search"
                className="h-7 pl-7 text-sm"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder={t('contentSecurity.search')}
                aria-label={t('contentSecurity.search')}
              />
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
          <RuleTable
            guard={guard}
            mode={detail.mode}
            rules={detail.rules}
            kinds={detail.kinds}
            query={query}
            canWrite={canWrite}
            actions={actions}
          />
        </CardContent>
      </Card>
    </div>
  );
}

/**
 * The state: three joined buttons, then one line on what the current state
 * does — the dot is the same colour as on the tab — and how many rules are
 * on.
 */
function ModeCard({
  guard,
  detail,
  canWrite,
  onMode,
}: {
  guard: Guard;
  detail: GuardDetail;
  canWrite: boolean;
  onMode: (mode: GuardMode) => void;
}) {
  const { t } = useTranslation();
  const name = guardName(t, guard);
  const mode = detail.mode;
  const now =
    mode === 'enforce' ? t(`contentSecurity.guard.${guard}.nowEnforce`) : t(`contentSecurity.now.${mode}`);
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
            options={MODES.map((m) => ({ value: m, label: modeName(t, m) }))}
            onChange={onMode}
            disabled={!canWrite}
          />
        </div>
        <p className="flex items-start gap-2 border-t pt-3 text-sm">
          <span aria-hidden className={cn('mt-1.5 size-2 shrink-0 rounded-full', modeDot(mode))} />
          <span>
            {now}
            <span className="ml-1 text-muted-foreground">
              {t('contentSecurity.ruleCount', { on: enabledCount(detail.rules), total: detail.rules.length })}
            </span>
          </span>
        </p>
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

/** Clicks on the switch, the action and the menu are not clicks on the row. */
const stop = { onClick: (e: { stopPropagation: () => void }) => e.stopPropagation() };

/**
 * The rules, grouped: redaction by what the rule finds, dangerous commands
 * built-in then custom, content by kind (hidden characters, instruction
 * override, identity and prompts, Chinese phrasing) then custom. Each
 * group's checkbox switches every rule shown in it; it is a shortcut, not
 * a switch of its own.
 *
 * Columns: whether it is on, the rule, what it matches, what it checks
 * (content), and its action, chosen in place. While the guard monitors, the
 * action stays as set and says that only recording happens for now. A row
 * opens the rule; the menu at its end has the rest.
 */
function RuleTable({
  guard,
  mode,
  rules,
  kinds,
  query,
  canWrite,
  actions,
}: {
  guard: Guard;
  mode: GuardMode;
  rules: SecurityRuleView[];
  kinds: SecurityKindView[];
  query: string;
  canWrite: boolean;
  actions: RuleActions;
}) {
  const { t, i18n } = useTranslation();
  const lang = i18n.language;
  const content = guard === 'content';
  const groupKey = (r: SecurityRuleView) => (r.custom ? 'custom' : r.kind);
  const groupTitle = (key: string) => kindName(lang, kinds, key);
  const shown = rules.filter((r) => matchesSearch(r, viewName(lang, r), query));
  const columns = content ? 6 : 5;

  return (
    <Table className="min-w-[760px] table-fixed">
      <colgroup>
        <col className="w-14" />
        <col className="w-[30%]" />
        <col />
        {content && <col className="w-[120px]" />}
        <col className="w-[200px]" />
        <col className="w-12" />
      </colgroup>
      <TableHeader>
        <TableRow>
          <TableHead>{t('contentSecurity.col.enabled')}</TableHead>
          <TableHead>{t('contentSecurity.col.rule')}</TableHead>
          <TableHead>{t('contentSecurity.col.match')}</TableHead>
          {content && <TableHead>{t('contentSecurity.col.scope')}</TableHead>}
          <TableHead>{t('contentSecurity.col.action')}</TableHead>
          <TableHead>
            <span className="sr-only">{t('common.actions')}</span>
          </TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {shown.length === 0 && (
          <TableRow className="hover:bg-transparent">
            <TableCell colSpan={columns} className="py-6 text-center text-sm text-muted-foreground">
              {t('contentSecurity.noMatch')}
            </TableCell>
          </TableRow>
        )}
        {groups(shown, groupKey).map((g) => {
          const title = groupTitle(g.key);
          return (
            <Fragment key={g.key}>
              <TableRow className="hover:bg-transparent">
                <TableCell className="h-8 bg-muted/40 py-0">
                  <Checkbox
                    checked={groupState(g.rules)}
                    onCheckedChange={() => actions.toggleMany(g.rules, groupState(g.rules) !== true, title)}
                    disabled={!canWrite}
                    aria-label={t('contentSecurity.groupToggle', { name: title })}
                  />
                </TableCell>
                <TableCell colSpan={columns - 1} className="h-8 bg-muted/40 py-0 text-xs text-muted-foreground">
                  <span className="font-medium text-foreground/80">{title}</span>
                  <span className="ml-2 tabular-nums">
                    {t('contentSecurity.groupCount', { on: enabledCount(g.rules), total: g.rules.length })}
                  </span>
                </TableCell>
              </TableRow>
              {g.rules.map((r) => (
                <RuleRow
                  key={`${r.custom ? 'c' : 'b'}:${r.id}`}
                  guard={guard}
                  mode={mode}
                  r={r}
                  canWrite={canWrite}
                  actions={actions}
                />
              ))}
            </Fragment>
          );
        })}
      </TableBody>
    </Table>
  );
}

function MatchCell({ guard, r }: { guard: Guard; r: SecurityRuleView }) {
  const { i18n } = useTranslation();
  const m = r.matcher;
  // A built-in redaction rule says what it finds in its own words.
  const about = ruleAbout(i18n.language, r);
  if (about) return <About text={about} />;
  if (guard === 'redact') return <MatcherText m={m} />;
  // Tool and content rules show the pattern itself (how it treats case is
  // said in the rule's dialog); a check implemented in code says what it
  // looks for.
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

/** The action a rule takes, chosen in place; plain text without the permission to change it. */
function ActionCell({
  guard,
  mode,
  r,
  canWrite,
  onAct,
}: {
  guard: Guard;
  mode: GuardMode;
  r: SecurityRuleView;
  canWrite: boolean;
  onAct: (action: RuleAction) => void;
}) {
  const { t, i18n } = useTranslation();
  const name = viewName(i18n.language, r);
  const action = r.action ?? 'record';
  const scope = ruleScope(r);
  const label = (a: RuleAction) =>
    a === 'replace' && r.custom
      ? t('contentSecurity.replaceWith', { text: r.replacement ?? '***' })
      : actionName(t, a);
  return (
    <div className="flex min-w-0 items-center gap-2">
      {canWrite ? (
        <Select value={action} onValueChange={(v) => onAct(v as RuleAction)}>
          <SelectTrigger
            size="sm"
            className={cn('h-7 w-auto min-w-0 max-w-[140px] px-2 text-xs', actionTone(action))}
            aria-label={t('contentSecurity.actionFor', { name })}
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {ACTIONS[guard].map((a) => (
              <SelectItem key={a} value={a} disabled={!actionAllowed(a, scope)} className="text-xs">
                {label(a)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : (
        <span className={cn('truncate text-sm', actionTone(action))}>{label(action)}</span>
      )}
      {mode === 'observe' && r.enabled && (
        <span className="shrink-0 text-xs text-muted-foreground">{t('contentSecurity.monitoring')}</span>
      )}
    </div>
  );
}

function RuleRow({
  guard,
  mode,
  r,
  canWrite,
  actions,
}: {
  guard: Guard;
  mode: GuardMode;
  r: SecurityRuleView;
  canWrite: boolean;
  actions: RuleActions;
}) {
  const { t, i18n } = useTranslation();
  const name = viewName(i18n.language, r);
  const why = ruleWhy(i18n.language, r);
  const pattern = patternOf(r)?.pattern;
  const about = ruleAbout(i18n.language, r).replaceAll('`', '');
  const open = () => actions.open(r);
  const onKeyDown = (e: KeyboardEvent<HTMLTableRowElement>) => {
    if (e.target !== e.currentTarget) return;
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      open();
    }
  };
  return (
    <TableRow
      tabIndex={0}
      className="cursor-pointer outline-none focus-visible:bg-muted/60"
      onClick={open}
      onKeyDown={onKeyDown}
    >
      <TableCell {...stop}>
        <Switch
          size="sm"
          checked={r.enabled}
          onCheckedChange={(v) => actions.toggle(r, v)}
          disabled={!canWrite}
          aria-label={t('contentSecurity.toggleFor', { name })}
        />
      </TableCell>
      <TableCell className="py-2">
        <div className={cn('truncate', !r.enabled && 'text-muted-foreground')}>{name}</div>
        {guard === 'redact' ? (
          <div className="truncate text-xs text-muted-foreground">
            {t('contentSecurity.placeholderIs')} <span className="font-mono">{placeholder(r.label)}</span>
          </div>
        ) : (
          why && (
            <div className="truncate text-xs text-muted-foreground" title={why}>
              {why}
            </div>
          )
        )}
      </TableCell>
      <TableCell className="truncate text-muted-foreground" title={pattern ?? (about || undefined)}>
        <MatchCell guard={guard} r={r} />
      </TableCell>
      {guard === 'content' && (
        <TableCell>
          <div className="flex flex-wrap gap-1">
            {ruleScope(r).map((s) => (
              <Badge key={s} variant="outline" className="font-normal">
                {scopeName(t, s)}
              </Badge>
            ))}
          </div>
        </TableCell>
      )}
      <TableCell {...stop}>
        <ActionCell guard={guard} mode={mode} r={r} canWrite={canWrite} onAct={(a) => actions.act(r, a)} />
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
              {canWrite ? <Pencil /> : <Eye />}
              {canWrite ? t('common.edit') : t('contentSecurity.menu.view')}
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => actions.toggle(r, !r.enabled)} disabled={!canWrite}>
              <Power />
              {r.enabled ? t('contentSecurity.menu.turnOff') : t('contentSecurity.menu.turnOn')}
            </DropdownMenuItem>
            {!r.custom && pattern !== undefined && (
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
