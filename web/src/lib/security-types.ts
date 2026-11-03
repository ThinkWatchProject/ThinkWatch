// Types for the three request guards — outbound redaction, tool-call
// inspection and the content filter.
//
// Hand-written mirrors of thinkwatch-core's `tw_guard` types: the backend
// passes core's JSON through and documents it in OpenAPI only as plain JSON,
// so `pnpm gen:api` cannot produce these. The read side (`SecurityDetail`,
// the test request and result) is `tw_guard::view` and `tw_guard::trial` —
// the same JSON the desktop app's `/security` returns; the write side
// (`*Policy`) is the value stored under each `security.*` settings key,
// `tw_guard::policy`. Keep them in step with core (its `ts` feature exports
// the read side under these names).

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
 * What a rule does when its guard is on (`enforce`); a guard that monitors
 * (`observe`) only records, whatever the rule says. Redaction:
 * `placeholder` / `record`; tool calls: `cut` / `record`; content:
 * `block` / `strip` / `replace` / `record`.
 */
export type RuleAction = 'placeholder' | 'cut' | 'block' | 'strip' | 'replace' | 'record';
export type RedactAction = 'placeholder' | 'record';
export type ToolAction = 'cut' | 'record';
export type ContentAction = 'block' | 'strip' | 'replace' | 'record';

/**
 * What a content rule checks: the request (the caller's messages and tool
 * results) and/or the answer's text (not its tool calls, not its thinking).
 */
export type RuleScope = 'request' | 'response';

/** The text a `replace` content rule writes in place of a hit when it names none. */
export const DEFAULT_REPLACEMENT = '***';

/** How a content rule matches. */
export type ContentMatch = 'contains' | 'regex' | 'codepoints';

/** How a custom redaction rule matches: the text as written, or a regex. */
export type RedactMatch = Exclude<ContentMatch, 'codepoints'>;

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

/** One prefix of a `prefixes` matcher and what must follow it: any token characters, hex digits, or letters and digits. */
export interface PrefixView {
  prefix: string;
  min_tail: number;
  tail: 'any' | 'hex' | 'alnum';
}

/**
 * What a rule matches, for display. Built-in rules use the specific kinds;
 * custom rules are `regex`, `contains` or `codepoints`. `case_sensitive`
 * is left out when false: tool-call regexes are case-sensitive, a custom
 * redaction or content rule is when it says so. A built-in rule's
 * `codepoints.ranges` is the canonical spelling (`U+200B`,
 * `U+E0000–U+E007F`); a custom rule's is the items as written, so joining
 * them with `, ` gives back what is stored. `builtin` is a check
 * implemented in code, with nothing to show but its stable name: a
 * tool-call check's name, or the id of a redaction rule that checks digits,
 * word lists or where a value sits. A kind the console does not know yet is
 * shown with a generic description.
 */
export type Matcher =
  | { kind: 'prefix'; prefix: string; min_tail: number }
  | { kind: 'prefixes'; prefixes: PrefixView[] }
  | { kind: 'saved' }
  | { kind: 'assignment'; suffixes: string[] }
  | { kind: 'labeled'; names: string[] }
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
  | { kind: 'regex'; pattern: string; case_sensitive?: boolean }
  | { kind: 'contains'; text: string; case_sensitive?: boolean }
  | { kind: 'codepoints'; ranges: string[] }
  | { kind: 'builtin'; check: string };

/** One rule, built-in or custom, as `GET /api/admin/security` lists it. */
export interface SecurityRuleView {
  /** A built-in rule's id, or a custom rule's name. */
  id: string;
  custom: boolean;
  /** English name. A custom rule's name, in no particular language. */
  name: string;
  /** Chinese name: every built-in rule has one; custom rules do not. */
  name_zh?: string;
  /** Why the rule is worth a look (English). Absent where the name says it, and on custom rules. */
  why?: string;
  /** The same, in Chinese. */
  why_zh?: string;
  /**
   * What a built-in redaction rule finds, as a sentence (English); text in
   * backticks is code. Absent on the other guards and on custom rules.
   */
  about?: string;
  /** The same, in Chinese. */
  about_zh?: string;
  /**
   * Group: the `slug` of one of the guard's `kinds`. Custom rules: `custom`.
   */
  kind: string;
  matcher: Matcher;
  enabled: boolean;
  /** Whether it is on out of the box. `true` for custom rules. */
  on_by_default: boolean;
  /** What it does when the guard is on. */
  action?: RuleAction | null;
  /** Built-in rules: the factory action. */
  default_action?: RuleAction | null;
  /** Redaction: the placeholder label (`SECRET`, `EMAIL` …), built-in and custom. */
  label?: string | null;
  /** Content rules: what they check. Built-in rules check requests only. */
  scope?: RuleScope[] | null;
  /** Content rules whose action is `replace`: the text a hit becomes. */
  replacement?: string | null;
}

/** A group of a guard's rule table, named in both languages. */
export interface SecurityKindView {
  /** What a rule's `kind` says. */
  slug: string;
  name: string;
  name_zh: string;
}

export interface GuardDetail {
  mode: GuardMode;
  /** In display order: built-in groups first, custom rules last. */
  rules: SecurityRuleView[];
  /** The groups, in display order; every rule's `kind` is one of them, `custom` last. */
  kinds: SecurityKindView[];
}

/**
 * A redaction exception value: never replaced or recorded, whichever rule
 * finds it. Only its SHA-256 and masked form are kept, never the value.
 */
export interface RedactException {
  sha256: string;
  masked: string;
  /** The rule that first recognised it; absent when none did. */
  rule?: string;
  /** Milliseconds since the epoch. */
  added_at: number;
}

/** `GET /api/admin/security/redact/exceptions`. */
export interface RedactExceptionList {
  items: RedactException[];
  /** How many there may be. */
  max: number;
}

/** `GET /api/admin/security`. */
export interface SecurityDetail {
  redact: GuardDetail;
  inspect_tools: GuardDetail;
  content: GuardDetail;
}

/**
 * `POST /api/admin/security/{guard}/test`. With `pattern` only that pattern
 * is tried (content and redaction: matched by `match`, redaction also by
 * `case_sensitive` and replaced under `label`; tool calls: a regex);
 * with `rule` only that built-in rule, even when it is off; with neither,
 * every rule that is on.
 */
export interface SecurityTestRequest {
  sample: string;
  pattern?: string | null;
  /** Left out: `contains`. Redaction takes `contains` or `regex` only. */
  match?: ContentMatch | null;
  /** Redaction and content. Left out: ignoring case. */
  case_sensitive?: boolean | null;
  rule?: string | null;
  label?: string | null;
  /**
   * What the rule tried (`pattern` or `rule`) does when the guard is on;
   * `output` and `refused` follow it. Left out: the saved action
   * (`placeholder` for a redaction pattern, `record` for the others).
   */
  action?: RuleAction | null;
  /** Content: the side the sample is tried as. Left out: the request. */
  scope?: RuleScope | null;
  /** Content, `action: replace`: the text a hit becomes. Left out: `***`. */
  replacement?: string | null;
}

export interface SecurityTestHit {
  /** A built-in rule's id, a custom rule's name, or `trial` for a tried `pattern`. */
  rule: string;
  custom: boolean;
  /** Position in the sample, in UTF-16 code units — JavaScript string indices. */
  start: number;
  end: number;
  /**
   * Redaction: the masked value; the others: the matched text. Invisible
   * characters are drawn as `‹U+200B›`, a run as `‹U+E0049 ×12›`.
   */
  excerpt: string;
  /** What the rule does when the guard is on. Absent on an exception value. */
  action?: RuleAction | null;
  /**
   * Redaction: the value is an exception (`security.redact.exceptions`):
   * recognised, but neither replaced nor recorded, and left as it is in
   * `output`.
   */
  excepted?: boolean;
}

export interface SecurityTestResult {
  hits: SecurityTestHit[];
  /**
   * The sample as it would be sent (or, tried as an answer, received) with
   * the guard on: redaction replaced, content stripped or replaced. `null`
   * when nothing changes, or when the content filter would refuse the
   * request.
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
  /** The text to find, or a regular expression, as `match` says. */
  pattern: string;
  /** Left out: `contains` — the text as written, `.` and `*` included. */
  match?: RedactMatch;
  /** Left out: ignoring case. */
  case_sensitive?: boolean;
  /** Placeholder label, `^[A-Z][A-Z0-9_]{0,23}$`. Left out: `SECRET`. */
  label?: string;
  /** Left out: `placeholder`. */
  action?: RedactAction;
  disabled?: boolean;
}

export interface RedactPolicy {
  mode?: GuardMode;
  /** Built-in rules turned on that are off out of the box. */
  enable?: string[];
  /** Built-in rules turned off that are on out of the box. */
  disable?: string[];
  /** Built-in rules whose action differs from the factory one. */
  actions?: Record<string, RedactAction>;
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
  /** Left out: `request`. With `response`, `block` and `codepoints` are refused. */
  scope?: RuleScope[];
  /** Left out: `contains`. */
  match?: ContentMatch;
  /** Left out: ignoring case. */
  case_sensitive?: boolean;
  /** Left out: `record`. */
  action?: ContentAction;
  /** `action: replace` only. Left out: `***`. */
  replacement?: string;
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
