#!/usr/bin/env node
// Verifies that:
//   1. en.json and zh.json have the exact same set of keys (parity)
//   2. Every t('foo.bar') call in src/ refers to a key that exists in en.json
//
// Exits with code 1 on any violation, so it can be wired into CI.
//
// Notes
// - Dynamic keys built from template literals like  t(`foo.${x}`)  are
//   resolved against DYNAMIC_ENUMS below — each pattern lists the enum
//   values it can take, so we can expand+verify the full set. Patterns
//   without an enum entry are reported as "skipped".
// - This script is intentionally dependency-free so it runs anywhere Node 18+ is available.

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const webRoot = resolve(__dirname, '..');
const srcDir = join(webRoot, 'src');
const enPath = join(webRoot, 'src/i18n/en.json');
const zhPath = join(webRoot, 'src/i18n/zh.json');

// Dynamic template-literal patterns → list of enum values the variable(s)
// can take. The pattern is the raw inside of the backticks, e.g.
// `limits.period_${cap.period}` → key "limits.period_${...}". We resolve
// on the literal pattern after collapsing the ${...} placeholder to
// `${_}`, then substitute each value to produce concrete keys.
//
// If a source file's dynamic pattern isn't listed here, it falls back to
// being reported as "skipped" so the author is prompted to add it.
const DYNAMIC_ENUMS = {
  'limits.surfaceShort_${_}': ['ai_gateway', 'mcp_gateway', 'console'],
  'limits.surface_${_}': ['ai_gateway', 'mcp_gateway', 'console'],
  'limits.period_${_}': ['daily', 'weekly', 'monthly'],
  'apiKeys.surface_${_}': ['ai_gateway', 'mcp_gateway', 'console'],
  'apiKeys.surfaceShort_${_}': ['ai_gateway', 'mcp_gateway', 'console'],
  // Mirrors the resource names that the backend hands out in
  // `crates/server/src/handlers/roles.rs`. Keep in sync — an entry that
  // exists in the backend catalog but not here will silently fall
  // through to the raw key (shown uppercased) in the permission tree.
  'permissions.resource.${_}': [
    'ai_gateway', 'mcp_gateway', 'api_keys', 'providers', 'mcp_servers',
    'models', 'users', 'team', 'teams', 'team_members', 'sessions',
    'roles', 'analytics', 'audit_logs', 'logs', 'log_forwarders',
    'webhooks', 'content_filter', 'pii_redactor', 'rate_limits',
    'settings', 'system',
  ],
  'permissions.action.${_}': [
    'use', 'read', 'create', 'update', 'delete', 'rotate', 'rotate_key',
    'revoke', 'write', 'read_own', 'read_team', 'read_all',
    'configure_oidc', 'edit_system',
  ],
  'roles.template_${_}': ['gateway_user', 'read_only', 'ops_admin', 'analytics_only'],
  'logs.preset.${_}': ['last1h', 'last6h', 'last24h', 'last3d', 'last7d', 'last30d'],
  'settings.contentFilter.preset.${_}.name': ['basic', 'strict', 'chinese'],
  'settings.contentFilter.preset.${_}.description': ['basic', 'strict', 'chinese'],
  'mcpStore.category.${_}': [
    'developer', 'database', 'communication', 'cloud',
    'utility', 'knowledge', 'productivity',
  ],
  'setup.steps.${_}': ['welcome', 'admin', 'settings', 'complete'],
  'unifiedLogs.${_}': ['audit', 'gateway', 'mcp', 'access', 'app'],
  'unifiedLogs.${_}Desc': ['audit', 'gateway', 'mcp', 'access', 'app'],
  'systemStatus.${_}': ['operational', 'degraded', 'down', 'unknown'],
  'userLimitOverrides.bulk_${_}': ['disable', 'delete'],
  'userLimitOverrides.expiry_${_}': ['4h', '24h', '7d', '30d', 'custom', 'permanent'],
  'userLimitsTab.source_${_}': ['role', 'override'],
  'users.bulk.action_${_}': ['activate', 'deactivate', 'delete'],
  // Conditional ternary — enumerate the three literal outcomes.
  "common.${_}": ['healthy', 'down', 'unknown'],
  'limits.metric_${_}': ['requests', 'tokens'],
  // Tier names come from crates/server/src/handlers/usage_license.rs::tiers()
  // lowercased client-side. Keep in sync with the backend array.
  'usageLicense.tier.${_}': ['starter', 'growth', 'scale', 'enterprise', 'custom'],
  'analyticsCosts.range_${_}': ['24h', '7d', '30d', 'mtd'],
  'analyticsCosts.group.${_}': ['model', 'user', 'cost_center', 'provider'],
  'dashboard.range.${_}': ['24h', '7d', '30d'],
  'models.costPreview.${_}': ['input', 'output'],
  // Routing strategy / affinity / circuit-breaker enums — kept in
  // lockstep with crates/gateway/src/strategy.rs::RoutingStrategy
  // and crates/gateway/src/router.rs::AffinityMode.
  'models.strategy.${_}': ['weighted', 'latency', 'health', 'latency_health'],
  'models.affinity.${_}': ['none', 'provider', 'route'],
  'models.health.${_}': ['closed', 'open', 'half_open'],
  // describeApiError in src/lib/api.ts builds `errors.byStatus.${n}` /
  // `errors.byType.${t}` off the wire. Types come from AppError in
  // crates/common/src/errors.rs — keep both arrays in sync with it.
  'errors.byStatus.${_}': ['401', '403', '404', '429', '503'],
  'errors.byType.${_}': [
    'unauthorized', 'forbidden', 'not_found', 'bad_request',
    'rate_limited', 'conflict', 'service_unavailable', 'internal_error',
  ],
};

// Normalize an observed dynamic pattern (from source) to the form used as
// a key in DYNAMIC_ENUMS: replace every ${...} with ${_}.
function normalizeDynamic(pattern) {
  return pattern.replace(/\$\{[^}]*\}/g, '${_}');
}

const RED = '\x1b[31m';
const GREEN = '\x1b[32m';
const YELLOW = '\x1b[33m';
const RESET = '\x1b[0m';

function flatten(obj, prefix = '') {
  const out = new Set();
  for (const [k, v] of Object.entries(obj)) {
    const path = prefix ? `${prefix}.${k}` : k;
    if (v && typeof v === 'object' && !Array.isArray(v)) {
      for (const k2 of flatten(v, path)) out.add(k2);
    } else {
      out.add(path);
    }
  }
  return out;
}

function walk(dir, exts = ['.ts', '.tsx']) {
  const out = [];
  for (const name of readdirSync(dir)) {
    if (name.startsWith('.') || name === 'node_modules' || name === 'dist') continue;
    const full = join(dir, name);
    const s = statSync(full);
    if (s.isDirectory()) {
      out.push(...walk(full, exts));
    } else if (exts.some((e) => name.endsWith(e))) {
      out.push(full);
    }
  }
  return out;
}

// Match: t('foo.bar') / t("foo.bar") / t(`foo.bar`) — but NOT t(`foo.${x}`)
// For template literal `t(\`foo.${x}\`)` we report as dynamic.
const T_CALL = /\bt\(\s*(['"`])([^'"`$)]+)\1/g;
// Dynamic forms:
//   (a) direct — t(`foo.${x}`)
//   (b) bare   — `foo.${x}` on its own line (e.g. assigned then passed to t()).
//       We accept any template literal whose prefix looks like a dotted
//       i18n key. False positives are harmless — they just add entries
//       to the "no DYNAMIC_ENUMS" warning list until you resolve them.
const T_DYNAMIC = /\bt\(\s*`([^`]*\$\{[^}]*\}[^`]*)`/g;
const T_DYNAMIC_BARE = /`([a-zA-Z][\w]*(?:\.[a-zA-Z][\w]*)+[^`]*\$\{[^}]*\}[^`]*)`/g;
// Plain string literal that looks like an i18n key — used both for static
// `t(key)` indirection (store key in variable, then call t) and for the
// unused-keys audit below. Same shape as an i18n key: at least one dot.
const KEY_LITERAL = /(['"`])([a-zA-Z][\w]*(?:\.[a-zA-Z][\w]*)+)\1/g;

function extractTKeys(file) {
  const src = readFileSync(file, 'utf8');
  const keys = new Set();
  const literalKeys = new Set();
  const dynamic = new Set();
  let m;
  while ((m = T_CALL.exec(src)) !== null) {
    const key = m[2];
    if (/^[a-zA-Z][\w]*(\.[a-zA-Z][\w]*)+$/.test(key)) keys.add(key);
  }
  while ((m = T_DYNAMIC.exec(src)) !== null) dynamic.add(m[1]);
  while ((m = T_DYNAMIC_BARE.exec(src)) !== null) dynamic.add(m[1]);
  while ((m = KEY_LITERAL.exec(src)) !== null) literalKeys.add(m[2]);
  return { keys, dynamic, literalKeys };
}

// ---------------------------------------------------------------------------
// Step 1: parity check
// ---------------------------------------------------------------------------

const en = JSON.parse(readFileSync(enPath, 'utf8'));
const zh = JSON.parse(readFileSync(zhPath, 'utf8'));
const enKeys = flatten(en);
const zhKeys = flatten(zh);

const missingInZh = [...enKeys].filter((k) => !zhKeys.has(k));
const missingInEn = [...zhKeys].filter((k) => !enKeys.has(k));

let failed = false;

if (missingInZh.length > 0) {
  console.error(`${RED}✗ Keys present in en.json but missing in zh.json:${RESET}`);
  for (const k of missingInZh) console.error(`    ${k}`);
  failed = true;
}
if (missingInEn.length > 0) {
  console.error(`${RED}✗ Keys present in zh.json but missing in en.json:${RESET}`);
  for (const k of missingInEn) console.error(`    ${k}`);
  failed = true;
}
if (!failed) {
  console.log(`${GREEN}✓${RESET} en/zh parity: ${enKeys.size} keys both sides`);
}

// ---------------------------------------------------------------------------
// Step 2: every used key exists in en.json
// ---------------------------------------------------------------------------

const usedKeys = new Set();
const referencedLiterals = new Set(); // strings matching the key shape
const dynamicHits = new Map(); // pattern -> file
for (const file of walk(srcDir)) {
  const { keys, dynamic, literalKeys } = extractTKeys(file);
  for (const k of keys) usedKeys.add(k);
  for (const k of literalKeys) referencedLiterals.add(k);
  for (const d of dynamic) {
    if (!dynamicHits.has(d)) dynamicHits.set(d, file);
  }
}

const undefinedKeys = [...usedKeys].filter((k) => !enKeys.has(k));
if (undefinedKeys.length > 0) {
  console.error(`${RED}✗ Keys referenced in source but missing from en.json:${RESET}`);
  for (const k of undefinedKeys.sort()) console.error(`    ${k}`);
  failed = true;
} else {
  console.log(`${GREEN}✓${RESET} all ${usedKeys.size} statically-used keys exist in en.json`);
}

// ---------------------------------------------------------------------------
// Step 3: expand each dynamic pattern via DYNAMIC_ENUMS and verify
// ---------------------------------------------------------------------------

const unresolvedPatterns = [];
const missingDynamicKeys = [];
const expandedDynamicKeys = new Set();
let resolvedPatternCount = 0;
let resolvedKeyCount = 0;

for (const [pattern, file] of dynamicHits) {
  const norm = normalizeDynamic(pattern);
  const values = DYNAMIC_ENUMS[norm];
  if (!values) {
    unresolvedPatterns.push({ pattern, file });
    continue;
  }
  resolvedPatternCount += 1;
  for (const v of values) {
    const key = norm.replace('${_}', v);
    resolvedKeyCount += 1;
    expandedDynamicKeys.add(key);
    if (!enKeys.has(key)) {
      missingDynamicKeys.push({ key, pattern, file });
    }
  }
}

if (missingDynamicKeys.length > 0) {
  console.error(
    `${RED}✗ Dynamic keys expanded from DYNAMIC_ENUMS but missing from en.json:${RESET}`,
  );
  for (const { key, pattern } of missingDynamicKeys) {
    console.error(`    ${key}  (from \`${pattern}\`)`);
  }
  failed = true;
} else if (resolvedPatternCount > 0) {
  console.log(
    `${GREEN}✓${RESET} ${resolvedPatternCount} dynamic pattern(s) expanded — all ${resolvedKeyCount} enumerated keys exist`,
  );
}

if (unresolvedPatterns.length > 0) {
  console.error(
    `${RED}✗ ${unresolvedPatterns.length} dynamic pattern(s) lack a DYNAMIC_ENUMS entry — declare the enum so all keys can be verified:${RESET}`,
  );
  for (const { pattern, file } of unresolvedPatterns) {
    console.error(`    \`${pattern}\` in ${file.replace(webRoot + '/', '')}`);
  }
  failed = true;
}

// ---------------------------------------------------------------------------
// Step 4: every en.json key is actually referenced somewhere.
//
// A key counts as referenced if it appears as:
//   - a static t('foo.bar') call, OR
//   - a plain string literal matching the dotted-key shape (covers
//     indirection like `const k = 'foo.bar'; t(k)`), OR
//   - an expansion of a dynamic pattern registered in DYNAMIC_ENUMS.
//
// With step 3 failing on unresolved patterns, we know every dynamic-key
// namespace is enumerated — so anything left unreferenced here is dead.
// ---------------------------------------------------------------------------

const unusedKeys = [];
for (const key of enKeys) {
  if (usedKeys.has(key)) continue;
  if (referencedLiterals.has(key)) continue;
  if (expandedDynamicKeys.has(key)) continue;
  unusedKeys.push(key);
}

if (unusedKeys.length > 0) {
  console.error(
    `${RED}✗ ${unusedKeys.length} key(s) in en.json are not referenced anywhere in src/ — delete them or wire them up:${RESET}`,
  );
  for (const k of unusedKeys.sort()) console.error(`    ${k}`);
  failed = true;
} else {
  console.log(`${GREEN}✓${RESET} every en.json key is referenced (no dead keys)`);
}

process.exit(failed ? 1 : 0);
