// The page edits the rule view it loaded and writes each guard back as a
// whole policy object. Everything here is pure: the view in, the next view
// or the policy out.
//
// **The policy is derived from the view, not read separately.** A built-in
// rule goes into `enable` when it is on but off out of the box, into
// `disable` the other way round, and into `actions` when its action is not
// the factory one; custom rules are written back from their matcher. What
// the table shows is therefore exactly what gets saved. A field left at its
// default (a content rule checking requests only, a replacement of `***`,
// a redaction rule replacing with a placeholder) is left out, so a policy
// saved by an earlier console reads the same.

import {
  DEFAULT_REPLACEMENT,
  type ContentAction,
  type ContentMatch,
  type ContentPolicy,
  type CustomContentRule,
  type CustomRedactRule,
  type CustomToolRule,
  type Guard,
  type GuardDetail,
  type GuardMode,
  type GuardPolicies,
  type Matcher,
  type RedactAction,
  type RedactMatch,
  type RedactPolicy,
  type RuleAction,
  type RuleScope,
  type SecurityRuleView,
  type ToolAction,
  type ToolPolicy,
} from '@/lib/security-types';

/** Placeholder label a redaction rule gets when none is written. */
export const DEFAULT_LABEL = 'SECRET';

/** Same rule as the server checks: capital letter first, 24 characters at most. */
const LABEL = /^[A-Z][A-Z0-9_]{0,23}$/;

export function isLabel(label: string): boolean {
  return LABEL.test(label);
}

/** What a hit is replaced with, numbered per label from 1. */
export function placeholder(label: string | null | undefined, n = 1): string {
  return `<<TW_${label || DEFAULT_LABEL}_${n}>>`;
}

/** The actions a rule can take, strongest first. */
export const ACTIONS: Record<Guard, readonly RuleAction[]> = {
  redact: ['placeholder', 'record'],
  inspect_tools: ['cut', 'record'],
  content: ['block', 'strip', 'replace', 'record'],
};

/**
 * What a new custom rule does. A redaction or tool-call rule written on
 * purpose is usually meant to act; a content rule starts by only
 * recording, to see what it would catch.
 */
export const NEW_RULE_ACTION: Record<Guard, RuleAction> = {
  redact: 'placeholder',
  inspect_tools: 'cut',
  content: 'record',
};

// ---------------------------------------------------------------------------
// What a content rule checks
// ---------------------------------------------------------------------------

/** The three choices the content rule dialog offers. */
export type ScopeChoice = 'request' | 'response' | 'both';

export const SCOPE_CHOICES: readonly ScopeChoice[] = ['request', 'response', 'both'];

export function scopeOfChoice(c: ScopeChoice): RuleScope[] {
  return c === 'both' ? ['request', 'response'] : [c];
}

/** What a rule checks. Rules that say nothing (built-in ones, other guards) check requests. */
export function ruleScope(r: Pick<SecurityRuleView, 'scope'>): RuleScope[] {
  const s = r.scope?.filter((x) => x === 'request' || x === 'response') ?? [];
  return s.length ? (['request', 'response'] as const).filter((x) => s.includes(x)) : ['request'];
}

export function choiceOfScope(scope: readonly RuleScope[]): ScopeChoice {
  const req = scope.includes('request');
  const res = scope.includes('response');
  return req && res ? 'both' : res ? 'response' : 'request';
}

/** Whether a rule with this scope checks the model's answers. */
export function checksAnswers(scope: readonly RuleScope[]): boolean {
  return scope.includes('response');
}

/**
 * Whether a content rule may take `action`: an answer that streams has
 * partly reached the client when a rule matches, so it cannot be refused.
 */
export function actionAllowed(action: RuleAction, scope: readonly RuleScope[]): boolean {
  return !(action === 'block' && checksAnswers(scope));
}

/** Code points are a request check: the answer is checked by text and regex. */
export function matchAllowed(match: ContentMatch, scope: readonly RuleScope[]): boolean {
  return !(match === 'codepoints' && checksAnswers(scope));
}

/** Built-in and custom rules may share a name. */
export function sameRule(a: Pick<SecurityRuleView, 'id' | 'custom'>, b: Pick<SecurityRuleView, 'id' | 'custom'>) {
  return a.id === b.id && a.custom === b.custom;
}

// ---------------------------------------------------------------------------
// Code points (content rules matched by `codepoints`)
// ---------------------------------------------------------------------------

/** At most this many items in one rule. */
export const MAX_CODEPOINT_ITEMS = 32;

const CODEPOINT_ITEM = /^u\+([0-9a-f]{1,6})(?:[-\u2013]u\+([0-9a-f]{1,6}))?$/i;

/** Items are separated by commas (either width), the ideographic comma or whitespace. */
export function codepointItems(text: string): string[] {
  return text.split(/[\s,\uff0c\u3001]+/).filter(Boolean);
}

export type CodepointProblem =
  | { kind: 'empty' }
  | { kind: 'tooMany' }
  | { kind: 'bad'; item: string };

const scalar = (n: number) => n <= 0x10ffff && (n < 0xd800 || n > 0xdfff);

/**
 * Checks a code-point list as it is typed: each item `U+HEX` or a range
 * `U+HEX-U+HEX` (hyphen or en dash, `u+` in either case), 1–6 hex digits,
 * within U+0000–U+10FFFF, no surrogate as an end, start not after end.
 * `null` when the list is fine. The server checks again when it is saved.
 */
export function checkCodepoints(text: string): CodepointProblem | null {
  const items = codepointItems(text);
  if (items.length === 0) return { kind: 'empty' };
  if (items.length > MAX_CODEPOINT_ITEMS) return { kind: 'tooMany' };
  for (const item of items) {
    const m = CODEPOINT_ITEM.exec(item);
    if (!m) return { kind: 'bad', item };
    const start = parseInt(m[1], 16);
    const end = m[2] === undefined ? start : parseInt(m[2], 16);
    if (!scalar(start) || !scalar(end) || start > end) return { kind: 'bad', item };
  }
  return null;
}

// ---------------------------------------------------------------------------
// Matchers
// ---------------------------------------------------------------------------

/** What a custom rule writes. */
export interface WrittenPattern {
  pattern: string;
  match: ContentMatch;
  /** Redaction and content rules choose; the view says it for every regex and text. */
  caseSensitive: boolean;
}

/** What a rule's matcher says, as a custom rule writes it. `null` for the built-in kinds. */
export function patternOf(r: SecurityRuleView): WrittenPattern | null {
  const m = r.matcher;
  switch (m.kind) {
    case 'regex':
      return { pattern: m.pattern, match: 'regex', caseSensitive: m.case_sensitive === true };
    case 'contains':
      return { pattern: m.text, match: 'contains', caseSensitive: m.case_sensitive === true };
    case 'codepoints':
      return { pattern: m.ranges.join(', '), match: 'codepoints', caseSensitive: false };
    default:
      return null;
  }
}

export function matcherOf(match: ContentMatch, pattern: string, caseSensitive = false): Matcher {
  const exact = caseSensitive ? { case_sensitive: true } : {};
  switch (match) {
    case 'regex':
      return { kind: 'regex', pattern, ...exact };
    case 'contains':
      return { kind: 'contains', text: pattern, ...exact };
    case 'codepoints':
      return { kind: 'codepoints', ranges: codepointItems(pattern) };
  }
}

/** How each guard's custom rules can match, the default first. */
export const MATCHES: Record<Guard, readonly ContentMatch[]> = {
  redact: ['contains', 'regex'],
  inspect_tools: ['regex'],
  content: ['contains', 'regex', 'codepoints'],
};

/** A custom rule as the dialog saves it. */
export interface CustomRuleInput {
  name: string;
  pattern: string;
  /** Content and redaction rules choose; tool rules are regexes. */
  match: ContentMatch;
  /** Redaction and content; tool rules have a fixed case rule. */
  caseSensitive: boolean;
  action: RuleAction;
  /** Redaction only. */
  label: string | null;
  /** Content only. */
  scope?: RuleScope[];
  /** Content rules that replace. */
  replacement?: string | null;
  enabled: boolean;
}

/** The view of a custom rule about to be saved. The server's view replaces it once read back. */
export function customRuleView(input: CustomRuleInput): SecurityRuleView {
  return {
    id: input.name,
    custom: true,
    name: input.name,
    kind: 'custom',
    matcher: matcherOf(input.match, input.pattern, input.caseSensitive),
    enabled: input.enabled,
    on_by_default: true,
    action: input.action,
    default_action: null,
    label: input.label,
    ...(input.scope ? { scope: input.scope } : {}),
    ...(input.action === 'replace' ? { replacement: input.replacement ?? DEFAULT_REPLACEMENT } : {}),
  };
}

// ---------------------------------------------------------------------------
// Edits: each returns the next view of one guard
// ---------------------------------------------------------------------------

export function withMode(d: GuardDetail, mode: GuardMode): GuardDetail {
  return { ...d, mode };
}

export function withEnabled(d: GuardDetail, rule: Pick<SecurityRuleView, 'id' | 'custom'>, enabled: boolean): GuardDetail {
  return { ...d, rules: d.rules.map((r) => (sameRule(r, rule) ? { ...r, enabled } : r)) };
}

/** Switches several rules at once: a group's checkbox. */
export function withEnabledMany(
  d: GuardDetail,
  rules: readonly Pick<SecurityRuleView, 'id' | 'custom'>[],
  enabled: boolean,
): GuardDetail {
  return {
    ...d,
    rules: d.rules.map((r) => (rules.some((x) => sameRule(r, x)) ? { ...r, enabled } : r)),
  };
}

/**
 * A rule's action, built-in or custom. A content rule that replaces keeps
 * the text it had, or gets the default one.
 */
export function withAction(d: GuardDetail, rule: Pick<SecurityRuleView, 'id' | 'custom'>, action: RuleAction): GuardDetail {
  return {
    ...d,
    rules: d.rules.map((r) => {
      if (!sameRule(r, rule)) return r;
      const next = { ...r, action };
      if (r.custom && action === 'replace' && !r.replacement) next.replacement = DEFAULT_REPLACEMENT;
      return next;
    }),
  };
}

/** How many of `rules` are on. */
export function enabledCount(rules: readonly SecurityRuleView[]): number {
  return rules.filter((r) => r.enabled).length;
}

/** A group checkbox: on, off or partly on. */
export function groupState(rules: readonly SecurityRuleView[]): boolean | 'indeterminate' {
  const on = enabledCount(rules);
  return on === 0 ? false : on === rules.length ? true : 'indeterminate';
}

/** The literals a built-in matcher shows: prefixes, name endings, field names, domain suffixes. */
function literalsOf(m: Matcher): string[] {
  switch (m.kind) {
    case 'prefix':
      return [m.prefix];
    case 'prefixes':
      return m.prefixes.map((p) => p.prefix);
    case 'assignment':
      return m.suffixes;
    case 'labeled':
      return m.names;
    case 'domain-suffix':
      return m.suffixes;
    default:
      return [];
  }
}

/**
 * Whether a rule matches a search: its name (as shown, in either language,
 * or its id) or what it matches — a custom rule's pattern, a built-in
 * rule's description, prefixes or field names — ignoring case. An empty
 * search matches every rule.
 */
export function matchesSearch(r: SecurityRuleView, shownName: string, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  const pattern = patternOf(r)?.pattern ?? '';
  return [
    shownName,
    r.id,
    r.name,
    r.name_zh ?? '',
    r.about ?? '',
    r.about_zh ?? '',
    pattern,
    ...literalsOf(r.matcher),
  ].some((s) => s.toLowerCase().includes(q));
}

/**
 * Saves a custom rule: replaces the one named `previous` in place (an edit,
 * possibly a rename), or adds it after the others when `previous` is null.
 */
export function withCustom(d: GuardDetail, previous: string | null, rule: SecurityRuleView): GuardDetail {
  const at = previous === null ? -1 : d.rules.findIndex((r) => r.custom && r.id === previous);
  return {
    ...d,
    rules: at < 0 ? [...d.rules, rule] : d.rules.map((r, i) => (i === at ? rule : r)),
  };
}

export function withoutCustom(d: GuardDetail, name: string): GuardDetail {
  return { ...d, rules: d.rules.filter((r) => !(r.custom && r.id === name)) };
}

// ---------------------------------------------------------------------------
// View → stored policy
// ---------------------------------------------------------------------------

function builtinSwitches(d: GuardDetail) {
  const builtins = d.rules.filter((r) => !r.custom);
  return {
    builtins,
    enable: builtins.filter((r) => r.enabled && !r.on_by_default).map((r) => r.id),
    disable: builtins.filter((r) => !r.enabled && r.on_by_default).map((r) => r.id),
  };
}

/** Built-in rules whose action is not the factory one. */
function changedActions<A extends RuleAction>(builtins: SecurityRuleView[], allowed: readonly A[]) {
  const out: Record<string, A> = {};
  for (const r of builtins) {
    const a = r.action as A | null | undefined;
    if (a && allowed.includes(a) && r.default_action && a !== r.default_action) out[r.id] = a;
  }
  return out;
}

function customPattern(r: SecurityRuleView): WrittenPattern {
  return patternOf(r) ?? { pattern: '', match: 'regex', caseSensitive: false };
}

const REDACT_ACTIONS: readonly RedactAction[] = ['placeholder', 'record'];

function redactPolicy(d: GuardDetail): RedactPolicy {
  const { builtins, enable, disable } = builtinSwitches(d);
  const actions = changedActions(builtins, REDACT_ACTIONS);
  const custom: CustomRedactRule[] = d.rules
    .filter((r) => r.custom)
    .map((r) => {
      const { pattern, match, caseSensitive } = customPattern(r);
      // Code points never reach a redaction rule; written as text if one did.
      const how: RedactMatch = match === 'regex' ? 'regex' : 'contains';
      return {
        name: r.id,
        pattern,
        match: how,
        ...(caseSensitive ? { case_sensitive: true } : {}),
        label: r.label || DEFAULT_LABEL,
        ...(r.action === 'record' ? { action: 'record' as const } : {}),
        ...(r.enabled ? {} : { disabled: true }),
      };
    });
  return {
    mode: d.mode,
    ...(enable.length ? { enable } : {}),
    ...(disable.length ? { disable } : {}),
    ...(Object.keys(actions).length ? { actions } : {}),
    ...(custom.length ? { custom } : {}),
  };
}

const TOOL_ACTIONS: readonly ToolAction[] = ['cut', 'record'];
const CONTENT_ACTIONS: readonly ContentAction[] = ['block', 'strip', 'replace', 'record'];

function toolPolicy(d: GuardDetail): ToolPolicy {
  const { builtins, enable, disable } = builtinSwitches(d);
  const actions = changedActions(builtins, TOOL_ACTIONS);
  const custom: CustomToolRule[] = d.rules
    .filter((r) => r.custom)
    .map((r) => ({
      name: r.id,
      pattern: customPattern(r).pattern,
      action: r.action === 'cut' ? 'cut' : 'record',
      ...(r.enabled ? {} : { disabled: true }),
    }));
  return {
    mode: d.mode,
    ...(enable.length ? { enable } : {}),
    ...(disable.length ? { disable } : {}),
    ...(Object.keys(actions).length ? { actions } : {}),
    ...(custom.length ? { custom } : {}),
  };
}

function contentPolicy(d: GuardDetail): ContentPolicy {
  const { builtins, enable, disable } = builtinSwitches(d);
  const actions = changedActions(builtins, CONTENT_ACTIONS);
  const custom: CustomContentRule[] = d.rules
    .filter((r) => r.custom)
    .map((r) => {
      const { pattern, match, caseSensitive } = customPattern(r);
      const action: ContentAction =
        r.action === 'block' || r.action === 'strip' || r.action === 'replace' ? r.action : 'record';
      const scope = ruleScope(r);
      const replacement = r.replacement ?? DEFAULT_REPLACEMENT;
      return {
        name: r.id,
        pattern,
        ...(scope.length === 1 && scope[0] === 'request' ? {} : { scope }),
        match,
        ...(caseSensitive && match !== 'codepoints' ? { case_sensitive: true } : {}),
        action,
        ...(action === 'replace' && replacement !== DEFAULT_REPLACEMENT ? { replacement } : {}),
        ...(r.enabled ? {} : { disabled: true }),
      };
    });
  return {
    mode: d.mode,
    ...(enable.length ? { enable } : {}),
    ...(disable.length ? { disable } : {}),
    ...(Object.keys(actions).length ? { actions } : {}),
    ...(custom.length ? { custom } : {}),
  };
}

/** The whole policy object for one guard, as its settings key stores it. */
export function policyOf<G extends Guard>(guard: G, d: GuardDetail): GuardPolicies[G];
export function policyOf(guard: Guard, d: GuardDetail): GuardPolicies[Guard] {
  switch (guard) {
    case 'redact':
      return redactPolicy(d);
    case 'inspect_tools':
      return toolPolicy(d);
    case 'content':
      return contentPolicy(d);
  }
}
