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
import { guardName, modeDot, modeName, viewName } from './names';
import {
  customRuleView,
  hasAction,
  patternOf,
  policyOf,
  withAction,
  withCustom,
  withEnabled,
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
 * Request guards: outbound redaction, tool-call inspection and the content
 * filter, one tab each. All three apply to every request through the AI
 * gateway, whatever the key, model or provider.
 *
 * Each guard has a mode — off, observe, and a third mode named for what it
 * does there — and a rule table: built-in rules that can be switched (tool
 * and content rules can also take another action) and custom rules.
 *
 * **Every change is written at once, as the guard's whole policy object**
 * under its `security.*` settings key, built from the view on screen.
 * Writes run one after another so each carries the ones before it; a mode
 * or switch change shows first and offers an undo. Once nothing is in
 * flight the view is read back from the server.
 */
export function GatewaySecurityPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<Guard>('redact');
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
          t('contentSecurity.toast.modeSet', { guard: guardName(t, guard), mode: modeName(t, guard, mode) }),
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
    const name = viewName(t, guard, rule);
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
    await write(guard, withAction(d, rule.id, action), false);
    setDialog(null);
    toast.success(t('contentSecurity.toast.ruleSaved', { name: viewName(t, guard, rule) }));
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
        name: viewName(t, guard, rule),
        pattern: written.pattern,
        match: written.match,
        action: rule.action ?? null,
      },
    });
  }

  const actions = (guard: Guard): RuleActions => ({
    mode: (mode) => setMode(guard, mode),
    toggle: (r, enabled) => toggle(guard, r, enabled),
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
        <Tabs value={tab} onValueChange={(v) => setTab(v as Guard)}>
          <TabsList>
            {GUARDS.map((g) => (
              <TabsTrigger key={g} value={g} className="gap-2 px-3">
                {guardName(t, g)}
                <span aria-hidden className={cn('size-1.5 rounded-full', modeDot(detail[g].mode))} />
                <span className="sr-only">{modeName(t, g, detail[g].mode)}</span>
              </TabsTrigger>
            ))}
          </TabsList>
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
          rule={dialog.rule}
          canWrite={mayWrite(dialog.guard)}
          onClose={() => setDialog(null)}
          onCopy={
            hasAction(dialog.guard) && patternOf(dialog.rule)
              ? () => copyAsCustom(dialog.guard, dialog.rule)
              : undefined
          }
          onSaveAction={(a) => saveAction(dialog.guard, dialog.rule, a)}
        />
      )}
      {dialog?.kind === 'test' && <TestDialog guard={dialog.guard} onClose={() => setDialog(null)} />}
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
