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
  'setup.steps.${_}': ['welcome', 'admin', 'settings', 'provider', 'complete'],
  'unifiedLogs.${_}': ['platform', 'audit', 'gateway', 'mcp', 'access', 'app'],
  'unifiedLogs.${_}Desc': ['platform', 'audit', 'gateway', 'mcp', 'access', 'app'],
  'systemStatus.${_}': ['operational', 'degraded', 'down', 'unknown'],
  // Conditional ternary — enumerate the three literal outcomes.
  "common.${_}": ['healthy', 'down', 'unknown'],
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
const T_DYNAMIC = /\bt\(\s*`([^`]*\$\{[^}]*\}[^`]*)`/g;

function extractTKeys(file) {
  const src = readFileSync(file, 'utf8');
  const keys = new Set();
  const dynamic = new Set();
  let m;
  while ((m = T_CALL.exec(src)) !== null) {
    const key = m[2];
    // Heuristic: i18n keys are dotted identifiers, not bare strings.
    if (/^[a-zA-Z][\w]*(\.[a-zA-Z][\w]*)+$/.test(key)) {
      keys.add(key);
    }
  }
  while ((m = T_DYNAMIC.exec(src)) !== null) {
    dynamic.add(m[1]);
  }
  return { keys, dynamic };
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
const dynamicHits = new Map(); // pattern -> file
for (const file of walk(srcDir)) {
  const { keys, dynamic } = extractTKeys(file);
  for (const k of keys) usedKeys.add(k);
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
  console.log(
    `${YELLOW}ℹ${RESET} ${unresolvedPatterns.length} dynamic pattern(s) lack a DYNAMIC_ENUMS entry — add one to enable static checking:`,
  );
  for (const { pattern, file } of unresolvedPatterns) {
    console.log(`    \`${pattern}\` in ${file.replace(webRoot + '/', '')}`);
  }
}

process.exit(failed ? 1 : 0);
