import { useTranslation } from 'react-i18next';

/**
 * Inline red asterisk for required-field labels. Carries an
 * aria-label so screen readers announce "required" instead of an
 * unannounced glyph; the visual `*` is hidden from the accessibility
 * tree to avoid a duplicate announcement.
 */
export function RequiredMark() {
  const { t } = useTranslation();
  return (
    <span
      aria-label={t('common.required')}
      className="text-destructive font-semibold"
    >
      <span aria-hidden="true">*</span>
    </span>
  );
}
