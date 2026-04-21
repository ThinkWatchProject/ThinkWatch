import { useTranslation } from 'react-i18next';
import { Shield, FileCheck } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';

/**
 * Compliance badges shown in the admin footer / dashboard chrome.
 *
 * Backed by `system_settings` so an operator who completes a SOC 2
 * Type II audit, signs a HIPAA BAA, or otherwise attests to a
 * compliance regime can flip the toggle in admin settings and the
 * badge surfaces immediately to every authenticated user. The
 * tooltip carries the audit date / report URL so a curious enterprise
 * buyer can verify rather than take the marker on faith.
 *
 * The component renders nothing for buyers whose deployment hasn't
 * configured any badges — better blank than misleading.
 */
export interface ComplianceState {
  soc2?: { reportUrl?: string; verifiedAt?: string };
  baa?: { signedAt?: string };
  hipaa?: boolean;
  gdpr?: boolean;
}

export function ComplianceBadges({ state }: { state: ComplianceState }) {
  const { t } = useTranslation();
  const items: { key: string; label: string; tip: string; icon: React.ReactNode }[] = [];

  if (state.soc2) {
    items.push({
      key: 'soc2',
      label: 'SOC 2',
      tip: state.soc2.verifiedAt
        ? t('compliance.soc2VerifiedOn', { date: state.soc2.verifiedAt })
        : t('compliance.soc2Verified'),
      icon: <Shield className="h-3 w-3" />,
    });
  }
  if (state.baa) {
    items.push({
      key: 'baa',
      label: 'BAA',
      tip: state.baa.signedAt
        ? t('compliance.baaSignedOn', { date: state.baa.signedAt })
        : t('compliance.baaSigned'),
      icon: <FileCheck className="h-3 w-3" />,
    });
  }
  if (state.hipaa) {
    items.push({
      key: 'hipaa',
      label: 'HIPAA',
      tip: t('compliance.hipaa'),
      icon: <Shield className="h-3 w-3" />,
    });
  }
  if (state.gdpr) {
    items.push({
      key: 'gdpr',
      label: 'GDPR',
      tip: t('compliance.gdpr'),
      icon: <Shield className="h-3 w-3" />,
    });
  }

  if (items.length === 0) return null;

  return (
    <div className="flex flex-wrap items-center gap-1">
      {items.map((b) => (
        <Tooltip key={b.key}>
          <TooltipTrigger asChild>
            <Badge variant="outline" className="gap-1 text-[10px]">
              {b.icon}
              {b.label}
            </Badge>
          </TooltipTrigger>
          <TooltipContent>{b.tip}</TooltipContent>
        </Tooltip>
      ))}
    </div>
  );
}
