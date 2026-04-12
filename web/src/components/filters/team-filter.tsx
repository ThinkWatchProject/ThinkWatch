import { useTranslation } from 'react-i18next';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import type { TeamSummary } from '@/lib/types';

interface TeamFilterProps {
  teams: TeamSummary[];
  value: string;
  onChange: (teamId: string) => void;
}

export function TeamFilter({ teams, value, onChange }: TeamFilterProps) {
  const { t } = useTranslation();
  if (teams.length === 0) return null;
  return (
    <div className="flex items-center gap-2">
      <span className="text-sm text-muted-foreground">{t('analytics.teamFilter')}</span>
      <Select value={value || 'all'} onValueChange={(v) => onChange(v === 'all' ? '' : v)}>
        <SelectTrigger className="w-48">
          <SelectValue placeholder={t('analytics.allTeams')} />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="all">{t('analytics.allTeams')}</SelectItem>
          {teams.map((team) => (
            <SelectItem key={team.id} value={team.id}>{team.name}</SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
