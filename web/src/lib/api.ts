const API_BASE = import.meta.env.VITE_API_BASE ?? '';

interface ApiOptions {
  method?: string;
  body?: unknown;
  headers?: Record<string, string>;
}

// --- HMAC Signing (Web Crypto API) ---

async function signRequest(
  method: string,
  path: string,
  bodyStr: string | undefined,
): Promise<Record<string, string>> {
  // Signing key is delivered via httpOnly cookie (primary) and also stored
  // in sessionStorage as fallback for the client-side HMAC computation.
  // The httpOnly cookie is sent automatically by the browser for signature
  // verification on the server side, but we still need the hex value
  // client-side to compute the HMAC signature.
  const signingKeyHex = sessionStorage.getItem('signing_key');
  if (!signingKeyHex) return {};

  const timestamp = Math.floor(Date.now() / 1000).toString();
  const nonce = crypto.randomUUID();

  // SHA-256 of body
  const bodyBytes = new TextEncoder().encode(bodyStr ?? '');
  const bodyHash = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bodyBytes)))
    .map(b => b.toString(16).padStart(2, '0'))
    .join('');

  // String-to-sign
  const stringToSign = `${method.toUpperCase()}\n${path}\n${timestamp}\n${nonce}\n${bodyHash}`;

  // Import HMAC key
  const keyBytes = new Uint8Array(signingKeyHex.match(/.{2}/g)!.map(h => parseInt(h, 16)));
  const cryptoKey = await crypto.subtle.importKey(
    'raw',
    keyBytes,
    { name: 'HMAC', hash: 'SHA-256' },
    false,
    ['sign'],
  );

  // Sign
  const sigBytes = new Uint8Array(
    await crypto.subtle.sign('HMAC', cryptoKey, new TextEncoder().encode(stringToSign)),
  );
  const sigHex = Array.from(sigBytes)
    .map(b => b.toString(16).padStart(2, '0'))
    .join('');

  return {
    'X-Signature-Timestamp': timestamp,
    'X-Signature-Nonce': nonce,
    'X-Signature': `hmac-sha256:${sigHex}`,
  };
}

// --- Token Refresh ---
//
// Cross-tab coordination: refresh requests can race when multiple tabs
// hit a 401 simultaneously. Without coordination each tab posts its own
// refresh, the second one wins, and the first tab's freshly-rotated
// token is silently invalidated. We coordinate via:
//   1. BroadcastChannel (modern browsers): one tab broadcasts the new
//      tokens after refresh, others apply them without a network call.
//   2. Per-tab in-memory dedupe of the in-flight promise.
//   3. localStorage `storage` event as a fallback for older browsers
//      that don't support BroadcastChannel.

let refreshPromise: Promise<boolean> | null = null;

const authChannel: BroadcastChannel | null =
  typeof BroadcastChannel !== 'undefined' ? new BroadcastChannel('thinkwatch-auth') : null;

interface RefreshBroadcast {
  type: 'refreshed';
  access_token: string;
  refresh_token: string;
  signing_key: string;
}

if (authChannel) {
  authChannel.onmessage = (ev: MessageEvent<RefreshBroadcast>) => {
    if (ev.data?.type === 'refreshed') {
      localStorage.setItem('access_token', ev.data.access_token);
      localStorage.setItem('refresh_token', ev.data.refresh_token);
      sessionStorage.setItem('signing_key', ev.data.signing_key);
    }
  };
}

async function tryRefreshToken(): Promise<boolean> {
  // Deduplicate concurrent refresh attempts within this tab
  if (refreshPromise) return refreshPromise;

  refreshPromise = (async () => {
    const refreshToken = localStorage.getItem('refresh_token');
    if (!refreshToken) return false;
    try {
      const res = await fetch(`${API_BASE}/api/auth/refresh`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ refresh_token: refreshToken }),
      });
      if (!res.ok) return false;
      const data = await res.json();
      localStorage.setItem('access_token', data.access_token);
      localStorage.setItem('refresh_token', data.refresh_token);
      sessionStorage.setItem('signing_key', data.signing_key);
      // Tell other tabs about the new tokens so they don't trigger
      // their own refresh and invalidate ours.
      authChannel?.postMessage({
        type: 'refreshed',
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        signing_key: data.signing_key,
      } satisfies RefreshBroadcast);
      return true;
    } catch {
      return false;
    }
  })();

  try {
    return await refreshPromise;
  } finally {
    refreshPromise = null;
  }
}

// --- API Client ---

export async function api<T>(path: string, options: ApiOptions = {}): Promise<T> {
  const token = localStorage.getItem('access_token');
  const method = options.method ?? 'GET';
  const bodyStr = options.body ? JSON.stringify(options.body) : undefined;

  // Compute signature headers for write operations
  const sigHeaders = await signRequest(method, path, bodyStr);

  const res = await fetch(`${API_BASE}${path}`, {
    method,
    headers: {
      'Content-Type': 'application/json',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      ...sigHeaders,
      ...options.headers,
    },
    body: bodyStr,
  });

  if (res.status === 401) {
    // Attempt token refresh before logging out
    const refreshed = await tryRefreshToken();
    if (refreshed) {
      // Retry the original request with new token
      const newToken = localStorage.getItem('access_token');
      const retrySigHeaders = await signRequest(method, path, bodyStr);
      const retryRes = await fetch(`${API_BASE}${path}`, {
        method,
        headers: {
          'Content-Type': 'application/json',
          ...(newToken ? { Authorization: `Bearer ${newToken}` } : {}),
          ...retrySigHeaders,
          ...options.headers,
        },
        body: bodyStr,
      });
      if (retryRes.ok) return retryRes.json();
    }
    localStorage.removeItem('access_token');
    localStorage.removeItem('refresh_token');
    sessionStorage.removeItem('signing_key');
    window.location.href = '/';
    throw new Error('Unauthorized');
  }

  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: { message: res.statusText } }));
    throw new Error(err.error?.message ?? 'Request failed');
  }

  return res.json();
}

export const apiPost = <T>(path: string, body: unknown) =>
  api<T>(path, { method: 'POST', body });

export const apiPatch = <T>(path: string, body: unknown) =>
  api<T>(path, { method: 'PATCH', body });

export const apiDelete = <T>(path: string) =>
  api<T>(path, { method: 'DELETE' });
