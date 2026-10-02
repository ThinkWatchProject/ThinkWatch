import { useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { AlertCircle, Copy, Loader2 } from 'lucide-react';
import { Alert, AlertDescription } from '@/components/ui/alert';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Textarea } from '@/components/ui/textarea';
import { Segmented } from '@/components/segmented';
import { cn } from '@/lib/utils';
import type {
  ContentMatch,
  Guard,
  RuleAction,
  SecurityRuleView,
  SecurityTestHit,
  SecurityTestRequest,
} from '@/lib/security-types';
import { Highlight, type Mark } from './highlight';
import { Code, MatcherText } from './matcher-text';
import {
  actionName,
  actionTone,
  kindName,
  modeName,
  ruleName,
  ruleWhy,
  viewName,
} from './names';
import {
  ACTIONS,
  DEFAULT_LABEL,
  MAX_CODEPOINT_ITEMS,
  NEW_RULE_ACTION,
  checkCodepoints,
  hasAction,
  isLabel,
  patternOf,
  placeholder,
  type ActionGuard,
  type CustomRuleInput,
} from './policy';
import { useTrial, type Trial } from './use-trial';

/** What "copy as a custom rule" carries over from a built-in rule. */
export interface RuleSeed {
  name: string;
  pattern: string;
  match: ContentMatch;
  action: RuleAction | null;
}

const MATCHES: readonly ContentMatch[] = ['contains', 'regex', 'codepoints'];

function errorText(err: unknown, fallback: string): string {
  return err instanceof Error ? err.message : fallback;
}

function Field({
  label,
  htmlFor,
  hint,
  error,
  children,
}: {
  label: string;
  htmlFor?: string;
  hint?: ReactNode;
  error?: string | null;
  children: ReactNode;
}) {
  return (
    <div className="min-w-0 space-y-1.5">
      <Label htmlFor={htmlFor}>{label}</Label>
      {children}
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
      {error && <p className="text-xs text-destructive">{error}</p>}
    </div>
  );
}

/**
 * What a rule does in the third mode. Built-in and custom rules choose the
 * same way: a built-in rule supplies the pattern, the operator decides what
 * a hit does. A changed built-in rule says what the factory setting was.
 */
function ActionField({
  guard,
  value,
  onChange,
  factory,
  disabled,
}: {
  guard: ActionGuard;
  value: RuleAction;
  onChange: (a: RuleAction) => void;
  factory?: RuleAction | null;
  disabled: boolean;
}) {
  const { t } = useTranslation();
  const what =
    value === 'record'
      ? t(`contentSecurity.dialog.recordWhat.${guard}`)
      : t(`contentSecurity.dialog.actionWhat.${value}`, { mode: modeName(t, guard, 'enforce') });
  return (
    <Field
      label={t('contentSecurity.dialog.action')}
      hint={
        factory && factory !== value
          ? t('contentSecurity.dialog.withFactory', { what, action: actionName(t, factory) })
          : what
      }
    >
      <div>
        <Segmented
          label={t('contentSecurity.dialog.action')}
          value={value}
          options={ACTIONS[guard].map((a) => ({ value: a, label: actionName(t, a) }))}
          onChange={onChange}
          disabled={disabled}
        />
      </div>
    </Field>
  );
}

/** Red for a hit that stops the request or the call, amber for the rest. */
function toneOf(action: RuleAction | null | undefined): Mark['tone'] {
  return action === 'block' || action === 'cut' ? 'bad' : 'warn';
}

/**
 * What the server found: how many, marked in the sample; for redaction and
 * deletion, the text as the third mode would send it; for the content
 * filter, whether that mode would refuse the request.
 */
function TrialBox({
  guard,
  trial,
  action,
  showOutput,
  showRefused,
}: {
  guard: Guard;
  trial: Trial;
  /** Colour every hit as this action; left out, each hit's own. */
  action?: RuleAction | null;
  showOutput: boolean;
  showRefused: boolean;
}) {
  const { t } = useTranslation();
  if (trial.state === 'idle') return null;
  if (trial.state === 'failed') {
    return <p className="text-xs text-destructive">{trial.error}</p>;
  }
  const result = trial.state === 'done' ? trial.result : trial.last;
  const hits = result?.hits ?? [];
  const tone = (h: SecurityTestHit) => toneOf(action === undefined ? h.action : action);
  const bad = hits.some((h) => tone(h) === 'bad');
  const refused = showRefused && result?.refused === true;
  return (
    <div className="space-y-2 rounded-md border bg-muted/20 px-3 py-2">
      <p
        className={cn(
          'flex items-center gap-1.5 text-xs',
          hits.length === 0
            ? 'text-muted-foreground'
            : bad
              ? 'text-destructive'
              : 'text-amber-600 dark:text-amber-400',
        )}
      >
        {trial.state === 'running' && <Loader2 className="size-3 animate-spin text-muted-foreground" />}
        {result &&
          (hits.length > 0
            ? t('contentSecurity.dialog.hits', { count: hits.length })
            : t('contentSecurity.dialog.noHit'))}
      </p>
      {result && hits.length > 0 && (
        <Highlight
          text={result.sample}
          marks={hits.map((h) => ({ start: h.start, end: h.end, tone: tone(h) }))}
        />
      )}
      {refused && (
        <p className="text-xs text-destructive">
          {t('contentSecurity.dialog.refused', { mode: modeName(t, guard, 'enforce') })}
        </p>
      )}
      {showOutput && !refused && result && result.output !== null && (
        <div className="space-y-1 border-t pt-2">
          <p className="text-xs text-muted-foreground">
            {t('contentSecurity.dialog.sent', { mode: modeName(t, guard, 'enforce') })}
          </p>
          <pre className="font-mono text-xs break-words whitespace-pre-wrap">{result.output}</pre>
        </div>
      )}
    </div>
  );
}

/**
 * Create or edit a custom rule.
 *
 * Content rules match by text, regex or code points; redaction rules name
 * the placeholder a hit becomes; tool and content rules choose what a hit
 * does. Code points and the placeholder name are checked as they are typed;
 * a regex is checked by the server, which marks the sample as it changes.
 */
export function CustomRuleDialog({
  guard,
  editing,
  seed,
  taken,
  canWrite,
  onClose,
  onSave,
}: {
  guard: Guard;
  /** The rule being edited; null to create one. */
  editing: SecurityRuleView | null;
  seed?: RuleSeed;
  /** Names of the guard's other custom rules. */
  taken: string[];
  canWrite: boolean;
  onClose: () => void;
  onSave: (input: CustomRuleInput) => Promise<void>;
}) {
  const { t } = useTranslation();
  const content = guard === 'content';
  const redact = guard === 'redact';
  const acts = hasAction(guard) ? guard : null;
  const was = editing ? patternOf(editing) : null;

  const [name, setName] = useState(editing?.id ?? seed?.name ?? '');
  // Only content rules choose; the other guards' custom rules are regexes.
  const [match, setMatch] = useState<ContentMatch>(
    content ? (was?.match ?? seed?.match ?? 'contains') : 'regex',
  );
  const [pattern, setPattern] = useState(was?.pattern ?? seed?.pattern ?? '');
  const [label, setLabel] = useState(editing?.label || DEFAULT_LABEL);
  const [action, setAction] = useState<RuleAction>(
    editing?.action ?? seed?.action ?? (acts ? NEW_RULE_ACTION[acts] : 'record'),
  );
  const [sample, setSample] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const trimmed = name.trim();
  const clash = taken.includes(trimmed);
  const codepoints = content && match === 'codepoints' && pattern.trim() !== '' ? checkCodepoints(pattern) : null;
  const codepointError =
    codepoints?.kind === 'bad'
      ? t('contentSecurity.dialog.badCodepoint', { item: codepoints.item })
      : codepoints?.kind === 'tooMany'
        ? t('contentSecurity.dialog.tooManyCodepoints', { max: MAX_CODEPOINT_ITEMS })
        : null;
  const labelError = redact && !isLabel(label) ? t('contentSecurity.dialog.badLabel') : null;
  const empty = match === 'contains' ? pattern.length === 0 : pattern.trim().length === 0;
  // What is still missing, said next to the buttons; a wrong value is
  // already said under its field.
  const missing = !trimmed
    ? t('contentSecurity.dialog.nameRequired')
    : empty
      ? t('contentSecurity.dialog.patternRequired')
      : null;
  const blocked = missing !== null || clash || codepointError !== null || labelError !== null;

  const request: SecurityTestRequest | null =
    !empty && !codepointError && !labelError
      ? {
          sample,
          pattern,
          match: content ? match : undefined,
          label: redact ? label : undefined,
        }
      : null;
  const trial = useTrial(guard, request);

  const patternLabel = !content
    ? t('contentSecurity.dialog.patternRegex')
    : match === 'contains'
      ? t('contentSecurity.dialog.patternContains')
      : match === 'regex'
        ? t('contentSecurity.dialog.patternRegex')
        : t('contentSecurity.dialog.patternCodepoints');
  const patternHint = !content
    ? t(`contentSecurity.dialog.regexHint.${guard}`)
    : match === 'contains'
      ? t('contentSecurity.dialog.containsHint')
      : match === 'regex'
        ? t('contentSecurity.dialog.regexHint.content')
        : t('contentSecurity.dialog.codepointsHint');

  async function save() {
    setSaving(true);
    setError(null);
    try {
      await onSave({
        name: trimmed,
        pattern,
        match,
        action: acts ? action : null,
        label: redact ? label : null,
        enabled: editing?.enabled ?? true,
      });
    } catch (err) {
      setError(errorText(err, t('common.error')));
      setSaving(false);
    }
  }

  const readOnly = !canWrite;
  return (
    <Dialog open onOpenChange={(o) => !o && !saving && onClose()}>
      <DialogContent className="sm:max-w-xl" aria-describedby={undefined}>
        <DialogHeader>
          <DialogTitle>
            {editing
              ? t(`contentSecurity.guard.${guard}.editTitle`)
              : t(`contentSecurity.guard.${guard}.createTitle`)}
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-4">
          <Field
            label={t('contentSecurity.dialog.name')}
            htmlFor="rule-name"
            hint={t(`contentSecurity.guard.${guard}.nameHint`)}
            error={clash ? t('contentSecurity.dialog.nameTaken') : null}
          >
            <Input
              id="rule-name"
              value={name}
              onChange={(e) => {
                setError(null);
                setName(e.target.value);
              }}
              placeholder={t(`contentSecurity.guard.${guard}.namePlaceholder`)}
              aria-invalid={clash || undefined}
              disabled={readOnly}
            />
          </Field>

          {content && (
            <Field label={t('contentSecurity.dialog.matchKind')}>
              <div>
                <Segmented
                  label={t('contentSecurity.dialog.matchKind')}
                  value={match}
                  options={MATCHES.map((m) => ({
                    value: m,
                    label: t(`contentSecurity.dialog.match.${m}`),
                  }))}
                  onChange={(m) => {
                    setError(null);
                    setMatch(m);
                  }}
                  disabled={readOnly}
                />
              </div>
            </Field>
          )}

          <Field label={patternLabel} htmlFor="rule-pattern" hint={patternHint} error={codepointError}>
            <Input
              id="rule-pattern"
              className="font-mono"
              value={pattern}
              spellCheck={false}
              autoCapitalize="off"
              autoCorrect="off"
              placeholder={match === 'codepoints' ? 'U+200B, U+E0000–U+E007F' : undefined}
              onChange={(e) => {
                setError(null);
                setPattern(e.target.value);
              }}
              aria-invalid={codepointError ? true : undefined}
              disabled={readOnly}
            />
          </Field>

          {redact && (
            <Field
              label={t('contentSecurity.dialog.label')}
              htmlFor="rule-label"
              hint={t('contentSecurity.dialog.labelHint')}
              error={labelError}
            >
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <Input
                  id="rule-label"
                  className="w-56 font-mono"
                  value={label}
                  spellCheck={false}
                  autoCapitalize="characters"
                  autoCorrect="off"
                  onChange={(e) => {
                    setError(null);
                    setLabel(e.target.value);
                  }}
                  aria-invalid={labelError ? true : undefined}
                  disabled={readOnly}
                />
                {!labelError && (
                  <span className="text-xs text-muted-foreground">
                    {t('contentSecurity.dialog.becomes')} <Code>{placeholder(label)}</Code>
                  </span>
                )}
              </div>
            </Field>
          )}

          {acts && (
            <ActionField guard={acts} value={action} onChange={setAction} disabled={readOnly} />
          )}

          <Field label={t('contentSecurity.dialog.sample')} htmlFor="rule-sample">
            <Textarea
              id="rule-sample"
              className="min-h-16 font-mono text-xs"
              value={sample}
              spellCheck={false}
              placeholder={t(`contentSecurity.guard.${guard}.samplePlaceholder`)}
              onChange={(e) => setSample(e.target.value)}
            />
            <TrialBox
              guard={guard}
              trial={trial}
              action={acts ? action : null}
              showOutput={redact || action === 'strip'}
              showRefused={false}
            />
          </Field>
        </div>

        {error && (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>
              {t('contentSecurity.dialog.saveFailed')}: {error}
            </AlertDescription>
          </Alert>
        )}

        <DialogFooter className="items-center">
          {missing && canWrite && (
            <span className="mr-auto text-xs text-muted-foreground">{missing}</span>
          )}
          <Button variant="outline" onClick={onClose} disabled={saving}>
            {canWrite ? t('common.cancel') : t('common.close')}
          </Button>
          {canWrite && (
            <Button onClick={() => void save()} disabled={blocked || saving}>
              {saving ? t('common.saving') : editing ? t('common.save') : t('common.create')}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * A built-in rule: what it matches, why it is there, a sample to try it on
 * (also while it is off — the rules that ship off are the ones to try
 * first). Tool and content rules also take an action here, and can be
 * copied as a custom rule to change what they match.
 */
export function BuiltinRuleDialog({
  guard,
  rule,
  canWrite,
  onClose,
  onCopy,
  onSaveAction,
}: {
  guard: Guard;
  rule: SecurityRuleView;
  canWrite: boolean;
  onClose: () => void;
  /** Absent where no equivalent custom rule can be written (redaction). */
  onCopy?: () => void;
  onSaveAction: (action: RuleAction) => Promise<void>;
}) {
  const { t } = useTranslation();
  const acts = hasAction(guard) ? guard : null;
  const [action, setAction] = useState<RuleAction>(rule.action ?? 'record');
  const [sample, setSample] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const trial = useTrial(guard, { sample, rule: rule.id });
  const why = ruleWhy(t, guard, rule);
  const changed = acts !== null && action !== (rule.action ?? 'record');

  async function save() {
    setSaving(true);
    setError(null);
    try {
      await onSaveAction(action);
    } catch (err) {
      setError(errorText(err, t('common.error')));
      setSaving(false);
    }
  }

  return (
    <Dialog open onOpenChange={(o) => !o && !saving && onClose()}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            {viewName(t, guard, rule)}
            <Badge variant="secondary">{t('contentSecurity.builtinGroup')}</Badge>
          </DialogTitle>
          <DialogDescription>
            {why || t('contentSecurity.dialog.category', { kind: kindName(t, rule.kind) })}
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <Field label={t('contentSecurity.col.match')}>
            <div className="rounded-md border bg-muted/30 px-3 py-2 text-sm leading-relaxed">
              <MatcherText m={rule.matcher} />
            </div>
          </Field>

          {guard === 'redact' && (
            <Field label={t('contentSecurity.col.replaceWith')}>
              <div className="rounded-md border bg-muted/30 px-3 py-2 text-sm">
                <Code>{placeholder(rule.label)}</Code>
              </div>
            </Field>
          )}

          {acts && (
            <ActionField
              guard={acts}
              value={action}
              onChange={setAction}
              factory={rule.default_action}
              disabled={!canWrite}
            />
          )}

          <dl className="grid grid-cols-[88px_minmax(0,1fr)] gap-y-1 text-sm">
            <dt className="text-muted-foreground">{t('contentSecurity.dialog.state')}</dt>
            <dd>{rule.enabled ? t('contentSecurity.dialog.on') : t('contentSecurity.dialog.off')}</dd>
          </dl>

          <Field label={t('contentSecurity.dialog.sample')} htmlFor="builtin-sample">
            <Textarea
              id="builtin-sample"
              className="min-h-16 font-mono text-xs"
              value={sample}
              spellCheck={false}
              placeholder={t(`contentSecurity.guard.${guard}.samplePlaceholder`)}
              onChange={(e) => setSample(e.target.value)}
            />
            <TrialBox
              guard={guard}
              trial={trial}
              action={acts ? action : null}
              showOutput={guard === 'redact' || action === 'strip'}
              showRefused={false}
            />
          </Field>
        </div>

        {error && (
          <Alert variant="destructive">
            <AlertCircle className="h-4 w-4" />
            <AlertDescription>
              {t('contentSecurity.dialog.saveFailed')}: {error}
            </AlertDescription>
          </Alert>
        )}

        <DialogFooter className="items-center sm:justify-between">
          {onCopy ? (
            <Button variant="outline" onClick={onCopy} disabled={!canWrite || saving}>
              <Copy />
              {t('contentSecurity.menu.copyAsCustom')}
            </Button>
          ) : (
            <span />
          )}
          {acts && canWrite ? (
            <div className="flex gap-2">
              <Button variant="outline" onClick={onClose} disabled={saving}>
                {t('common.cancel')}
              </Button>
              <Button onClick={() => void save()} disabled={!changed || saving}>
                {saving ? t('common.saving') : t('common.save')}
              </Button>
            </div>
          ) : (
            <Button onClick={onClose}>{t('common.close')}</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * Tries a sample against every rule that is on, through the server — no
 * request is sent anywhere. Shows each rule that matched, the text the
 * third mode would send, and for the content filter whether that mode
 * would refuse the request.
 */
export function TestDialog({ guard, onClose }: { guard: Guard; onClose: () => void }) {
  const { t } = useTranslation();
  const [sample, setSample] = useState('');
  const trial = useTrial(guard, { sample });
  const result = trial.state === 'done' ? trial.result : trial.state === 'running' ? trial.last : undefined;
  const hits = result?.hits ?? [];
  const acts = hasAction(guard);

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>{t(`contentSecurity.guard.${guard}.testTitle`)}</DialogTitle>
          <DialogDescription>{t('contentSecurity.dialog.testDesc')}</DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <Field label={t('contentSecurity.dialog.sample')} htmlFor="test-sample">
            <Textarea
              id="test-sample"
              className="min-h-24 font-mono text-xs"
              value={sample}
              spellCheck={false}
              placeholder={t(`contentSecurity.guard.${guard}.samplePlaceholder`)}
              onChange={(e) => setSample(e.target.value)}
            />
          </Field>

          {trial.state !== 'idle' && (
            <div className="space-y-2">
              <TrialBox
                guard={guard}
                trial={trial}
                showOutput={guard !== 'inspect_tools'}
                showRefused={guard === 'content'}
              />
              {hits.length > 0 && (
                <Table className="table-fixed">
                  <colgroup>
                    <col className="w-[40%]" />
                    <col />
                    {acts && <col className="w-24" />}
                  </colgroup>
                  <TableHeader>
                    <TableRow>
                      <TableHead>{t('contentSecurity.col.rule')}</TableHead>
                      <TableHead>{t('contentSecurity.dialog.content')}</TableHead>
                      {acts && <TableHead>{t('contentSecurity.col.action')}</TableHead>}
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {hits.map((h, i) => (
                      <TableRow key={`${h.rule}:${h.start}:${i}`}>
                        <TableCell className="truncate">
                          {ruleName(t, guard, h.rule, h.custom)}
                          {h.custom && (
                            <Badge variant="outline" className="ml-1.5">
                              {kindName(t, 'custom')}
                            </Badge>
                          )}
                        </TableCell>
                        <TableCell className="truncate font-mono text-xs text-muted-foreground">
                          {h.excerpt}
                        </TableCell>
                        {acts && (
                          <TableCell className={actionTone(h.action)}>
                            {actionName(t, h.action ?? 'record')}
                          </TableCell>
                        )}
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              )}
            </div>
          )}
        </div>

        <DialogFooter>
          <Button onClick={onClose}>{t('common.close')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
