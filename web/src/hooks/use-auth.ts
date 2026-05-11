import { useCallback, useEffect, useState } from 'react';
import {
  api,
  apiPost,
  broadcastLogout,
  clearCachedPermissions,
  registerKeyPair,
  setCachedPermissions,
} from '@/lib/api';
import { UserResponseSchema, type UserResponse } from '@/lib/schemas';

type User = UserResponse;

interface LoginResponse {
  token_type: string;
  expires_in: number;
  permissions?: string[];
  denied_permissions?: string[];
  roles?: string[];
  password_change_required?: boolean;
  // When TOTP is required, only this field is returned
  totp_required?: boolean;
}

interface PowSolution {
  challenge_id: string;
  nonce: string;
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
      const u = await api<User>('/api/auth/me', { no401Redirect: true, schema: UserResponseSchema });
      setUser(u);
      setCachedPermissions(u.permissions, u.denied_permissions);
    } catch {
      setUser(null);
      clearCachedPermissions();
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetchUser(); }, [fetchUser]);

  const login = async (
    email: string,
    password: string,
    totpCode?: string,
    pow?: PowSolution,
  ): Promise<LoginResponse> => {
    const body: Record<string, unknown> = { email, password };
    if (totpCode) body.totp_code = totpCode;
    if (pow) body.pow = pow;
    // `no401Redirect: true` — login is the *exact* endpoint where 401
    // means "wrong password", not "session expired." Without this flag
    // the api client would redirect to `/` on bad creds, which reloads
    // the LoginPage and drops the in-memory error state — the user
    // would see "Login failed" flash for ~100ms before the page
    // reload wiped it.
    const res = await api<LoginResponse>('/api/auth/login', {
      method: 'POST',
      body,
      no401Redirect: true,
    });
    if (res.totp_required) {
      return res; // Caller must handle TOTP step
    }
    // Server set the access/refresh cookies on the response — the
    // browser already has them. Generate an ECDSA key pair and
    // register the public key with the server.
    await registerKeyPair();
    setCachedPermissions(res.permissions, res.denied_permissions);
    await fetchUser();
    return res;
  };

  const logout = useCallback(async () => {
    try {
      await apiPost('/api/auth/logout', {});
    } catch {
      // ignore
    }
    const { clearSigningKey } = await import('@/lib/crypto-store');
    await clearSigningKey();
    clearCachedPermissions();
    broadcastLogout();
    setUser(null);
  }, []);

  const handleSsoCallback = useCallback(async () => {
    // SSO redirect set the auth cookies. Generate an ECDSA key pair
    // and register the public key with the server.
    await registerKeyPair();
    await fetchUser();
  }, [fetchUser]);

  return { user, loading, login, logout, handleSsoCallback };
}
