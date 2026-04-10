import type { ZodType } from 'zod';

export const API_BASE = import.meta.env.VITE_API_BASE ?? '';

interface ApiOptions<T = unknown> {
  method?: string;
  body?: unknown;
  headers?: Record<string, string>;
  /// Optional zod schema. When provided, the response is validated at
  /// runtime — a schema mismatch is logged via console.error AND throws,
  /// so callers find out immediately if the backend changes shape.
  schema?: ZodType<T>;
  /// When true, a 401 that survives refresh does NOT trigger the
  /// global window.location.href redirect. Use this for "am I logged in?"
  /// probes (e.g. /api/auth/me on mount) where 401 just means "not logged
  /// in yet" rather than "session expired mid-use".
  no401Redirect?: boolean;
}

// --- Auth model ---
//
// Access and refresh tokens live in HttpOnly cookies set by the
// server on login/refresh/SSO. The browser auto-attaches them to
// every request to the same origin (we use credentials: 'include'
// below for the explicit declaration). The page JS NEVER reads
// access_token / refresh_token — they're not in localStorage and
// they can't be read out of the cookie either, so an XSS payload
// can't exfiltrate them.
//
// `signing_key` is the one secret the page JS still needs to read,
// because it has to compute HMAC signatures on write requests.
// It's stored in sessionStorage (dies with the tab) and the server
// also keeps a copy in an HttpOnly cookie that it reads to verify
// the signature, so client-side and server-side stay in lock-step.
//
// `permissions` for hasPermission() come from the /api/auth/me
// response (cached in `permissionsCache` below) — the JWT used to
// be readable from localStorage and we'd decode it for this, but
// the cookie is opaque now.

let permissionsCache: Set<string> = new Set();

export function setCachedPermissions(perms: string[] | undefined): void {
  permissionsCache = new Set(perms ?? []);
}

export function clearCachedPermissions(): void {
  permissionsCache = new Set();
}

// --- HMAC Signing (Web Crypto API) ---

async function signRequest(
  method: string,
  path: string,
  bodyStr: string | undefined,
): Promise<Record<string, string>> {
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
// With cookie-based auth, refresh is just a POST without body —
// the browser sends the refresh_token cookie automatically. We
// still dedupe in-flight refreshes per tab so two simultaneous
// 401s don't both trigger a refresh (the second one would hit
// the rotation blacklist and force-logout the user).
//
// Cross-tab coordination is implicit: when one tab refreshes, the
// new cookie is set on the entire origin, so every other tab's
// next request automatically uses it. The BroadcastChannel below
// just propagates "logged out" so all tabs redirect to login at
// once, instead of each finding out separately when they 401.

let refreshPromise: Promise<boolean> | null = null;

const authChannel: BroadcastChannel | null =
  typeof BroadcastChannel !== 'undefined' ? new BroadcastChannel('thinkwatch-auth') : null;

if (authChannel) {
  authChannel.onmessage = (ev: MessageEvent<{ type: string }>) => {
    if (ev.data?.type === 'logged-out') {
      sessionStorage.removeItem('signing_key');
      clearCachedPermissions();
      // Don't redirect inside the message handler — let the existing
      // 401 path handle it the next time this tab makes a request.
    }
  };
}

async function tryRefreshToken(): Promise<boolean> {
  // Deduplicate concurrent refresh attempts within this tab
  if (refreshPromise) return refreshPromise;

  refreshPromise = (async () => {
    try {
      const res = await fetch(`${API_BASE}/api/auth/refresh`, {
        method: 'POST',
        credentials: 'include',
        headers: { 'Content-Type': 'application/json' },
        body: '{}',
      });
      if (!res.ok) return false;
      const data = await res.json();
      // The new tokens are in cookies the browser already set;
      // we only need to update sessionStorage with the new
      // signing_key and the cached permissions.
      if (typeof data.signing_key === 'string') {
        sessionStorage.setItem('signing_key', data.signing_key);
      }
      if (Array.isArray(data.permissions)) {
        setCachedPermissions(data.permissions);
      }
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

function validate<T>(path: string, json: unknown, schema?: ZodType<T>): T {
  if (!schema) return json as T;
  const parsed = schema.safeParse(json);
  if (!parsed.success) {
    console.error(`API response failed schema validation for ${path}`, parsed.error, json);
    throw new Error(`Invalid response shape from ${path}`);
  }
  return parsed.data;
}

export async function api<T>(path: string, options: ApiOptions<T> = {}): Promise<T> {
  const method = options.method ?? 'GET';
  const bodyStr = options.body ? JSON.stringify(options.body) : undefined;

  // Compute signature headers for write operations (sessionStorage
  // signing_key, set by login/refresh response).
  const sigHeaders = await signRequest(method, path, bodyStr);

  const res = await fetch(`${API_BASE}${path}`, {
    method,
    credentials: 'include',
    headers: {
      'Content-Type': 'application/json',
      ...sigHeaders,
      ...options.headers,
    },
    body: bodyStr,
  });

  if (res.status === 401) {
    // Attempt token refresh before logging out
    const refreshed = await tryRefreshToken();
    if (refreshed) {
      // Retry the original request — cookies are now refreshed
      // by the browser, no need to re-fetch tokens manually.
      const retrySigHeaders = await signRequest(method, path, bodyStr);
      const retryRes = await fetch(`${API_BASE}${path}`, {
        method,
        credentials: 'include',
        headers: {
          'Content-Type': 'application/json',
          ...retrySigHeaders,
          ...options.headers,
        },
        body: bodyStr,
      });
      if (retryRes.ok) return validate(path, await retryRes.json(), options.schema);
    }
    // Skip eviction for probe calls like /api/auth/me on mount —
    // a 401 there means "not logged in yet", not "session expired".
    if (!options.no401Redirect) {
      sessionStorage.removeItem('signing_key');
      clearCachedPermissions();
      authChannel?.postMessage({ type: 'logged-out' });
      void fetch(`${API_BASE}/api/auth/logout`, {
        method: 'POST',
        credentials: 'include',
      }).catch(() => {});
      window.location.href = '/';
    }
    throw new Error('Unauthorized');
  }

  if (!res.ok) {
    const err = await res.json().catch(() => ({ error: { message: res.statusText } }));
    throw new Error(err.error?.message ?? 'Request failed');
  }

  return validate(path, await res.json(), options.schema);
}

export const apiPost = <T>(path: string, body: unknown) =>
  api<T>(path, { method: 'POST', body });

export const apiPatch = <T>(path: string, body: unknown) =>
  api<T>(path, { method: 'PATCH', body });

export const apiDelete = <T>(path: string) =>
  api<T>(path, { method: 'DELETE' });

/// Returns the cached permission set populated from /api/auth/me.
/// Empty until the auth hook has fetched once. Never throws.
export function currentUserPermissions(): string[] {
  return Array.from(permissionsCache);
}

export function hasPermission(perm: string): boolean {
  return permissionsCache.has(perm);
}

/// Broadcast "logged out" to other tabs so they all clear their
/// in-memory state and redirect together. Used by use-auth's
/// logout flow.
export function broadcastLogout(): void {
  authChannel?.postMessage({ type: 'logged-out' });
}
