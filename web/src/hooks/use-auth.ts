import { useCallback, useEffect, useState } from 'react';
import {
  api,
  apiPost,
  broadcastLogout,
  clearCachedPermissions,
  setCachedPermissions,
} from '@/lib/api';

interface User {
  id: string;
  email: string;
  display_name: string;
  avatar_url: string | null;
  is_active: boolean;
  permissions?: string[];
}

interface LoginResponse {
  token_type: string;
  expires_in: number;
  signing_key: string;
  permissions?: string[];
  roles?: string[];
  password_change_required?: boolean;
  // When TOTP is required, only this field is returned
  totp_required?: boolean;
}

export function useAuth() {
  const [user, setUser] = useState<User | null>(null);
  const [loading, setLoading] = useState(true);

  const fetchUser = useCallback(async () => {
    // No localStorage check anymore — the access cookie is opaque
    // from JS, so the only way to know if we're logged in is to
    // ask the server. /api/auth/me returns 401 if the cookie is
    // missing or invalid, which the api client handles via the
    // 401 → refresh → logout flow.
    try {
      const u = await api<User>('/api/auth/me');
      setUser(u);
      setCachedPermissions(u.permissions);
    } catch {
      setUser(null);
      clearCachedPermissions();
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetchUser(); }, [fetchUser]);

  const login = async (email: string, password: string, totpCode?: string): Promise<LoginResponse> => {
    const body: Record<string, string> = { email, password };
    if (totpCode) body.totp_code = totpCode;
    const res = await apiPost<LoginResponse>('/api/auth/login', body);
    if (res.totp_required) {
      return res; // Caller must handle TOTP step
    }
    // Server set the access/refresh cookies on the response — the
    // browser already has them. We only need to stash signing_key
    // (for HMAC) and seed the permission cache so hasPermission()
    // works without a /me round-trip.
    sessionStorage.setItem('signing_key', res.signing_key);
    setCachedPermissions(res.permissions);
    await fetchUser();
    return res;
  };

  const logout = useCallback(async () => {
    // Tell the server to clear the auth cookies. We don't depend
    // on this succeeding — the local cleanup happens regardless.
    try {
      await apiPost('/api/auth/logout', {});
    } catch {
      // ignore
    }
    sessionStorage.removeItem('signing_key');
    clearCachedPermissions();
    broadcastLogout();
    setUser(null);
  }, []);

  const handleSsoCallback = useCallback(async (signingKey: string) => {
    // The SSO callback already set the auth cookies via Set-Cookie
    // on the redirect response — only the signing_key travels via
    // URL fragment for the page JS to stash.
    sessionStorage.setItem('signing_key', signingKey);
    await fetchUser();
  }, [fetchUser]);

  return { user, loading, login, logout, handleSsoCallback };
}
