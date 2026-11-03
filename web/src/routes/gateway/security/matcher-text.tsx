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

/**
 * A built-in rule's description as the server writes it: text in backticks
 * is code, shown in code type without the backticks.
 */
export function About({ text }: { text: string }) {
  return (
    <>
      {text.split('`').map((piece, i) =>
        i % 2 === 1 ? <Code key={i}>{piece}</Code> : <Fragment key={i}>{piece}</Fragment>,
      )}
    </>
  );
}

/**
 * What a rule matches, as one sentence: a custom rule's pattern, a content
 * rule's code points. A built-in redaction rule is described by the server
 * (`about`, see {@link About}); a check implemented in code, or a matcher
 * this console does not know, reads as a built-in check.
 */
export function MatcherText({ m }: { m: Matcher }) {
  const { t } = useTranslation();
  switch (m.kind) {
    case 'regex':
      return fill(
        t(m.case_sensitive ? 'contentSecurity.matcher.regexCaseSensitive' : 'contentSecurity.matcher.regex', RAW),
        { pattern: <Code>{m.pattern}</Code> },
      );
    case 'contains':
      return fill(
        t(
          m.case_sensitive ? 'contentSecurity.matcher.containsCaseSensitive' : 'contentSecurity.matcher.contains',
          RAW,
        ),
        { text: <Code>{m.text}</Code> },
      );
    case 'codepoints':
      return fill(t('contentSecurity.matcher.codepoints', RAW), {
        ranges: <Codepoints ranges={m.ranges} />,
      });
    default:
      return t('contentSecurity.matcher.other');
  }
}
