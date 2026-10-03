// The page edits the rule view it loaded and writes each guard back as a
// whole policy object. Everything here is pure: the view in, the next view
// or the policy out.
//
// **The policy is derived from the view, not read separately.** A built-in
// rule goes into `enable` when it is on but off out of the box, into
// `disable` the other way round, and into `actions` when its action is not
// the factory one; custom rules are written back from their matcher. What
// the table shows is therefore exactly what gets saved.

import type {
  ContentAction,
  ContentMatch,
  ContentPolicy,
  CustomContentRule,
  CustomRedactRule,
  CustomToolRule,
  Guard,
  GuardDetail,
  GuardMode,
  GuardPolicies,
  Matcher,
  RedactPolicy,
  RuleAction,
  SecurityRuleView,
  ToolAction,
  ToolPolicy,
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

/** The guards whose rules carry an action of their own. */
export type ActionGuard = 'inspect_tools' | 'content';

export function hasAction(guard: Guard): guard is ActionGuard {
  return guard !== 'redact';
}

/** The actions a rule can take, strongest first. */
export const ACTIONS: Record<ActionGuard, readonly RuleAction[]> = {
  inspect_tools: ['cut', 'record'],
  content: ['block', 'strip', 'record'],
};

/** What a new custom rule does: a rule written on purpose is usually meant to act. */
export const NEW_RULE_ACTION: Record<ActionGuard, RuleAction> = {
  inspect_tools: 'cut',
  content: 'block',
};

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

/** What a rule's matcher says, as a custom rule writes it. `null` for the built-in kinds. */
export function patternOf(r: SecurityRuleView): { pattern: string; match: ContentMatch } | null {
  switch (r.matcher.kind) {
    case 'regex':
      return { pattern: r.matcher.pattern, match: 'regex' };
    case 'contains':
      return { pattern: r.matcher.text, match: 'contains' };
    case 'codepoints':
      return { pattern: r.matcher.ranges.join(', '), match: 'codepoints' };
    default:
      return null;
  }
}

export function matcherOf(match: ContentMatch, pattern: string): Matcher {
  switch (match) {
    case 'regex':
      return { kind: 'regex', pattern };
    case 'contains':
      return { kind: 'contains', text: pattern };
    case 'codepoints':
      return { kind: 'codepoints', ranges: codepointItems(pattern) };
  }
}

/** A custom rule as the dialog saves it. */
export interface CustomRuleInput {
  name: string;
  pattern: string;
  /** Content rules choose; the other two guards' custom rules are regexes. */
  match: ContentMatch;
  /** Tool and content rules only. */
  action: RuleAction | null;
  /** Redaction only. */
  label: string | null;
  enabled: boolean;
}

/** The view of a custom rule about to be saved. The server's view replaces it once read back. */
export function customRuleView(input: CustomRuleInput): SecurityRuleView {
  return {
    id: input.name,
    custom: true,
    name: input.name,
    kind: 'custom',
    matcher: matcherOf(input.match, input.pattern),
    enabled: input.enabled,
    on_by_default: true,
    action: input.action,
    default_action: null,
    label: input.label,
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

/** A built-in rule's action. */
export function withAction(d: GuardDetail, id: string, action: RuleAction): GuardDetail {
  return {
    ...d,
    rules: d.rules.map((r) => (!r.custom && r.id === id ? { ...r, action } : r)),
  };
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

function customPattern(r: SecurityRuleView): { pattern: string; match: ContentMatch } {
  return patternOf(r) ?? { pattern: '', match: 'regex' };
}

function redactPolicy(d: GuardDetail): RedactPolicy {
  const { enable, disable } = builtinSwitches(d);
  const custom: CustomRedactRule[] = d.rules
    .filter((r) => r.custom)
    .map((r) => ({
      name: r.id,
      pattern: customPattern(r).pattern,
      label: r.label || DEFAULT_LABEL,
      ...(r.enabled ? {} : { disabled: true }),
    }));
  return {
    mode: d.mode,
    ...(enable.length ? { enable } : {}),
    ...(disable.length ? { disable } : {}),
    ...(custom.length ? { custom } : {}),
  };
}

const TOOL_ACTIONS: readonly ToolAction[] = ['cut', 'record'];
const CONTENT_ACTIONS: readonly ContentAction[] = ['block', 'strip', 'record'];

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
      const { pattern, match } = customPattern(r);
      const action: ContentAction = r.action === 'block' || r.action === 'strip' ? r.action : 'record';
      return { name: r.id, pattern, match, action, ...(r.enabled ? {} : { disabled: true }) };
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
