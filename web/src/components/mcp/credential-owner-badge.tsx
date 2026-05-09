import { useTranslation } from 'react-i18next';
import { ShieldCheck, User } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';

export type CredentialOwner = 'per_user' | 'admin_shared';

const icons: Record<CredentialOwner, typeof User> = {
  per_user: User,
  admin_shared: ShieldCheck,
};

interface Props {
  owner: CredentialOwner;
  /** Icon-only chip with tooltip — for tables where horizontal space
   *  is tight. */
  compact?: boolean;
  className?: string;
}

/**
 * Surfaces "who supplies the credential" alongside `AuthModeBadge`.
 * Same compact-icon shape as AuthModeBadge so they stack visually
 * without the row growing.
 */
export function CredentialOwnerBadge({ owner, compact = false, className }: Props) {
  const { t } = useTranslation();
  const Icon = icons[owner];
  const title = t(`mcpServers.credentialOwnerBadge.${owner}.title`);
  const description = t(`mcpServers.credentialOwnerBadge.${owner}.description`);

  if (compact) {
    return (
      <Tooltip>
        <TooltipTrigger asChild>
          <span
            className={cn(
              'inline-flex h-6 w-6 items-center justify-center rounded-md border bg-background text-muted-foreground',
              className,
            )}
            aria-label={title}
          >
            <Icon className="h-3.5 w-3.5" />
          </span>
        </TooltipTrigger>
        <TooltipContent side="top">
          <div className="text-xs font-medium">{title}</div>
          <div className="text-xs text-muted-foreground max-w-[280px]">{description}</div>
        </TooltipContent>
      </Tooltip>
    );
  }

  return (
    <Badge variant="outline" className={cn('gap-1.5 font-normal', className)}>
      <Icon className="h-3.5 w-3.5" />
      {title}
    </Badge>
  );
}
