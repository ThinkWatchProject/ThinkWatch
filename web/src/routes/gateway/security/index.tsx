import { useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { AlertCircle } from 'lucide-react';
import { toast } from 'sonner';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { api, apiPatch, hasPermission } from '@/lib/api';
import { cn } from '@/lib/utils';
import {
  GUARDS,
  SETTING_KEYS,
  type Guard,
  type GuardDetail,
  type GuardMode,
  type RuleAction,
  type SecurityDetail,
  type SecurityRuleView,
} from '@/lib/security-types';
import { GuardPanel, type RuleActions } from './guard-panel';
import { LogPanel } from './log-panel';
import { actionName, guardName, modeDot, modeName, viewName } from './names';
import {
  customRuleView,
  patternOf,
  policyOf,
  withAction,
  withCustom,
  withEnabled,
  withEnabledMany,
  withMode,
  withoutCustom,
  type CustomRuleInput,
} from './policy';
import { BuiltinRuleDialog, CustomRuleDialog, TestDialog, type RuleSeed } from './rule-dialogs';

const SECURITY_KEY = ['admin', 'security'] as const;

/** Permission names stay as they were: `pii_redactor:*` for redaction, `content_filter:*` for the rest. */
const WRITE_PERMISSION: Record<Guard, string> = {
  redact: 'pii_redactor:write',
  inspect_tools: 'content_filter:write',
  content: 'content_filter:write',
};
const READ_PERMISSION: Record<Guard, string> = {
  redact: 'pii_redactor:read',
  inspect_tools: 'content_filter:read',
  content: 'content_filter:read',
};

/** A policy is saved through the settings endpoint: that takes `settings:write` as well as the guard's own. */
const mayWrite = (g: Guard) => hasPermission('settings:write') && hasPermission(WRITE_PERMISSION[g]);

/** The log is read from the audit log, which takes `logs:read_all`. */
const mayReadLog = () => hasPermission('logs:read_all');

type Tab = 'log' | Guard;

type DialogState =
  | null
  | { kind: 'custom'; guard: Guard; editing: SecurityRuleView | null; seed?: RuleSeed }
  | { kind: 'builtin'; guard: Guard; rule: SecurityRuleView }
  | { kind: 'test'; guard: Guard }
  | { kind: 'delete'; guard: Guard; rule: SecurityRuleView };

function message(err: unknown, fallback: string): string {
  return err instanceof Error ? err.message : fallback;
}

/**
 * The security page: a log of what the guards found, then one tab per
 * guard — secrets and privacy (outbound redaction), dangerous commands
 * (tool-call inspection) and the content filter, which checks requests and
 * the text of answers. All three apply to every request through the AI
 * gateway, whatever the key, model or provider.
 *
 * Each guard is On, Monitor or Off, and has a rule table: built-in rules,
 * grouped, that can be switched one by one or a group at a time, and
 * custom rules; every rule has an action of its own, which applies while
 * the guard is on. A monitoring guard only records, whatever the actions
 * say. Every hit is an audit event, so the log forwarders send it on.
 *
 * **Every change is written at once, as the guard's whole policy object**
 * under its `security.*` settings key, built from the view on screen.
 * Writes run one after another so each carries the ones before it; a mode
 * or switch change shows first and offers an undo. Once nothing is in
 * flight the view is read back from the server.
 */
export function GatewaySecurityPage() {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<Tab>(() => (mayReadLog() ? 'log' : 'redact'));
  const [dialog, setDialog] = useState<DialogState>(null);
  const [deleting, setDeleting] = useState(false);

  const detailQuery = useQuery({
    queryKey: SECURITY_KEY,
    queryFn: ({ signal }) => api<SecurityDetail>('/api/admin/security', { signal }),
  });
  const detail = detailQuery.data;

  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const inFlight = useRef(0);

  /** The newest view of one guard, including changes still being written. */
  const current = (guard: Guard): GuardDetail | undefined =>
    queryClient.getQueryData<SecurityDetail>(SECURITY_KEY)?.[guard];

  const show = (guard: Guard, next: GuardDetail) => {
    void queryClient.cancelQueries({ queryKey: SECURITY_KEY, exact: true });
    queryClient.setQueryData<SecurityDetail>(SECURITY_KEY, (d) => (d ? { ...d, [guard]: next } : d));
  };

  /**
   * Writes `next` as the guard's policy, after any write still in flight.
   * `optimistic` shows it before the server answers; otherwise it shows once
   * written (dialogs, which report a refusal in place).
   */
  const write = async (guard: Guard, next: GuardDetail, optimistic: boolean): Promise<void> => {
    if (optimistic) show(guard, next);
    const settings = { [SETTING_KEYS[guard]]: policyOf(guard, next) };
    inFlight.current += 1;
    const run = queue.current.then(() => apiPatch('/api/admin/settings', { settings }));
    queue.current = run.catch(() => undefined);
    try {
      await run;
      if (!optimistic) show(guard, next);
    } finally {
      inFlight.current -= 1;
      if (inFlight.current === 0) void queryClient.invalidateQueries({ queryKey: SECURITY_KEY });
    }
  };

  const failed = (err: unknown) =>
    toast.error(t('contentSecurity.toast.saveFailed', { message: message(err, t('common.error')) }));

  function setMode(guard: Guard, mode: GuardMode, undoable = true) {
    const d = current(guard);
    if (!d || d.mode === mode) return;
    const before = d.mode;
    write(guard, withMode(d, mode), true).then(
      () =>
        toast.success(
          t('contentSecurity.toast.modeSet', { guard: guardName(t, guard), mode: modeName(t, mode) }),
          undoable
            ? { action: { label: t('contentSecurity.toast.undo'), onClick: () => setMode(guard, before, false) } }
            : undefined,
        ),
      failed,
    );
  }

  function toggle(guard: Guard, rule: SecurityRuleView, enabled: boolean, undoable = true) {
    const d = current(guard);
    if (!d) return;
    const name = viewName(i18n.language, rule);
    write(guard, withEnabled(d, rule, enabled), true).then(
      () =>
        toast.success(
          enabled ? t('contentSecurity.toast.ruleOn', { name }) : t('contentSecurity.toast.ruleOff', { name }),
          undoable
            ? { action: { label: t('contentSecurity.toast.undo'), onClick: () => toggle(guard, rule, !enabled, false) } }
            : undefined,
        ),
      failed,
    );
  }

  /** A group's checkbox: every rule shown in it, in one write. */
  function toggleMany(guard: Guard, rules: SecurityRuleView[], enabled: boolean, group: string, undoable = true) {
    const d = current(guard);
    if (!d || rules.length === 0) return;
    const before = rules.filter((r) => r.enabled !== enabled);
    write(guard, withEnabledMany(d, rules, enabled), true).then(
      () =>
        toast.success(
          enabled
            ? t('contentSecurity.toast.groupOn', { name: group, count: rules.length })
            : t('contentSecurity.toast.groupOff', { name: group, count: rules.length }),
          undoable && before.length > 0
            ? {
                action: {
                  label: t('contentSecurity.toast.undo'),
                  onClick: () => toggleMany(guard, before, !enabled, group, false),
                },
              }
            : undefined,
        ),
      failed,
    );
  }

  /** A rule's action, chosen in its row. */
  function act(guard: Guard, rule: SecurityRuleView, action: RuleAction, undoable = true) {
    const d = current(guard);
    if (!d) return;
    const before = d.rules.find((r) => r.custom === rule.custom && r.id === rule.id)?.action ?? 'record';
    if (before === action) return;
    const name = viewName(i18n.language, rule);
    write(guard, withAction(d, rule, action), true).then(
      () =>
        toast.success(
          t('contentSecurity.toast.actionSet', { name, action: actionName(t, action) }),
          undoable
            ? { action: { label: t('contentSecurity.toast.undo'), onClick: () => act(guard, rule, before, false) } }
            : undefined,
        ),
      failed,
    );
  }

  /** Custom rule dialog: a refusal stays in the dialog, with the server's reason. */
  async function saveCustom(guard: Guard, editing: SecurityRuleView | null, input: CustomRuleInput) {
    const d = current(guard);
    if (!d) return;
    await write(guard, withCustom(d, editing?.id ?? null, customRuleView(input)), false);
    setDialog(null);
    toast.success(
      editing
        ? t('contentSecurity.toast.ruleSaved', { name: input.name })
        : t('contentSecurity.toast.ruleCreated', { name: input.name }),
    );
  }

  async function saveAction(guard: Guard, rule: SecurityRuleView, action: RuleAction) {
    const d = current(guard);
    if (!d) return;
    await write(guard, withAction(d, rule, action), false);
    setDialog(null);
    toast.success(t('contentSecurity.toast.ruleSaved', { name: viewName(i18n.language, rule) }));
  }

  async function removeCustom(guard: Guard, rule: SecurityRuleView) {
    const d = current(guard);
    if (!d) return;
    setDeleting(true);
    try {
      await write(guard, withoutCustom(d, rule.id), false);
      setDialog(null);
      toast.success(t('contentSecurity.toast.ruleDeleted', { name: rule.id }));
    } catch (err) {
      failed(err);
    } finally {
      setDeleting(false);
    }
  }

  /** "Copy as a custom rule": the same pattern and action, under a name to change. */
  function copyAsCustom(guard: Guard, rule: SecurityRuleView) {
    const written = patternOf(rule);
    if (!written) return;
    setDialog({
      kind: 'custom',
      guard,
      editing: null,
      seed: {
        name: viewName(i18n.language, rule),
        pattern: written.pattern,
        match: written.match,
        action: rule.action ?? null,
      },
    });
  }

  const actions = (guard: Guard): RuleActions => ({
    mode: (mode) => setMode(guard, mode),
    toggle: (r, enabled) => toggle(guard, r, enabled),
    toggleMany: (rules, enabled, group) => toggleMany(guard, rules, enabled, group),
    act: (r, action) => act(guard, r, action),
    open: (r) =>
      setDialog(r.custom ? { kind: 'custom', guard, editing: r } : { kind: 'builtin', guard, rule: r }),
    copy: (r) => copyAsCustom(guard, r),
    remove: (r) => setDialog({ kind: 'delete', guard, rule: r }),
    create: () => setDialog({ kind: 'custom', guard, editing: null }),
    test: () => setDialog({ kind: 'test', guard }),
  });

  const counts: Record<GuardMode, number> = { enforce: 0, observe: 0, off: 0 };
  if (detail) for (const g of GUARDS) counts[detail[g].mode] += 1;

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t('nav.contentSecurity')}</h1>
          <p className="text-muted-foreground">{t('contentSecurity.subtitle')}</p>
        </div>
        {detail && (
          <div className="flex items-center gap-4 text-sm text-muted-foreground">
            {(['enforce', 'observe', 'off'] as const)
              .filter((m) => counts[m] > 0)
              .map((m) => (
                <span key={m} className="inline-flex items-center gap-1.5">
                  <span aria-hidden className={cn('size-2 rounded-full', modeDot(m))} />
                  {t(`contentSecurity.summary.${m}`, { count: counts[m] })}
                </span>
              ))}
          </div>
        )}
      </div>

      {/* A failed re-read after a write keeps what is on screen; only a
          page with nothing loaded shows the error. */}
      {!detail ? (
        detailQuery.isPending ? (
          <div className="space-y-4" role="status" aria-busy="true">
            <Skeleton className="h-9 w-96 max-w-full" />
            <Skeleton className="h-36 w-full" />
            <Skeleton className="h-96 w-full" />
          </div>
        ) : (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription className="flex flex-wrap items-center gap-3">
              <span>
                {t('contentSecurity.loadFailed')}: {message(detailQuery.error, t('common.error'))}
              </span>
              <Button variant="outline" size="sm" onClick={() => void detailQuery.refetch()}>
                {t('common.retry')}
              </Button>
            </AlertDescription>
          </Alert>
        )
      ) : (
        <Tabs value={tab} onValueChange={(v) => setTab(v as Tab)}>
          <TabsList>
            {mayReadLog() && (
              <TabsTrigger value="log" className="px-3">
                {t('contentSecurity.log.tab')}
              </TabsTrigger>
            )}
            {GUARDS.map((g) => (
              <TabsTrigger key={g} value={g} className="gap-2 px-3">
                <span aria-hidden className={cn('size-1.5 rounded-full', modeDot(detail[g].mode))} />
                {guardName(t, g)}
                <span className="sr-only">{modeName(t, detail[g].mode)}</span>
              </TabsTrigger>
            ))}
          </TabsList>
          {mayReadLog() && (
            <TabsContent value="log" className="pt-2">
              <LogPanel detail={detail} />
            </TabsContent>
          )}
          {GUARDS.map((g) => (
            <TabsContent key={g} value={g} className="pt-2">
              <GuardPanel
                guard={g}
                detail={detail[g]}
                canWrite={mayWrite(g)}
                canTest={hasPermission(READ_PERMISSION[g])}
                actions={actions(g)}
              />
            </TabsContent>
          ))}
        </Tabs>
      )}

      {dialog?.kind === 'custom' && (
        <CustomRuleDialog
          guard={dialog.guard}
          mode={detail?.[dialog.guard].mode ?? 'observe'}
          editing={dialog.editing}
          seed={dialog.seed}
          taken={(detail?.[dialog.guard].rules ?? [])
            .filter((r) => r.custom && r.id !== dialog.editing?.id)
            .map((r) => r.id)}
          canWrite={mayWrite(dialog.guard)}
          onClose={() => setDialog(null)}
          onSave={(input) => saveCustom(dialog.guard, dialog.editing, input)}
        />
      )}
      {dialog?.kind === 'builtin' && (
        <BuiltinRuleDialog
          guard={dialog.guard}
          mode={detail?.[dialog.guard].mode ?? 'observe'}
          rule={dialog.rule}
          kinds={detail?.[dialog.guard].kinds ?? []}
          canWrite={mayWrite(dialog.guard)}
          onClose={() => setDialog(null)}
          onCopy={
            patternOf(dialog.rule)
              ? () => copyAsCustom(dialog.guard, dialog.rule)
              : undefined
          }
          onSaveAction={(a) => saveAction(dialog.guard, dialog.rule, a)}
        />
      )}
      {dialog?.kind === 'test' && <TestDialog guard={dialog.guard} detail={detail?.[dialog.guard]} onClose={() => setDialog(null)} />}
      <ConfirmDialog
        open={dialog?.kind === 'delete'}
        onOpenChange={(o) => !o && !deleting && setDialog(null)}
        title={dialog?.kind === 'delete' ? t('contentSecurity.dialog.deleteTitle', { name: dialog.rule.id }) : ''}
        description={t('contentSecurity.dialog.deleteDesc')}
        variant="destructive"
        confirmLabel={t('common.delete')}
        loading={deleting}
        onConfirm={() => {
          if (dialog?.kind === 'delete') void removeCustom(dialog.guard, dialog.rule);
        }}
      />
    </div>
  );
}
