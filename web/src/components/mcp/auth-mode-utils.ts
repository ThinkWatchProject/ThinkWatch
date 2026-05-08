import { Globe, KeyRound, Lock } from 'lucide-react';

/** Mirror of the backend `auth_shape` enum on `mcp_servers`. */
export type AuthMode = 'oauth' | 'static' | 'direct';

export const AUTH_MODES: AuthMode[] = ['oauth', 'static', 'direct'];

export const authModeIcon: Record<AuthMode, typeof Globe> = {
  oauth: Lock,
  static: KeyRound,
  direct: Globe,
};

/**
 * Map a server's `auth_shape` field to the UI's `AuthMode`. Kept as
 * a thin shim so the badge / form code can keep using the friendlier
 * "direct" label for anonymous servers (the backend calls them
 * `'anonymous'`; the UI has historically called them `'direct'`).
 */
export function deriveAuthMode(server: { auth_shape: string }): AuthMode {
  switch (server.auth_shape) {
    case 'oauth':
      return 'oauth';
    case 'static':
      return 'static';
    default:
      return 'direct';
  }
}
