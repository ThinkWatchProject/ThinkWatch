#!/usr/bin/env node
// Verifies that:
//   1. en.json and zh.json have the exact same set of keys (parity)
//   2. Every t('foo.bar') call in src/ refers to a key that exists in en.json
//
// Exits with code 1 on any violation, so it can be wired into CI.
//
// Notes
// - Dynamic keys built from template literals like  t(`foo.${x}`)  are
//   reported as "dynamic" and skipped (we can't statically resolve them).
// - This script is intentionally dependency-free so it runs anywhere Node 18+ is available.

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const webRoot = resolve(__dirname, '..');
const srcDir = join(webRoot, 'src');
const enPath = join(webRoot, 'src/i18n/en.json');
const zhPath = join(webRoot, 'src/i18n/zh.json');

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

if (dynamicHits.size > 0) {
  console.log(
    `${YELLOW}ℹ${RESET} ${dynamicHits.size} dynamic key pattern(s) skipped (cannot be statically verified):`,
  );
  for (const [pattern, file] of dynamicHits) {
    console.log(`    \`${pattern}\` in ${file.replace(webRoot + '/', '')}`);
  }
}

process.exit(failed ? 1 : 0);
