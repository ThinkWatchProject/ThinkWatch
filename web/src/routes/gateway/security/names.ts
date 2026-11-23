import type { TFunction } from 'i18next';
import type {
  Guard,
  GuardDetail,
  GuardMode,
  Matcher,
  RuleAction,
  RuleScope,
  SecurityKindView,
  SecurityRuleView,
} from '@/lib/security-types';
import CATALOG from './guard.zh.json';

// Built-in display text is translated by guard and ID. The view supplies
// English fallbacks and custom rules keep their original names.
type RuleCopy = { name: string; why?: string; about?: string };
const catalog: Record<Guard, { rules: Record<string, RuleCopy>; kinds: Record<string, string> }> = CATALOG;

function guardAboutZh(m: Matcher): string | undefined {
  if (m.kind !== "prefix" && m.kind !== "prefixes") return undefined;
  const prefixes = m.kind === "prefix" ? [{ ...m, tail: "any" }] : m.prefixes;
  const groups = new Map<string, { prefixes: string[]; length: number; unit: string }>();
  for (const p of prefixes) {
    const key = `${p.min_tail}:${p.tail}`;
    const unit = p.tail === "hex" ? "十六进制字符" : p.tail === "alnum" ? "字母或数字" : "字符";
    const group = groups.get(key) ?? { prefixes: [], length: p.min_tail, unit };
    group.prefixes.push(`\`${p.prefix}\``);
    groups.set(key, group);
  }
  return [...groups.values()].map(({ prefixes, length, unit }) =>
    `${prefixes.join("、")} 开头，其后至少 ${length} 个${unit}`,
  ).join("；") || undefined;
}

export function guardName(t: TFunction, guard: Guard): string {
  return t(`contentSecurity.guard.${guard}.name`);
}

/** A mode's name, the same on every guard: On, Monitor, Off. */
export function modeName(t: TFunction, mode: GuardMode): string {
  return t(`contentSecurity.mode.${mode}`);
}

/** What a content rule checks, as a tag: Request, Answer. */
export function scopeName(t: TFunction, scope: RuleScope): string {
  return t(`contentSecurity.scope.${scope}`);
}

function zh(lang: string): boolean {
  return lang.startsWith('zh');
}

function ruleCopy(guard: Guard, id: string): RuleCopy | undefined {
  const rules = catalog[guard].rules;
  return Object.hasOwn(rules, id) ? rules[id] : undefined;
}

/** A rule's name as the view gives it. A custom rule's id is the name its author gave it. */
export function viewName(lang: string, guard: Guard, r: SecurityRuleView): string {
  if (r.custom) return r.id;
  return (zh(lang) ? ruleCopy(guard, r.id)?.name : undefined) ?? (r.name || r.id);
}

/**
 * A rule's name from its id, for a log row or a trial hit: looked up in the
 * guard's view. A rule the view does not list (since removed) shows
 * `fallback`, or its id.
 */
export function ruleName(
  lang: string,
  guard: Guard,
  detail: GuardDetail | undefined,
  id: string,
  custom: boolean,
  fallback?: string,
): string {
  if (custom) return id;
  const r = detail?.rules.find((r) => !r.custom && r.id === id);
  return r ? viewName(lang, guard, r) : ((zh(lang) ? ruleCopy(guard, id)?.name : undefined) ?? fallback ?? id);
}

/** Why a built-in rule is worth a look. Empty for rules that carry no reason. */
export function ruleWhy(lang: string, guard: Guard, r: SecurityRuleView): string {
  if (r.custom) return '';
  return (zh(lang) ? ruleCopy(guard, r.id)?.why : undefined) ?? r.why ?? '';
}

/** What a built-in redaction rule finds, as a sentence; backticks mark code. Empty elsewhere. */
export function ruleAbout(lang: string, guard: Guard, r: SecurityRuleView): string {
  if (r.custom) return '';
  return (zh(lang) ? guardAboutZh(r.matcher) ?? ruleCopy(guard, r.id)?.about : undefined) ?? r.about ?? '';
}

/** A rule group's heading, from the guard's view. */
export function kindName(lang: string, guard: Guard, kinds: SecurityKindView[] | undefined, slug: string): string {
  const copy = catalog[guard].kinds;
  if (zh(lang) && Object.hasOwn(copy, slug)) return copy[slug] ?? slug;
  const k = kinds?.find((k) => k.slug === slug);
  return k?.name ?? slug;
}

export function actionName(t: TFunction, action: RuleAction): string {
  return t(`contentSecurity.ruleAction.${action}`);
}

/**
 * Text colour for an action: stopping a request or a call is red, changing
 * text is amber, recording is muted.
 */
export function actionTone(action: RuleAction | null | undefined): string {
  switch (action) {
    case 'block':
    case 'cut':
      return 'text-destructive';
    case 'strip':
    case 'replace':
    case 'placeholder':
      return 'text-amber-600 dark:text-amber-400';
    default:
      return 'text-muted-foreground';
  }
}

/** Dot colour for a mode: enforcing is green, observing amber, off grey. */
export function modeDot(mode: GuardMode): string {
  return mode === 'enforce'
    ? 'bg-emerald-500'
    : mode === 'observe'
      ? 'bg-amber-500'
      : 'bg-muted-foreground/40';
}
