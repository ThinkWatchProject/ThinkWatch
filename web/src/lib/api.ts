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
  /// Optional AbortSignal for cancelling in-flight requests (e.g. from
  /// useEffect cleanup). Passed through to the underlying fetch call.
  signal?: AbortSignal;
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
// Request signing uses ECDSA P-256 with a client-generated key pair.
// The private key is non-extractable (stored in IndexedDB) so XSS
// cannot exfiltrate it. After login, the client generates a key pair
// and registers the public key with the server via POST /api/auth/register-key.
// No secret travels from server to client.
//
// `permissions` for hasPermission() come from the /api/auth/me
// response (cached in `permissionsCache` below). The JWT cookie is
// opaque to the client, so claims have to come over the wire.

let permissionsCache: Set<string> = new Set();
let deniedPermissionsCache: Set<string> = new Set();

export function setCachedPermissions(perms: string[] | undefined, denied?: string[]): void {
  permissionsCache = new Set(perms ?? []);
  deniedPermissionsCache = new Set(denied ?? []);
}

export function clearCachedPermissions(): void {
  permissionsCache = new Set();
  deniedPermissionsCache = new Set();
}

// --- ECDSA P-256 Signing (Web Crypto API) ---

/** Base64url-encode a Uint8Array (no padding). */
function base64urlEncode(bytes: Uint8Array): string {
  let binary = '';
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

async function signRequest(
  method: string,
  path: string,
  bodyStr: string | undefined,
): Promise<Record<string, string>> {
  // Private key is stored as a non-extractable CryptoKey in IndexedDB.
  // XSS cannot export the raw key material.
  const { getSigningKey } = await import('./crypto-store');
  const privateKey = await getSigningKey();
  if (!privateKey) return {};

  const timestamp = Math.floor(Date.now() / 1000).toString();
  const nonce = crypto.randomUUID();

  // SHA-256 of body
  const bodyBytes = new TextEncoder().encode(bodyStr ?? '');
  const bodyHash = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', bodyBytes)))
    .map(b => b.toString(16).padStart(2, '0'))
    .join('');

  // String-to-sign (same format as before)
  const stringToSign = `${method.toUpperCase()}\n${path}\n${timestamp}\n${nonce}\n${bodyHash}`;

  // Sign with ECDSA P-256 + SHA-256
  const sigBytes = new Uint8Array(
    await crypto.subtle.sign(
      { name: 'ECDSA', hash: 'SHA-256' },
      privateKey,
      new TextEncoder().encode(stringToSign),
    ),
  );

  return {
    'X-Signature-Timestamp': timestamp,
    'X-Signature-Nonce': nonce,
    'X-Signature': `ecdsa-p256:${base64urlEncode(sigBytes)}`,
  };
}

/**
 * Generate an ECDSA key pair and register the public key with the server.
 * Called after login, register, SSO callback, and token refresh.
 */
export async function registerKeyPair(): Promise<void> {
  const { generateAndStoreKeyPair } = await import('./crypto-store');
  const publicJwk = await generateAndStoreKeyPair();
  // POST the public key to the server (no signature needed on this endpoint)
  await fetch(`${API_BASE}/api/auth/register-key`, {
    method: 'POST',
    credentials: 'include',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ public_key: publicJwk }),
  });
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
      import('./crypto-store').then(m => m.clearSigningKey());
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
      // generate a fresh ECDSA key pair and register the public key.
      await registerKeyPair();
      if (Array.isArray(data.permissions)) {
        setCachedPermissions(data.permissions, data.denied_permissions);
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

  // Compute ECDSA signature headers for write operations (private key
  // in IndexedDB, generated at login time).
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
    signal: options.signal,
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
        signal: options.signal,
      });
      if (retryRes.ok) return validate(path, await retryRes.json(), options.schema);
    }
    // Skip eviction for probe calls like /api/auth/me on mount —
    // a 401 there means "not logged in yet", not "session expired".
    if (!options.no401Redirect) {
      import('./crypto-store').then(m => m.clearSigningKey());
      clearCachedPermissions();
      authChannel?.postMessage({ type: 'logged-out' });
      void fetch(`${API_BASE}/api/auth/logout`, {
        method: 'POST',
        credentials: 'include',
      }).catch(() => {});
      window.location.href = '/';
    }
    throw new ApiError('Unauthorized', 401, 'unauthorized');
  }

  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    const errorBody = body?.error;
    const serverMessage =
      typeof errorBody === 'string'
        ? errorBody
        : errorBody?.message ?? body?.message ?? res.statusText;
    const errorType: string | undefined =
      typeof errorBody === 'object' ? errorBody?.type : undefined;
    throw new ApiError(serverMessage || 'Request failed', res.status, errorType);
  }

  return validate(path, await res.json(), options.schema);
}

/**
 * Structured API failure carrying the HTTP status + the server's
 * `error.type` tag (`unauthorized`, `forbidden`, `rate_limited`,
 * `service_unavailable`, …) so consumers can render a translated,
 * actionable message instead of a raw English fragment. Toast
 * helpers below pick the i18n key off `type` first, falling back to
 * `status`, finally to the bare server message.
 */
export class ApiError extends Error {
  status: number;
  type?: string;
  constructor(message: string, status: number, type?: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.type = type;
  }
}

/**
 * Resolve a translated, actionable message for an API failure.
 *
 * `type` (server-supplied) gives the most specific tag; falling back
 * to status code keeps the mapping working for legacy responses; the
 * raw server message is the last resort when nothing else matches.
 * `t` is passed in instead of imported so this stays usable from
 * non-React contexts (toast handlers in api.ts itself).
 */
export function describeApiError(
  err: unknown,
  t: (key: string, fallback?: { defaultValue?: string }) => string,
): string {
  if (err instanceof ApiError) {
    if (err.type) {
      const typeKey = `errors.byType.${err.type}`;
      const translated = t(typeKey, { defaultValue: '' });
      if (translated) return translated;
    }
    const statusKey = `errors.byStatus.${err.status}`;
    const byStatus = t(statusKey, { defaultValue: '' });
    if (byStatus) return byStatus;
    return err.message;
  }
  if (err instanceof Error) return err.message;
  return t('errors.generic');
}

export const apiPost = <T>(path: string, body: unknown) =>
  api<T>(path, { method: 'POST', body });

export const apiPatch = <T>(path: string, body: unknown) =>
  api<T>(path, { method: 'PATCH', body });

export const apiPut = <T>(path: string, body: unknown) =>
  api<T>(path, { method: 'PUT', body });

export const apiDelete = <T>(path: string) =>
  api<T>(path, { method: 'DELETE' });

/// Returns the cached permission set populated from /api/auth/me.
/// Empty until the auth hook has fetched once. Never throws.
export function currentUserPermissions(): string[] {
  return Array.from(permissionsCache);
}

export function hasPermission(perm: string): boolean {
  if (deniedPermissionsCache.has(perm)) return false;
  return permissionsCache.has(perm);
}

/// Broadcast "logged out" to other tabs so they all clear their
/// in-memory state and redirect together. Used by use-auth's
/// logout flow.
export function broadcastLogout(): void {
  authChannel?.postMessage({ type: 'logged-out' });
}
