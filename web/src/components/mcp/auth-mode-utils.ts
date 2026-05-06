import { Globe, KeyRound, Lock } from 'lucide-react';

export type AuthMode = 'oauth' | 'static' | 'direct';

export const AUTH_MODES: AuthMode[] = ['oauth', 'static', 'direct'];

export const authModeIcon: Record<AuthMode, typeof Globe> = {
  oauth: Lock,
  static: KeyRound,
  direct: Globe,
};

/**
 * Derive a server's primary auth mode from its persisted fields.
 *
 * Three modes only — `direct` covers both "public, no headers at all"
 * and "service-to-service via custom headers", because the data model
 * doesn't distinguish them: both have no OAuth issuer and no
 * `allow_static_token`, the only difference is whether
 * `config_json.custom_headers` is empty. Splitting them in the UI
 * was confusing — admins kept asking what the difference was.
 *
 * Per-user cache scoping is *not* derived from this mode; the
 * gateway's `determine_cache_scope` keys off whether `custom_headers`
 * contains `{{user_id}}` / `{{user_email}}` template variables and
 * flips the lane to per-caller automatically.
 */
export function deriveAuthMode(server: {
  oauth_issuer: string | null;
  allow_static_token: boolean;
}): AuthMode {
  if (server.oauth_issuer) return 'oauth';
  if (server.allow_static_token) return 'static';
  return 'direct';
}
