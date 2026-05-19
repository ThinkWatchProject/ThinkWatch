import { useTranslation } from 'react-i18next';

/// Inline helper under the weight input showing
/// `baseline × weight = $X/M tokens`. Baseline comes from the
/// platform_pricing singleton; when it's unavailable (e.g. no
/// settings:read permission) the preview just renders nothing.
export function CostPreview({
  weight,
  basePerToken,
  currency,
  side,
}: {
  weight: string;
  basePerToken: string | undefined;
  currency: string | undefined;
  side: 'input' | 'output';
}) {
  const { t } = useTranslation();
  if (!basePerToken) return null;
  const w = Number(weight);
  const base = Number(basePerToken);
  if (!Number.isFinite(w) || !Number.isFinite(base) || w <= 0 || base < 0) return null;
  const perMillion = w * base * 1_000_000;
  return (
    <p className="text-[11px] text-muted-foreground font-mono">
      {t(`models.costPreview.${side}` as 'models.costPreview.input', {
        amount: perMillion.toFixed(4),
        currency: currency ?? 'USD',
      })}
    </p>
  );
}
