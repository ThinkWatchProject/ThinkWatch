import { Globe, KeyRound, Lock, Settings2 } from 'lucide-react';

export type AuthMode = 'public' | 'oauth' | 'static' | 'headers';

export const AUTH_MODES: AuthMode[] = ['oauth', 'static', 'headers', 'public'];

export const authModeIcon: Record<AuthMode, typeof Globe> = {
  oauth: Lock,
  static: KeyRound,
  headers: Settings2,
  public: Globe,
};

/**
 * Derive a server's primary auth mode from its persisted fields.
 *
 * Precedence: OAuth wins over static-token (which can be a fallback for
 * OAuth servers); custom_headers alone implies a service-to-service
 * setup; otherwise the server is public. This matches the wizard's
 * mode-picker semantics — pick the *primary* auth mechanism the
 * operator chose, not the union of every signal.
 */
export function deriveAuthMode(server: {
  oauth_issuer: string | null;
  allow_static_token: boolean;
  config_json?: { custom_headers?: Record<string, string> };
}): AuthMode {
  if (server.oauth_issuer) return 'oauth';
  if (server.allow_static_token) return 'static';
  const headers = server.config_json?.custom_headers ?? {};
  if (Object.keys(headers).length > 0) return 'headers';
  return 'public';
}
