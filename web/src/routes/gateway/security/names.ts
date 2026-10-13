import type { TFunction } from 'i18next';
import type { Guard, GuardMode, RuleAction, RuleScope, SecurityRuleView } from '@/lib/security-types';

// Names on the security page. Built-in rules are looked up by id; a rule
// the server ships before the console has a name for it falls back to the
// server's English. A custom rule's id is the name its author gave it.

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

export function ruleName(
  t: TFunction,
  guard: Guard,
  id: string,
  custom: boolean,
  fallback?: string,
): string {
  if (custom) return id;
  const defaultValue = fallback ?? id;
  switch (guard) {
    case 'redact':
      return t(`contentSecurity.redactRule.${id}`, { defaultValue });
    case 'inspect_tools':
      return t(`contentSecurity.toolRule.${id}.name`, { defaultValue });
    case 'content':
      return t(`contentSecurity.contentRule.${id}`, { defaultValue });
  }
}

export function viewName(t: TFunction, guard: Guard, r: SecurityRuleView): string {
  return ruleName(t, guard, r.id, r.custom, r.name);
}

/** Why a built-in rule is worth a look. Empty for rules that carry no reason. */
export function ruleWhy(t: TFunction, guard: Guard, r: SecurityRuleView): string {
  if (r.custom || !r.why) return '';
  switch (guard) {
    case 'inspect_tools':
      return t(`contentSecurity.toolRule.${r.id}.why`, { defaultValue: r.why });
    case 'content':
      return t(`contentSecurity.contentWhy.${r.id}`, { defaultValue: r.why });
    default:
      return r.why;
  }
}

/** A rule group's heading. */
export function kindName(t: TFunction, kind: string): string {
  return t(`contentSecurity.kind.${kind}`, { defaultValue: kind });
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
