// Provisional types for the three request guards — outbound redaction,
// tool-call inspection and the content filter.
//
// Written by hand from the guard-unify interface contract until the
// backend's OpenAPI schema carries them; replace with the generated types
// then. The read side (`SecurityDetail` and the test result) is the same
// JSON the desktop app's `/security` returns: thinkwatch-core's
// `tw_guard::view` and `tw_guard::trial`. The write side (`*Policy`) is the
// value stored under each `security.*` settings key: `tw_guard::policy`.

/** One of the three guards. Also the path segment in `/security/{guard}/test`. */
export type Guard = 'redact' | 'inspect_tools' | 'content';

/** In page order. */
export const GUARDS: readonly Guard[] = ['redact', 'inspect_tools', 'content'];

/** The settings key each guard's policy is stored under (`PATCH /api/admin/settings`). */
export const SETTING_KEYS: Record<Guard, string> = {
  redact: 'security.redact',
  inspect_tools: 'security.inspect_tools',
  content: 'security.content',
};

export type GuardMode = 'off' | 'observe' | 'enforce';

/**
 * What a rule does in the third mode. Tool-call inspection: `cut` /
 * `record`; content filter: `block` / `strip` / `record`. Outbound
 * redaction has no per-rule action — a hit is always replaced.
 */
export type RuleAction = 'cut' | 'block' | 'strip' | 'record';
export type ToolAction = 'cut' | 'record';
export type ContentAction = 'block' | 'strip' | 'record';

/** How a content rule matches. */
export type ContentMatch = 'contains' | 'regex' | 'codepoints';

export interface CardPrefix {
  from: number;
  to: number;
}

export interface CardNetwork {
  /** English name (`UnionPay`, `Visa` …). */
  name: string;
  prefixes: CardPrefix[];
  lengths: number[];
}

/**
 * What a rule matches, for display. Built-in rules use the specific kinds;
 * custom rules are `regex`, `contains` or `codepoints`. `codepoints.ranges`
 * is the canonical spelling (`U+200B`, `U+E0000–U+E007F`).
 *
 * `email` and `cn-mobile-phone` are the two personal-information rules the
 * contract adds; their kind names are a guess the backend may not share —
 * any kind the console does not know is shown with a generic description.
 */
export type Matcher =
  | { kind: 'prefix'; prefix: string; min_tail: number }
  | { kind: 'openai-legacy'; min_len: number }
  | { kind: 'pem' }
  | { kind: 'jwt' }
  | { kind: 'conn-string' }
  | { kind: 'private-ip' }
  | { kind: 'domain-suffix'; suffixes: string[] }
  | { kind: 'cn-resident-id'; born_since: number }
  | { kind: 'bank-card'; networks: CardNetwork[] }
  | { kind: 'email' }
  | { kind: 'cn-mobile-phone' }
  | { kind: 'regex'; pattern: string }
  | { kind: 'contains'; text: string }
  | { kind: 'codepoints'; ranges: string[] };

/** One rule, built-in or custom, as `GET /api/admin/security` lists it. */
export interface SecurityRuleView {
  /** A built-in rule's id, or a custom rule's name. */
  id: string;
  custom: boolean;
  /** English name; the console looks the id up first. A custom rule's name. */
  name: string;
  /** Why the rule is worth a look (English). Absent on redaction and custom rules. */
  why?: string | null;
  /**
   * Group. Redaction: `api-keys` `private-keys` `jwt` `conn-strings`
   * `personal` `internal`; tool calls: `command`; content: `invisible`
   * `injection` `persona` `chinese`. Custom rules: `custom`.
   */
  kind: string;
  matcher: Matcher;
  enabled: boolean;
  /** Whether it is on out of the box. `true` for custom rules. */
  on_by_default: boolean;
  /** Tool calls and content: what it does in the third mode. */
  action?: RuleAction | null;
  /** Built-in tool and content rules: the factory action. */
  default_action?: RuleAction | null;
  /** Redaction: the placeholder label (`SECRET`, `EMAIL` …), built-in and custom. */
  label?: string | null;
}

export interface GuardDetail {
  mode: GuardMode;
  /** In display order: built-in groups first, custom rules last. */
  rules: SecurityRuleView[];
}

/** `GET /api/admin/security`. */
export interface SecurityDetail {
  redact: GuardDetail;
  inspect_tools: GuardDetail;
  content: GuardDetail;
}

/**
 * `POST /api/admin/security/{guard}/test`. With `pattern` only that pattern
 * is tried (content: matched by `match`, redaction: replaced under `label`);
 * with `rule` only that built-in rule, even when it is off; with neither,
 * every rule that is on.
 */
export interface SecurityTestRequest {
  sample: string;
  pattern?: string | null;
  match?: ContentMatch | null;
  rule?: string | null;
  label?: string | null;
}

export interface SecurityTestHit {
  rule: string;
  custom: boolean;
  /** Position in the sample, in UTF-16 code units — JavaScript string indices. */
  start: number;
  end: number;
  excerpt: string;
  /** Tool calls and content: what the rule does in the third mode. */
  action?: RuleAction | null;
}

export interface SecurityTestResult {
  hits: SecurityTestHit[];
  /**
   * The sample as it would be sent in the third mode: redaction replaced,
   * content stripped. `null` when nothing changes.
   */
  output: string | null;
  /** Content filter: a `block` rule matched, so the request would not be sent. */
  refused: boolean;
}

// ---------------------------------------------------------------------------
// Stored policies — the JSON under each `security.*` settings key. Every
// field may be left out and means the factory value then.
// ---------------------------------------------------------------------------

export interface CustomRedactRule {
  /** Also the rule's identity: unique within the guard. */
  name: string;
  /** Regular expression. */
  pattern: string;
  /** Placeholder label, `^[A-Z][A-Z0-9_]{0,23}$`. Left out: `SECRET`. */
  label?: string;
  disabled?: boolean;
}

export interface RedactPolicy {
  mode?: GuardMode;
  /** Built-in rules turned on that are off out of the box. */
  enable?: string[];
  /** Built-in rules turned off that are on out of the box. */
  disable?: string[];
  custom?: CustomRedactRule[];
}

export interface CustomToolRule {
  name: string;
  /** Regular expression, matched against a tool call's arguments. */
  pattern: string;
  /** Left out: `record`. */
  action?: ToolAction;
  disabled?: boolean;
}

export interface ToolPolicy {
  mode?: GuardMode;
  enable?: string[];
  disable?: string[];
  /** Built-in rules whose action differs from the factory one. */
  actions?: Record<string, ToolAction>;
  custom?: CustomToolRule[];
}

export interface CustomContentRule {
  name: string;
  pattern: string;
  /** Left out: `contains`. */
  match?: ContentMatch;
  /** Left out: `record`. */
  action?: ContentAction;
  disabled?: boolean;
}

export interface ContentPolicy {
  mode?: GuardMode;
  enable?: string[];
  disable?: string[];
  actions?: Record<string, ContentAction>;
  custom?: CustomContentRule[];
}

export interface GuardPolicies {
  redact: RedactPolicy;
  inspect_tools: ToolPolicy;
  content: ContentPolicy;
}
