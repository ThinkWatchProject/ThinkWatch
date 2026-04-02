import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { Shield } from 'lucide-react';

interface SystemRole {
  name: string;
  description: string;
  permissions: string[];
}

const systemRoles: SystemRole[] = [
  {
    name: 'super_admin',
    description: 'Full system access. Can manage all settings, users, providers, and view all audit logs.',
    permissions: [
      'users:manage',
      'roles:manage',
      'providers:manage',
      'keys:manage',
      'mcp:manage',
      'analytics:view',
      'audit:view',
      'settings:manage',
    ],
  },
  {
    name: 'admin',
    description: 'Administrative access. Can manage providers, keys, and MCP servers.',
    permissions: [
      'providers:manage',
      'keys:manage',
      'mcp:manage',
      'analytics:view',
      'audit:view',
      'users:view',
    ],
  },
  {
    name: 'team_manager',
    description: 'Team-level management. Can create API keys and view usage for their team.',
    permissions: [
      'keys:manage_team',
      'analytics:view_team',
      'mcp:view',
      'providers:view',
    ],
  },
  {
    name: 'developer',
    description: 'Standard developer access. Can use the gateway and view their own usage.',
    permissions: [
      'keys:view_own',
      'analytics:view_own',
      'mcp:view',
      'providers:view',
      'models:view',
    ],
  },
  {
    name: 'viewer',
    description: 'Read-only access. Can view providers, models, and their own analytics.',
    permissions: [
      'analytics:view_own',
      'providers:view',
      'models:view',
    ],
  },
];

export function RolesPage() {
  const { t } = useTranslation();

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">{t('roles.title')}</h1>
        <p className="text-muted-foreground">{t('roles.subtitle')}</p>
      </div>

      <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
        {systemRoles.map((role) => (
          <Card key={role.name}>
            <CardHeader className="pb-3">
              <CardTitle className="flex items-center gap-2 text-sm font-medium">
                <Shield className="h-4 w-4 text-muted-foreground" />
                <Badge variant="secondary">{role.name}</Badge>
                <Badge variant="outline" className="ml-auto text-[10px]">{t('roles.systemRole')}</Badge>
              </CardTitle>
            </CardHeader>
            <CardContent className="space-y-3">
              <p className="text-xs text-muted-foreground">{role.description}</p>
              <div>
                <p className="text-xs font-medium mb-1.5">{t('roles.permissions')}</p>
                <div className="flex flex-wrap gap-1">
                  {role.permissions.map((perm) => (
                    <Badge key={perm} variant="outline" className="text-[10px]">
                      {perm}
                    </Badge>
                  ))}
                </div>
              </div>
            </CardContent>
          </Card>
        ))}
      </div>
    </div>
  );
}
