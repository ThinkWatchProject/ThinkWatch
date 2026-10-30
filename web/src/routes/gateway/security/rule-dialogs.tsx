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
import { Switch } from '@/components/ui/switch';
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
import {
  DEFAULT_REPLACEMENT,
  type ContentMatch,
  type Guard,
  type GuardDetail,
  type GuardMode,
  type RuleAction,
  type RuleScope,
  type SecurityKindView,
  type SecurityRuleView,
  type SecurityTestHit,
  type SecurityTestRequest,
} from '@/lib/security-types';
import { Highlight, type Mark } from './highlight';
import { About, Code, MatcherText } from './matcher-text';
import { actionName, actionTone, kindName, ruleAbout, ruleName, ruleWhy, scopeName, viewName } from './names';
import {
  ACTIONS,
  DEFAULT_LABEL,
  MATCHES,
  MAX_CODEPOINT_ITEMS,
  NEW_RULE_ACTION,
  SCOPE_CHOICES,
  actionAllowed,
  checkCodepoints,
  checksAnswers,
  choiceOfScope,
  isLabel,
  matchAllowed,
  patternOf,
  placeholder,
  ruleScope,
  scopeOfChoice,
  type CustomRuleInput,
  type ScopeChoice,
} from './policy';
import { useTrial, type Trial } from './use-trial';

/** What "copy as a custom rule" carries over from a built-in rule. */
export interface RuleSeed {
  name: string;
  pattern: string;
  match: ContentMatch;
  action: RuleAction | null;
}

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
      {hint && <div className="space-y-0.5 text-xs text-muted-foreground">{hint}</div>}
      {error && <p className="text-xs text-destructive">{error}</p>}
    </div>
  );
}

/**
 * What a rule does when its guard is on. Built-in and custom rules choose
 * the same way: a built-in rule supplies the pattern, the operator decides
 * what a hit does. An action a content rule cannot take on what it checks
 * stays visible, unavailable, with the reason under it. A changed built-in
 * rule says what the factory setting was.
 */
function ActionField({
  guard,
  value,
  onChange,
  scope,
  factory,
  notice,
  disabled,
}: {
  guard: Guard;
  value: RuleAction;
  onChange: (a: RuleAction) => void;
  /** Content rules: what the rule checks. */
  scope?: RuleScope[];
  factory?: RuleAction | null;
  /** Said once the action was changed for the operator. */
  notice?: string | null;
  disabled: boolean;
}) {
  const { t } = useTranslation();
  const what = t(`contentSecurity.dialog.actionWhat.${value}`);
  const unavailable = ACTIONS[guard].some((a) => scope && !actionAllowed(a, scope));
  return (
    <Field
      label={t('contentSecurity.dialog.action')}
      hint={
        <>
          <p>
            {factory && factory !== value
              ? t('contentSecurity.dialog.withFactory', { what, action: actionName(t, factory) })
              : what}
          </p>
          {unavailable && <p>{t('contentSecurity.dialog.blockReason')}</p>}
          {notice && <p className="text-amber-600 dark:text-amber-400">{notice}</p>}
        </>
      }
    >
      <div>
        <Segmented
          label={t('contentSecurity.dialog.action')}
          value={value}
          options={ACTIONS[guard].map((a) => ({
            value: a,
            label: actionName(t, a),
            disabled: scope ? !actionAllowed(a, scope) : false,
          }))}
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
 * What the server found: how many, marked in the sample; the text as it
 * would be sent or received with the guard on; for the content filter,
 * whether the request would be refused.
 *
 * Every hit carries the action it takes there. A single rule is tried with
 * the action chosen in its dialog, so the colours, the processed text and
 * the refusal all follow that choice.
 */
function TrialBox({ trial }: { trial: Trial }) {
  const { t } = useTranslation();
  if (trial.state === 'idle') return null;
  if (trial.state === 'failed') {
    return <p className="text-xs text-destructive">{trial.error}</p>;
  }
  const result = trial.state === 'done' ? trial.result : trial.last;
  const all = result?.hits ?? [];
  // An exception value is marked, but it is not a hit: nothing is done to it.
  const hits = all.filter((h) => !h.excepted);
  const excepted = all.length - hits.length;
  const tone = (h: SecurityTestHit): Mark['tone'] => (h.excepted ? 'muted' : toneOf(h.action));
  const bad = hits.some((h) => tone(h) === 'bad');
  // Dangerous commands send nothing and refuse nothing; the server
  // answers null and false there.
  const refused = result?.refused === true;
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
      {result && all.length > 0 && (
        <Highlight
          text={result.sample}
          marks={all.map((h) => ({ start: h.start, end: h.end, tone: tone(h) }))}
        />
      )}
      {excepted > 0 && (
        <p className="text-xs text-muted-foreground">
          {t('contentSecurity.exceptions.inTrial', { count: excepted })}
        </p>
      )}
      {refused && <p className="text-xs text-destructive">{t('contentSecurity.dialog.refused')}</p>}
      {!refused && result && result.output !== null && (
        <div className="space-y-1 border-t pt-2">
          <p className="text-xs text-muted-foreground">{t('contentSecurity.dialog.output')}</p>
          <pre className="font-mono text-xs break-words whitespace-pre-wrap">{result.output}</pre>
        </div>
      )}
    </div>
  );
}

/** Which side a content sample is tried as: a request or an answer. */
function TestSide({
  value,
  onChange,
  scope,
}: {
  value: RuleScope;
  onChange: (s: RuleScope) => void;
  /** The sides the rule checks; the other is unavailable. Left out: both. */
  scope?: RuleScope[];
}) {
  const { t } = useTranslation();
  return (
    <Segmented
      size="sm"
      label={t('contentSecurity.dialog.testAs')}
      value={value}
      options={(['request', 'response'] as const).map((s) => ({
        value: s,
        label: t(`contentSecurity.dialog.testAsSide.${s}`),
        disabled: scope ? !scope.includes(s) : false,
      }))}
      onChange={onChange}
    />
  );
}

/** What the guard's state means for the rule being edited, said by the buttons. */
function ModeNote({ mode }: { mode: GuardMode }) {
  const { t } = useTranslation();
  if (mode === 'enforce') return null;
  return <span className="mr-auto text-xs text-muted-foreground">{t(`contentSecurity.dialog.modeNote.${mode}`)}</span>;
}

/**
 * Create or edit a custom rule.
 *
 * Content rules say what they check — requests, answers or both — match by
 * text, regex or code points (requests only), case-sensitive or not, and
 * refuse (requests only), delete, replace with a text of their own, or
 * record. Redaction rules match by text or regex, case-sensitive or not,
 * name the placeholder a hit becomes, and replace or record. Tool rules
 * are regexes that cut the call off or record it. A choice the scope rules
 * out stays visible and unavailable; changing the scope so that the current
 * choice no longer fits switches it and says so. Code points and the
 * placeholder name are checked as they are typed; a regex is checked by
 * the server, which marks the sample as it changes.
 */
export function CustomRuleDialog({
  guard,
  mode,
  editing,
  seed,
  taken,
  canWrite,
  onClose,
  onSave,
}: {
  guard: Guard;
  /** The guard's state, for the note by the buttons. */
  mode: GuardMode;
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
  const was = editing ? patternOf(editing) : null;

  const [name, setName] = useState(editing?.id ?? seed?.name ?? '');
  const [scopeChoice, setScopeChoice] = useState<ScopeChoice>(
    content && editing ? choiceOfScope(ruleScope(editing)) : 'request',
  );
  const scope = scopeOfChoice(scopeChoice);
  // Content and redaction rules choose; tool rules are regexes.
  const matches = MATCHES[guard];
  const [match, setMatch] = useState<ContentMatch>(() => {
    const written = was?.match ?? seed?.match;
    return written && matches.includes(written) ? written : matches[0];
  });
  const choosesCase = redact || content;
  const [caseSensitive, setCaseSensitive] = useState(choosesCase && (was?.caseSensitive ?? false));
  const [pattern, setPattern] = useState(was?.pattern ?? seed?.pattern ?? '');
  const [label, setLabel] = useState(editing?.label || DEFAULT_LABEL);
  const [action, setAction] = useState<RuleAction>(() => {
    const a = editing?.action ?? seed?.action;
    return a && ACTIONS[guard].includes(a) ? a : NEW_RULE_ACTION[guard];
  });
  const [replacement, setReplacement] = useState(editing?.replacement || DEFAULT_REPLACEMENT);
  const [matchNotice, setMatchNotice] = useState<string | null>(null);
  const [actionNotice, setActionNotice] = useState<string | null>(null);
  const [testSide, setTestSide] = useState<RuleScope>(scope[0]);
  const [sample, setSample] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  /** A scope that checks answers takes neither code points nor a refusal: those switch, with a notice. */
  function chooseScope(next: ScopeChoice) {
    setError(null);
    setScopeChoice(next);
    const s = scopeOfChoice(next);
    if (!s.includes(testSide)) setTestSide(s[0]);
    if (!matchAllowed(match, s)) {
      setMatch('contains');
      setMatchNotice(t('contentSecurity.dialog.matchSwitched'));
    } else {
      setMatchNotice(null);
    }
    if (!actionAllowed(action, s)) {
      setAction('strip');
      setActionNotice(t('contentSecurity.dialog.actionSwitched'));
    } else {
      setActionNotice(null);
    }
  }

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
  const replaces = content && action === 'replace';
  const empty = match === 'contains' ? pattern.length === 0 : pattern.trim().length === 0;
  // What is still missing, said next to the buttons; a wrong value is
  // already said under its field.
  const missing = !trimmed
    ? t('contentSecurity.dialog.nameRequired')
    : empty
      ? t('contentSecurity.dialog.patternRequired')
      : replaces && replacement.length === 0
        ? t('contentSecurity.dialog.replacementRequired')
        : null;
  const blocked = missing !== null || clash || codepointError !== null || labelError !== null;
  const exact = choosesCase && match !== 'codepoints' && caseSensitive;

  const request: SecurityTestRequest | null =
    !empty && !codepointError && !labelError
      ? {
          sample,
          pattern,
          match: matches.length > 1 ? match : undefined,
          case_sensitive: choosesCase && match !== 'codepoints' ? caseSensitive : undefined,
          label: redact ? label : undefined,
          action,
          ...(content ? { scope: testSide } : {}),
          ...(replaces ? { replacement } : {}),
        }
      : null;
  const trial = useTrial(guard, request);

  const patternLabel =
    match === 'contains'
      ? t('contentSecurity.dialog.patternContains')
      : match === 'regex'
        ? t('contentSecurity.dialog.patternRegex')
        : t('contentSecurity.dialog.patternCodepoints');
  const caseHint = exact ? t('contentSecurity.dialog.caseHintOn') : t('contentSecurity.dialog.caseHintOff');
  const matchHint =
    match === 'contains'
      ? `${t('contentSecurity.dialog.containsHint')}${caseHint}`
      : match === 'regex'
        ? guard === 'inspect_tools'
          ? t('contentSecurity.dialog.regexHint.inspect_tools')
          : `${t(`contentSecurity.dialog.regexHint.${guard}`)}${caseHint}`
        : t('contentSecurity.dialog.codepointsHint');

  async function save() {
    setSaving(true);
    setError(null);
    try {
      await onSave({
        name: trimmed,
        pattern,
        match,
        caseSensitive: exact,
        action,
        label: redact ? label : null,
        ...(content ? { scope, replacement: replaces ? replacement : null } : {}),
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

        <div className="max-h-[70vh] space-y-4 overflow-y-auto pr-1">
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
            <Field label={t('contentSecurity.dialog.scope')} hint={t('contentSecurity.dialog.scopeHint')}>
              <div>
                <Segmented
                  label={t('contentSecurity.dialog.scope')}
                  value={scopeChoice}
                  options={SCOPE_CHOICES.map((c) => ({
                    value: c,
                    label: t(`contentSecurity.scopeChoice.${c}`),
                  }))}
                  onChange={chooseScope}
                  disabled={readOnly}
                />
              </div>
            </Field>
          )}

          {matches.length > 1 && (
            <Field
              label={t('contentSecurity.dialog.matchKind')}
              hint={
                <>
                  <p>{matchHint}</p>
                  {content && checksAnswers(scope) && match === 'regex' && (
                    <p>{t('contentSecurity.dialog.regexAnswerHint')}</p>
                  )}
                  {content && checksAnswers(scope) && <p>{t('contentSecurity.dialog.codepointsReason')}</p>}
                  {matchNotice && <p className="text-amber-600 dark:text-amber-400">{matchNotice}</p>}
                </>
              }
            >
              <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
                <Segmented
                  label={t('contentSecurity.dialog.matchKind')}
                  value={match}
                  options={matches.map((m) => ({
                    value: m,
                    label: t(`contentSecurity.dialog.match.${m}`),
                    disabled: content ? !matchAllowed(m, scope) : false,
                  }))}
                  onChange={(m) => {
                    setError(null);
                    setMatchNotice(null);
                    setMatch(m);
                  }}
                  disabled={readOnly}
                />
                {choosesCase && (
                  <label className="flex items-center gap-2 text-sm">
                    <Switch
                      size="sm"
                      checked={match !== 'codepoints' && caseSensitive}
                      onCheckedChange={(v) => {
                        setError(null);
                        setCaseSensitive(v);
                      }}
                      disabled={readOnly || match === 'codepoints'}
                    />
                    {t('contentSecurity.dialog.caseSensitive')}
                  </label>
                )}
              </div>
            </Field>
          )}

          <Field
            label={patternLabel}
            htmlFor="rule-pattern"
            hint={matches.length > 1 ? undefined : matchHint}
            error={codepointError}
          >
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

          <ActionField
            guard={guard}
            value={action}
            onChange={(a) => {
              setActionNotice(null);
              setAction(a);
            }}
            scope={content ? scope : undefined}
            notice={actionNotice}
            disabled={readOnly}
          />

          {replaces && (
            <Field
              label={t('contentSecurity.dialog.replacement')}
              htmlFor="rule-replacement"
              hint={t('contentSecurity.dialog.replacementHint')}
            >
              <Input
                id="rule-replacement"
                className="w-56 font-mono"
                value={replacement}
                spellCheck={false}
                onChange={(e) => {
                  setError(null);
                  setReplacement(e.target.value);
                }}
                disabled={readOnly}
              />
            </Field>
          )}

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

          <Field label={t('contentSecurity.dialog.sample')} htmlFor="rule-sample">
            {content && <TestSide value={testSide} onChange={setTestSide} scope={scope} />}
            <Textarea
              id="rule-sample"
              className="min-h-16 font-mono text-xs"
              value={sample}
              spellCheck={false}
              placeholder={t(`contentSecurity.guard.${guard}.samplePlaceholder`)}
              onChange={(e) => setSample(e.target.value)}
            />
            <TrialBox trial={trial} />
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
          {missing && canWrite ? (
            <span className="mr-auto text-xs text-muted-foreground">{missing}</span>
          ) : (
            <ModeNote mode={mode} />
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
 * A built-in rule: what it matches, why it is there, its action, and a
 * sample to try it on (also while it is off — the rules that ship off are
 * the ones to try first). A built-in rule with a pattern can be copied as
 * a custom rule to change what it matches.
 */
export function BuiltinRuleDialog({
  guard,
  mode,
  rule,
  kinds,
  canWrite,
  onClose,
  onCopy,
  onSaveAction,
}: {
  guard: Guard;
  mode: GuardMode;
  rule: SecurityRuleView;
  /** The guard's groups, to name the rule's. */
  kinds: SecurityKindView[];
  canWrite: boolean;
  onClose: () => void;
  /** Absent where no equivalent custom rule can be written: checks implemented in code. */
  onCopy?: () => void;
  onSaveAction: (action: RuleAction) => Promise<void>;
}) {
  const { t, i18n } = useTranslation();
  const lang = i18n.language;
  const initial = rule.action ?? 'record';
  const [action, setAction] = useState<RuleAction>(initial);
  const [sample, setSample] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const trial = useTrial(guard, { sample, rule: rule.id, action });
  const why = ruleWhy(lang, rule);
  const about = ruleAbout(lang, rule);
  const changed = action !== initial;

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
            {viewName(lang, rule)}
            <Badge variant="secondary">{t('contentSecurity.builtinGroup')}</Badge>
          </DialogTitle>
          <DialogDescription>
            {why || t('contentSecurity.dialog.category', { kind: kindName(lang, kinds, rule.kind) })}
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <Field label={t('contentSecurity.col.match')}>
            <div className="rounded-md border bg-muted/30 px-3 py-2 text-sm leading-relaxed">
              {about ? <About text={about} /> : <MatcherText m={rule.matcher} />}
            </div>
          </Field>

          {guard === 'content' && (
            <Field label={t('contentSecurity.dialog.scope')} hint={t('contentSecurity.dialog.builtinScope')}>
              <div className="flex gap-1">
                {ruleScope(rule).map((s) => (
                  <Badge key={s} variant="outline" className="font-normal">
                    {scopeName(t, s)}
                  </Badge>
                ))}
              </div>
            </Field>
          )}

          {guard === 'redact' && (
            <Field label={t('contentSecurity.dialog.placeholderField')}>
              <div className="rounded-md border bg-muted/30 px-3 py-2 text-sm">
                <Code>{placeholder(rule.label)}</Code>
              </div>
            </Field>
          )}

          <ActionField
            guard={guard}
            value={action}
            onChange={setAction}
            scope={guard === 'content' ? ruleScope(rule) : undefined}
            factory={rule.default_action}
            disabled={!canWrite}
          />

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
            <TrialBox trial={trial} />
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
            <ModeNote mode={mode} />
          )}
          {canWrite ? (
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
 * request is sent anywhere. Shows each rule that matched, the text after
 * processing, and for the content filter whether the request would be
 * refused. A content sample is tried as a request or as an answer.
 */
export function TestDialog({
  guard,
  detail,
  onClose,
}: {
  guard: Guard;
  /** The guard's view, to name the rules that matched. */
  detail: GuardDetail | undefined;
  onClose: () => void;
}) {
  const { t, i18n } = useTranslation();
  const lang = i18n.language;
  const [sample, setSample] = useState('');
  const [side, setSide] = useState<RuleScope>('request');
  const content = guard === 'content';
  const trial = useTrial(guard, content ? { sample, scope: side } : { sample });
  const result = trial.state === 'done' ? trial.result : trial.state === 'running' ? trial.last : undefined;
  const hits = result?.hits ?? [];

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>{t(`contentSecurity.guard.${guard}.testTitle`)}</DialogTitle>
          <DialogDescription>{t('contentSecurity.dialog.testDesc')}</DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <Field label={t('contentSecurity.dialog.sample')} htmlFor="test-sample">
            {content && <TestSide value={side} onChange={setSide} />}
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
              <TrialBox trial={trial} />
              {hits.length > 0 && (
                <Table className="table-fixed">
                  <colgroup>
                    <col className="w-[40%]" />
                    <col />
                    <col className="w-32" />
                  </colgroup>
                  <TableHeader>
                    <TableRow>
                      <TableHead>{t('contentSecurity.col.rule')}</TableHead>
                      <TableHead>{t('contentSecurity.dialog.content')}</TableHead>
                      <TableHead>{t('contentSecurity.col.action')}</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {hits.map((h, i) => (
                      <TableRow key={`${h.rule}:${h.start}:${i}`}>
                        <TableCell className="truncate">
                          {ruleName(lang, detail, h.rule, h.custom)}
                          {h.custom && (
                            <Badge variant="outline" className="ml-1.5">
                              {kindName(lang, detail?.kinds, 'custom')}
                            </Badge>
                          )}
                        </TableCell>
                        <TableCell className="truncate font-mono text-xs text-muted-foreground">
                          {h.excerpt}
                        </TableCell>
                        {h.excepted ? (
                          <TableCell className="text-muted-foreground">
                            {t('contentSecurity.exceptions.excepted')}
                          </TableCell>
                        ) : (
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
