import { Fragment, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import type { Matcher } from '@/lib/security-types';
import { cn } from '@/lib/utils';

/**
 * Translated templates whose placeholders become React nodes. i18next's own
 * interpolation can't do that safely: the values are rule patterns, and a
 * regex such as `(?<name>…)` would be read as markup.
 */
const RAW = { skipInterpolation: true } as const;

function fill(template: string, parts: Record<string, ReactNode>): ReactNode {
  return template.split(/(\{\{\w+\}\})/).map((piece, i) => {
    const name = /^\{\{(\w+)\}\}$/.exec(piece)?.[1];
    return name !== undefined && name in parts ? <Fragment key={i}>{parts[name]}</Fragment> : piece;
  });
}

/** Joins nodes the way the language joins a list (`a, b or c` / `a、b或c`). */
function joined(lang: string, items: ReactNode[], type: 'conjunction' | 'disjunction'): ReactNode {
  const parts = new Intl.ListFormat(lang, { type }).formatToParts(items.map((_, i) => String(i)));
  return parts.map((p, i) =>
    p.type === 'element' ? <Fragment key={i}>{items[Number(p.value)]}</Fragment> : p.value,
  );
}

/** A literal from a rule, in code type. Spaces are kept: ` dan ` differs from `dan`. */
export function Code({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <code
      className={cn(
        'rounded bg-muted px-1 py-px font-mono text-[0.85em] whitespace-pre text-foreground',
        className,
      )}
    >
      {children}
    </code>
  );
}

/** A code-point list: each range in code type, separated by commas. */
export function Codepoints({ ranges }: { ranges: string[] }) {
  return (
    <>
      {ranges.map((r, i) => (
        <Fragment key={r}>
          {i > 0 && ', '}
          <Code>{r}</Code>
        </Fragment>
      ))}
    </>
  );
}

/** What a rule matches, as one sentence. */
export function MatcherText({ m }: { m: Matcher }) {
  const { t, i18n } = useTranslation();
  const lang = i18n.language;
  switch (m.kind) {
    case 'prefix':
      return fill(t('contentSecurity.matcher.prefix', RAW), {
        prefix: <Code>{m.prefix}</Code>,
        n: m.min_tail,
      });
    case 'openai-legacy':
      return fill(t('contentSecurity.matcher.openaiLegacy', RAW), {
        prefix: <Code>sk-</Code>,
        n: m.min_len,
      });
    case 'pem':
      return fill(t('contentSecurity.matcher.pem', RAW), {
        begin: <Code>-----BEGIN … PRIVATE KEY-----</Code>,
      });
    case 'jwt':
      return fill(t('contentSecurity.matcher.jwt', RAW), { alg: <Code>"alg"</Code> });
    case 'conn-string':
      return fill(t('contentSecurity.matcher.connString', RAW), {
        uri: <Code>{t('contentSecurity.matcher.connStringShape')}</Code>,
      });
    case 'private-ip':
      return fill(t('contentSecurity.matcher.privateIp', RAW), {
        ranges: joined(
          lang,
          ['10.', '172.16–31.', '192.168.'].map((p) => <Code key={p}>{p}</Code>),
          'disjunction',
        ),
        loopback: <Code>127.0.0.1</Code>,
      });
    case 'domain-suffix':
      return fill(t('contentSecurity.matcher.domainSuffix', RAW), {
        suffixes: joined(
          lang,
          m.suffixes.map((s) => <Code key={s}>{s}</Code>),
          'disjunction',
        ),
      });
    case 'cn-resident-id':
      return fill(t('contentSecurity.matcher.cnResidentId', RAW), { year: m.born_since });
    case 'bank-card':
      return fill(t('contentSecurity.matcher.bankCard', RAW), {
        networks: joined(
          lang,
          m.networks.map((n) => t(`contentSecurity.cardNetwork.${n.name}`, { defaultValue: n.name })),
          'disjunction',
        ),
      });
    case 'email':
      return fill(t('contentSecurity.matcher.email', RAW), { example: <Code>name@example.com</Code> });
    case 'cn-mobile-phone':
      return t('contentSecurity.matcher.cnMobilePhone');
    case 'regex':
      return fill(t('contentSecurity.matcher.regex', RAW), { pattern: <Code>{m.pattern}</Code> });
    case 'contains':
      return fill(t('contentSecurity.matcher.contains', RAW), { text: <Code>{m.text}</Code> });
    case 'codepoints':
      return fill(t('contentSecurity.matcher.codepoints', RAW), {
        ranges: <Codepoints ranges={m.ranges} />,
      });
    case 'builtin':
      // A check implemented in code: say what it looks for, not how.
      return t(`contentSecurity.check.${m.check}`, { defaultValue: t('contentSecurity.matcher.other') });
    default:
      // A kind this console does not know yet.
      return t('contentSecurity.matcher.other');
  }
}
