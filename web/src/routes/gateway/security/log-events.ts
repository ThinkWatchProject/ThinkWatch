// The guard events of the audit log, read back for the security page's log.

import type { TFunction } from 'i18next';
import { GUARDS, type Guard, type RuleAction } from '@/lib/security-types';
import { actionName } from './names';

/**
 * The audit events each guard writes, one per rule that matched in a
 * request (see the gateway's `guards` module). The log forwarders push the
 * same events out; this is where the console reads them back.
 */
export const GUARD_ACTIONS: Record<Guard, readonly string[]> = {
  redact: ['gateway.redaction_flagged', 'gateway.redaction_replaced'],
  inspect_tools: ['gateway.tool_call_flagged', 'gateway.tool_call_blocked'],
  content: [
    'gateway.content_flagged',
    'gateway.content_stripped',
    'gateway.content_replaced',
    'gateway.content_blocked',
  ],
};

/** One row of `GET /api/audit/logs`, as far as the guard events go. */
export interface GuardEvent {
  id: string;
  action: string;
  user_email?: string | null;
  user_id?: string | null;
  api_key_id?: string | null;
  created_at: string;
  detail?: GuardEventDetail | null;
}

/** What a guard event's `detail` carries. Every field is optional: older events lack the newer ones. */
export interface GuardEventDetail {
  trace_id?: string;
  model?: string;
  rule?: string;
  rule_name?: string;
  custom?: boolean;
  /** The rule's own action. */
  action?: string;
  /** What happened: `recorded`, `replaced`, `stripped`, `blocked`, `cut`. */
  outcome?: string;
  /** The guard's state: `observe` (monitor) or `enforce`. */
  mode?: string;
  /** Content: `request` or `response`. */
  direction?: string;
  in_tool_result?: boolean;
  count?: number;
  values?: number;
  tool?: string;
}

/** The guard an event belongs to. */
export function guardOf(action: string): Guard | null {
  return GUARDS.find((g) => GUARD_ACTIONS[g].includes(action)) ?? null;
}

/** Where in the request or the answer an event's hit was. */
export function partOf(t: TFunction, guard: Guard, d: GuardEventDetail): string {
  if (guard === 'inspect_tools') {
    return d.tool
      ? t('contentSecurity.log.part.toolCallNamed', { tool: d.tool })
      : t('contentSecurity.log.part.toolCall');
  }
  if (guard === 'content' && d.direction === 'response') return t('contentSecurity.log.part.answerText');
  if (guard === 'content' && d.in_tool_result) return t('contentSecurity.log.part.toolResult');
  return t('contentSecurity.log.part.request');
}

/**
 * What became of a hit, in the words of the rule actions: replaced with a
 * placeholder, cut off, refused, deleted or replaced (with how many places),
 * or recorded only.
 */
export function outcomeOf(t: TFunction, action: string, d: GuardEventDetail): { text: string; tone: RuleAction } {
  const n = d.count ?? d.values ?? 0;
  switch (action) {
    case 'gateway.redaction_replaced':
      return { text: actionName(t, 'placeholder'), tone: 'placeholder' };
    case 'gateway.tool_call_blocked':
      return { text: actionName(t, 'cut'), tone: 'cut' };
    case 'gateway.content_blocked':
      return { text: actionName(t, 'block'), tone: 'block' };
    case 'gateway.content_stripped':
      return { text: t('contentSecurity.log.stripped', { count: n }), tone: 'strip' };
    case 'gateway.content_replaced':
      return { text: t('contentSecurity.log.replaced', { count: n }), tone: 'replace' };
    default:
      return { text: actionName(t, 'record'), tone: 'record' };
  }
}
