import type { TFunction } from 'i18next';
import type {
  Guard,
  GuardDetail,
  GuardMode,
  RuleAction,
  RuleScope,
  SecurityKindView,
  SecurityRuleView,
} from '@/lib/security-types';

// Names on the security page. A built-in rule's name, reason and
// description, and each group's name, come with the rule view in both
// languages (thinkwatch-core writes them once for both products); the
// console keeps no table of its own. A custom rule's id is the name its
// author gave it.

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

/** Whether the console is in Chinese: rule text comes from the server in both languages. */
function zh(lang: string): boolean {
  return lang.startsWith('zh');
}

/** The Chinese text in Chinese when there is one, else the English. */
function pick(lang: string, en: string | undefined, cn: string | undefined): string {
  return (zh(lang) && cn ? cn : en) ?? '';
}

/** A rule's name as the view gives it. A custom rule's id is the name its author gave it. */
export function viewName(lang: string, r: SecurityRuleView): string {
  if (r.custom) return r.id;
  return pick(lang, r.name, r.name_zh) || r.id;
}

/**
 * A rule's name from its id, for a log row or a trial hit: looked up in the
 * guard's view. A rule the view does not list (since removed) shows
 * `fallback`, or its id.
 */
export function ruleName(
  lang: string,
  detail: GuardDetail | undefined,
  id: string,
  custom: boolean,
  fallback?: string,
): string {
  if (custom) return id;
  const r = detail?.rules.find((r) => !r.custom && r.id === id);
  return r ? viewName(lang, r) : (fallback ?? id);
}

/** Why a built-in rule is worth a look. Empty for rules that carry no reason. */
export function ruleWhy(lang: string, r: SecurityRuleView): string {
  if (r.custom) return '';
  return pick(lang, r.why, r.why_zh);
}

/** What a built-in redaction rule finds, as a sentence; backticks mark code. Empty elsewhere. */
export function ruleAbout(lang: string, r: SecurityRuleView): string {
  if (r.custom) return '';
  return pick(lang, r.about, r.about_zh);
}

/** A rule group's heading, from the guard's view. */
export function kindName(lang: string, kinds: SecurityKindView[] | undefined, slug: string): string {
  const k = kinds?.find((k) => k.slug === slug);
  return k ? pick(lang, k.name, k.name_zh) : slug;
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
