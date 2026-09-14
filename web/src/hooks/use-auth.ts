import { useCallback, useEffect } from 'react';
import { hashKey, useQuery, useQueryClient, type QueryClient } from '@tanstack/react-query';
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

/// The signed-in user, or `null` once the server has said there is none.
const ME_KEY = ['auth', 'me'];

/**
 * Forget the signed-in user and everything that was loaded on their behalf.
 *
 * The query cache outlives a session in this tab. Without the sweep, the
 * next person to sign in here would be shown the previous user's cached
 * pages — member lists, keys, audit rows — until each one refetched.
 */
function endSession(queryClient: QueryClient) {
  clearCachedPermissions();
  queryClient.setQueryData(ME_KEY, null);
  const me = hashKey(ME_KEY);
  queryClient.removeQueries({ predicate: (query) => query.queryHash !== me });
}

export function useAuth() {
  const queryClient = useQueryClient();

  const { data: user = null, isPending: loading } = useQuery({
    queryKey: ME_KEY,
    // No localStorage check anymore — the access cookie is opaque
    // from JS, so the only way to know if we're logged in is to
    // ask the server. /api/auth/me returns 401 if the cookie is
    // missing or invalid, which the api client handles via the
    // 401 → refresh → logout flow.
    //
    // No abort signal: every failure below reads as "signed out", and a
    // request cancelled because a component unmounted is not one.
    queryFn: async () => {
      try {
        const u = await api<User>('/api/auth/me', { no401Redirect: true, schema: UserResponseSchema });
        setCachedPermissions(u.permissions, u.denied_permissions);
        return u;
      } catch {
        clearCachedPermissions();
        return null;
      }
    },
    // Who is signed in only changes through this hook — login, logout, the
    // SSO callback — and each of those updates the entry itself. Left to go
    // stale, every component mounting the hook and every network reconnect
    // would refetch it, and one refetch failing on a flaky connection would
    // drop the admin to the login page mid-session.
    staleTime: Infinity,
  });

  // Listen for cross-tab logout broadcasts so this tab drops its
  // signed-in user (and the admin UI unmounts) immediately,
  // instead of waiting for the next request to 401. The broadcast
  // origin is api.ts's BroadcastChannel handler; it also clears the
  // signing key + permission cache, but the React tree only resets
  // when the user entry flips to null here.
  useEffect(() => {
    const handler = () => endSession(queryClient);
    window.addEventListener('thinkwatch:logged-out', handler);
    return () => window.removeEventListener('thinkwatch:logged-out', handler);
  }, [queryClient]);

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
    await queryClient.refetchQueries({ queryKey: ME_KEY });
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
    broadcastLogout();
    endSession(queryClient);
  }, [queryClient]);

  const handleSsoCallback = useCallback(async () => {
    // SSO redirect set the auth cookies. Generate an ECDSA key pair
    // and register the public key with the server.
    await registerKeyPair();
    await queryClient.refetchQueries({ queryKey: ME_KEY });
  }, [queryClient]);

  return { user, loading, login, logout, handleSsoCallback };
}
