// Types and pure helpers for the Settings page.
//
// Extracted out of the main settings.tsx to make the page file
// merely large instead of unmanageable. The page component still
// owns all editable state and the save flow — this file holds
// the data shapes and the value-coercion helpers.

export interface SystemInfo {
  version: string;
  uptime: string;
  rust_version: string;
  server_host: string;
  gateway_port: number;
  console_port: number;
  public_protocol: string;
  public_host: string;
  public_port: number;
}

export interface OidcConfig {
  issuer_url: string | null;
  client_id: string | null;
  redirect_url: string | null;
  enabled: boolean;
  has_secret?: boolean;
}

export interface AuditConfig {
  clickhouse_url: string;
  clickhouse_db: string;
  connected: boolean;
}

export interface SettingEntry {
  key: string;
  value: unknown;
  category: string;
  description: string;
  updated_at: string;
}

export interface ContentFilterRule {
  name: string;
  pattern: string;
  match_type: 'contains' | 'regex';
  action: 'block' | 'warn' | 'log';
}

export interface ContentFilterPreset {
  id: string;
  rules: ContentFilterRule[];
}

export interface ContentFilterTestMatch {
  name: string;
  pattern: string;
  match_type: string;
  action: string;
  matched_snippet: string;
}

export interface PiiPattern {
  name: string;
  regex: string;
  placeholder_prefix: string;
}

export interface PiiTestMatch {
  name: string;
  original: string;
  placeholder: string;
}

export interface PiiTestResponse {
  redacted_text: string;
  matches: PiiTestMatch[];
}

/// Defensive normalizer for content filter rules loaded from the
/// settings JSON. The DB column is JSONB so anything could be in
/// there; we coerce missing or wrong-typed fields to safe defaults
/// rather than crashing the page.
export function normalizeContentRule(raw: unknown): ContentFilterRule {
  const r = (raw || {}) as Record<string, unknown>;
  return {
    name: typeof r.name === 'string' ? r.name : '',
    pattern: typeof r.pattern === 'string' ? r.pattern : '',
    match_type: r.match_type === 'regex' ? 'regex' : 'contains',
    action: r.action === 'warn' || r.action === 'log' ? r.action : 'block',
  };
}

/// Look up a setting value by `<category>.<shortKey>` from the
/// grouped response of GET /api/admin/settings.
export function getSettingValue(
  settings: Record<string, SettingEntry[]>,
  category: string,
  shortKey: string,
): unknown {
  const entries = settings[category];
  if (!entries) return undefined;
  const fullKey = `${category}.${shortKey}`;
  const entry = entries.find((e) => e.key === fullKey);
  return entry?.value;
}

/// Coerce an unknown setting value into a number with a fallback.
export function num(v: unknown, fallback = 0): number {
  if (v === undefined || v === null || v === '') return fallback;
  const n = Number(v);
  return Number.isNaN(n) ? fallback : n;
}

/// Coerce an unknown setting value into a string with a fallback.
export function str(v: unknown, fallback = ''): string {
  if (v === undefined || v === null) return fallback;
  return String(v);
}
