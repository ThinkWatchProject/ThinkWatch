import { useEffect, useMemo, useState } from 'react';
import { useNavigate } from '@tanstack/react-router';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import {
  Search, Server, Store, Wrench, Key, FileText, Users, Building2, Shield, Settings,
  Sliders, BookOpen, Activity, DollarSign, UserCog, Braces, Radio,
} from 'lucide-react';
import { cn } from '@/lib/utils';

interface CmdAction {
  id: string;
  labelKey: string;
  to: string;
  icon: typeof Server;
  group: 'overview' | 'mcp' | 'gateway' | 'analytics' | 'admin' | 'other';
  keywords?: string;
}

const ACTIONS: CmdAction[] = [
  { id: 'dashboard',       labelKey: 'nav.dashboard',       to: '/',                   icon: Activity,   group: 'overview',  keywords: 'home index overview' },
  { id: 'mcp.servers',     labelKey: 'nav.mcpServers',      to: '/mcp/servers',        icon: Server,     group: 'mcp',       keywords: 'mcp server endpoint' },
  { id: 'mcp.store',       labelKey: 'nav.mcpStore',        to: '/mcp/store',          icon: Store,      group: 'mcp',       keywords: 'market template install' },
  { id: 'mcp.tools',       labelKey: 'nav.tools',        to: '/mcp/tools',          icon: Wrench,     group: 'mcp' },
  { id: 'gw.providers',    labelKey: 'nav.providers',       to: '/gateway/providers',  icon: Radio,      group: 'gateway',   keywords: 'openai anthropic google azure bedrock' },
  { id: 'gw.models',       labelKey: 'nav.models',          to: '/gateway/models',     icon: Braces,     group: 'gateway' },
  { id: 'gw.security',     labelKey: 'nav.contentSecurity', to: '/gateway/security',   icon: Shield,     group: 'gateway',   keywords: 'content filter pii' },
  { id: 'api-keys',        labelKey: 'nav.apiKeys',         to: '/api-keys',           icon: Key,        group: 'gateway' },
  { id: 'logs',            labelKey: 'nav.logs',            to: '/logs',               icon: FileText,   group: 'analytics' },
  { id: 'usage',           labelKey: 'nav.usage',           to: '/analytics/usage',    icon: Activity,   group: 'analytics' },
  { id: 'costs',           labelKey: 'nav.costs',           to: '/analytics/costs',    icon: DollarSign, group: 'analytics' },
  { id: 'users',           labelKey: 'nav.users',           to: '/admin/users',        icon: UserCog,    group: 'admin' },
  { id: 'teams',           labelKey: 'nav.teams',           to: '/admin/teams',        icon: Users,      group: 'admin' },
  { id: 'roles',           labelKey: 'nav.roles',           to: '/admin/roles',        icon: Shield,     group: 'admin',     keywords: 'rbac permissions' },
  { id: 'settings',        labelKey: 'nav.settings',        to: '/admin/settings',     icon: Settings,   group: 'admin' },
  { id: 'log-forwarders',  labelKey: 'nav.logForwarders',   to: '/logs/forwarders',    icon: Sliders,    group: 'admin' },
  { id: 'api-docs',        labelKey: 'nav.apiDocs',         to: '/admin/api-docs',     icon: BookOpen,   group: 'admin' },
  { id: 'profile',         labelKey: 'auth.profile',         to: '/profile',            icon: Building2,  group: 'other' },
  { id: 'guide',           labelKey: 'nav.configGuide',           to: '/guide',              icon: BookOpen,   group: 'other' },
];

export function CommandPalette() {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [activeIdx, setActiveIdx] = useState(0);
  const navigate = useNavigate();
  const { t } = useTranslation();

  // Global shortcut: Cmd+K / Ctrl+K
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.key === 'k' || e.key === 'K') && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen((v) => !v);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  // Reset state on open
  useEffect(() => {
    if (open) {
      setQuery('');
      setActiveIdx(0);
    }
  }, [open]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return ACTIONS;
    return ACTIONS.filter((a) => {
      const label = t(a.labelKey).toLowerCase();
      const kw = a.keywords?.toLowerCase() ?? '';
      return label.includes(q) || kw.includes(q) || a.id.includes(q);
    });
  }, [query, t]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setActiveIdx((i) => Math.min(filtered.length - 1, i + 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setActiveIdx((i) => Math.max(0, i - 1));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const pick = filtered[activeIdx];
      if (pick) {
        setOpen(false);
        navigate({ to: pick.to });
      }
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogContent className="p-0 gap-0 overflow-hidden max-w-xl">
        <div className="flex items-center gap-2 border-b px-3 py-2">
          <Search className="h-4 w-4 text-muted-foreground" />
          <Input
            autoFocus
            placeholder={t('commandPalette.placeholder', 'Search actions or pages...')}
            value={query}
            onChange={(e) => { setQuery(e.target.value); setActiveIdx(0); }}
            onKeyDown={onKeyDown}
            className="border-0 p-0 focus-visible:ring-0 focus-visible:border-0 shadow-none"
          />
          <kbd className="rounded border bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">ESC</kbd>
        </div>
        <ul className="max-h-80 overflow-y-auto py-1">
          {filtered.length === 0 && (
            <li className="px-3 py-6 text-center text-sm text-muted-foreground">
              {t('commandPalette.empty', 'No matches')}
            </li>
          )}
          {filtered.map((a, i) => {
            const Icon = a.icon;
            const active = i === activeIdx;
            return (
              <li
                key={a.id}
                className={cn(
                  'flex cursor-pointer items-center gap-3 px-3 py-2 text-sm',
                  active ? 'bg-accent text-accent-foreground' : 'hover:bg-muted/50',
                )}
                onMouseEnter={() => setActiveIdx(i)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  setOpen(false);
                  navigate({ to: a.to });
                }}
              >
                <Icon className="h-4 w-4 text-muted-foreground" />
                <span className="flex-1 truncate">{t(a.labelKey)}</span>
                <span className="shrink-0 font-mono text-[10px] uppercase text-muted-foreground">{a.group}</span>
              </li>
            );
          })}
        </ul>
      </DialogContent>
    </Dialog>
  );
}
