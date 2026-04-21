import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';

/**
 * Locale-aware number / currency formatters as a single hook.
 *
 * Each route used to reach for `new Intl.NumberFormat(locale, …)`
 * inline; this hook returns memoised formatter functions keyed on
 * the active i18next language so callers stay declarative
 * (`fmtUsd(value)`) and the formatter instances are reused across
 * renders. The mapping from i18next code (`zh`) to BCP 47
 * (`zh-CN`) is centralised here so future locales add to one place.
 */
export function useIntlFormat() {
  const { i18n } = useTranslation();
  const locale = useMemo(() => {
    switch (i18n.language) {
      case 'zh':
        return 'zh-CN';
      case 'en':
      default:
        return 'en-US';
    }
  }, [i18n.language]);

  return useMemo(() => {
    const compact = new Intl.NumberFormat(locale, {
      notation: 'compact',
      maximumFractionDigits: 1,
    });
    const usd = new Intl.NumberFormat(locale, {
      style: 'currency',
      currency: 'USD',
      maximumFractionDigits: 2,
    });
    const int = new Intl.NumberFormat(locale);
    const percent = new Intl.NumberFormat(locale, {
      style: 'percent',
      maximumFractionDigits: 1,
    });
    return {
      locale,
      fmtCompact: (v: number) => compact.format(v),
      fmtUsd: (v: number) => usd.format(v),
      fmtInt: (v: number) => int.format(v),
      fmtPercent: (v: number) => percent.format(v),
    };
  }, [locale]);
}
