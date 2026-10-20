import { useId, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { MonitorDown } from 'lucide-react';
import { useQuery } from '@tanstack/react-query';
import { Button } from '@/components/ui/button';
import { useGatewayUrl } from '@/hooks/use-gateway-url';
import { hasPermission } from '@/lib/api';
import { keyRulesQuery } from '@/components/limits/key-limits-query';
import {
  LITE_LIMITS_MIN_VERSION,
  LITE_MIN_VERSION,
  liteDownloadUrl,
  liteImportLink,
  liteLimits,
} from '@/lib/lite-link';
import { openAppLink } from '@/lib/open-app-link';

interface AddToLiteProps {
  /// The plaintext key, as shown on this screen.
  apiKey: string;
  /// The key's id, where the screen has it: the link then carries the
  /// key's own request limits, read with `rate_limits:read`. Without the
  /// id, the permission, or a readable answer, the link goes without them.
  keyId?: string;
  /// Whether the key may call the AI gateway (`ai_gateway` in its surfaces).
  aiGateway: boolean;
  /// Whether the key's owner holds `ai_gateway:use`, without which the
  /// gateway refuses every request the key makes. Keys are created for
  /// oneself, so it defaults to the signed-in user's permission; first-run
  /// setup, which has no permissions loaded yet, says so itself.
  ownerMayUseGateway?: boolean;
  /// The screen's Copy button, which goes beside "Add to ThinkWatch Lite".
  children?: ReactNode;
}

/// "Add to ThinkWatch Lite" for a newly shown key. It opens a
/// `thinkwatch://import` link with this gateway's address and the key,
/// built here in the browser, through a hidden iframe: the page itself
/// never navigates, so a browser without Lite cannot replace it — and the
/// key on it — with an error page. Lite is a desktop app: below the `md`
/// breakpoint (where the console's mobile layout starts) only the Copy
/// button shows.
export function AddToLite({ apiKey, keyId, aiGateway, ownerMayUseGateway, children }: AddToLiteProps) {
  const { t, i18n } = useTranslation();
  const { baseUrl, known, failed } = useGatewayUrl();
  const [opened, setOpened] = useState(false);
  const reasonId = useId();
  const limitsId = useId();
  const mayUseGateway = ownerMayUseGateway ?? hasPermission('ai_gateway:use');

  // Until the rules come (and when they cannot be read) the link goes
  // without limits rather than holding the button back.
  const rules = useQuery({
    ...keyRulesQuery(keyId ?? ''),
    enabled: !!keyId && aiGateway && hasPermission('rate_limits:read'),
    retry: false,
  });
  const limits = rules.data ? liteLimits(rules.data) : [];
  const built = known ? liteImportLink(baseUrl, apiKey, limits) : undefined;
  const link = built && 'link' in built ? built.link : undefined;
  const withLimits = !!link && limits.length > 0;
  const minVersion = withLimits ? LITE_LIMITS_MIN_VERSION : LITE_MIN_VERSION;
  let reason: string | undefined;
  if (!aiGateway) {
    reason = t('lite.reasonNoAiGateway');
  } else if (!mayUseGateway) {
    reason = t('lite.reasonNoGatewayUse');
  } else if (failed) {
    reason = t('lite.reasonNoGatewayUrl');
  } else if (built && 'problem' in built) {
    reason =
      built.problem === 'insecure-url'
        ? t('lite.reasonInsecureUrl', { url: baseUrl })
        : built.problem === 'bad-url'
          ? t('lite.reasonBadUrl', { url: baseUrl })
          : t('lite.reasonBadKey');
  }

  const noResponse = t('lite.noResponse');
  const label = (
    <>
      <MonitorDown className="h-4 w-4" />
      {t('lite.add')}
    </>
  );

  return (
    // Without a Copy button beside it, the whole block is desktop-only.
    <div className={children ? 'space-y-2' : 'hidden space-y-2 md:block'}>
      <div className="flex gap-2 *:flex-1">
        {children}
        {link && !reason ? (
          <Button
            variant="outline"
            className="hidden md:inline-flex"
            aria-describedby={withLimits ? limitsId : undefined}
            onClick={() => {
              openAppLink(link);
              setOpened(true);
            }}
          >
            {label}
          </Button>
        ) : (
          <Button
            variant="outline"
            className="hidden md:inline-flex"
            disabled
            aria-describedby={reason ? reasonId : undefined}
          >
            {label}
          </Button>
        )}
      </div>
      {reason ? (
        <p id={reasonId} className="hidden text-xs text-muted-foreground md:block">
          {reason}
        </p>
      ) : (
        withLimits && (
          <p id={limitsId} className="hidden text-xs text-muted-foreground md:block">
            {t('lite.limitsIncluded', { version: LITE_LIMITS_MIN_VERSION })}
          </p>
        )
      )}
      {!reason && opened && (
        <p className="hidden text-xs text-muted-foreground md:block">
          {noResponse}
          {/* A space after "?", none after the full-width "？". */}
          {/[\x21-\x7e]$/.test(noResponse) ? ' ' : ''}
          <a
            href={liteDownloadUrl(i18n.language)}
            target="_blank"
            rel="noopener noreferrer"
            className="text-foreground underline underline-offset-4"
          >
            {t('lite.installFirst', { version: minVersion })}
          </a>
        </p>
      )}
    </div>
  );
}
