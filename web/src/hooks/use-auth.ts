import { useCallback, useEffect, useState } from 'react';
import { api, apiPost } from '@/lib/api';

interface User {
  id: string;
  email: string;
  display_name: string;
  avatar_url: string | null;
  is_active: boolean;
}

interface LoginResponse {
  access_token: string;
  refresh_token: string;
  token_type: string;
  expires_in: number;
  signing_key: string;
  password_change_required?: boolean;
  // When TOTP is required, only this field is returned
  totp_required?: boolean;
}

export function useAuth() {
  const [user, setUser] = useState<User | null>(null);
  const [loading, setLoading] = useState(true);

  const fetchUser = useCallback(async () => {
    const token = localStorage.getItem('access_token');
    if (!token) {
      setLoading(false);
      return;
    }
    try {
      const u = await api<User>('/api/auth/me');
      setUser(u);
    } catch {
      setUser(null);
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
    localStorage.setItem('access_token', res.access_token);
    localStorage.setItem('refresh_token', res.refresh_token);
    sessionStorage.setItem('signing_key', res.signing_key);
    await fetchUser();
    return res;
  };

  const logout = () => {
    localStorage.removeItem('access_token');
    localStorage.removeItem('refresh_token');
    sessionStorage.removeItem('signing_key');
    setUser(null);
  };

  const handleSsoCallback = useCallback(async (accessToken: string, refreshToken: string, signingKey: string) => {
    localStorage.setItem('access_token', accessToken);
    localStorage.setItem('refresh_token', refreshToken);
    sessionStorage.setItem('signing_key', signingKey);
    await fetchUser();
  }, [fetchUser]);

  return { user, loading, login, logout, handleSsoCallback };
}
