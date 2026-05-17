/**
 * Stores an ECDSA P-256 key pair in IndexedDB with an expiry timestamp.
 *
 * The private key is non-extractable — XSS cannot call
 * crypto.subtle.exportKey() on it. The public key JWK is exportable
 * so it can be sent to the server via POST /api/auth/register-key.
 *
 * Asymmetric: no secret needs to travel from server to client.
 */

const DB_NAME = 'thinkwatch-keys';
const STORE_NAME = 'signing';
const KEY_ID = 'current';
/**
 * Key lifetime matches the server's default refresh-token TTL
 * (`jwt_refresh_ttl_days`, default 7 days). The previous 24-hour
 * value lied — server stored the public key for the full refresh
 * lifetime, so after 24h the frontend silently discarded the
 * private key, every signed mutation went out unsigned, got 401,
 * triggered a refresh + register-key cycle, then retried. Worked
 * but added one wasted round-trip on each first action after a
 * day-long idle. The 401-retry path is still load-bearing as a
 * safety net (if server TTL is dialed below 7d in config, or the
 * key is evicted from Redis for any other reason), so widening
 * the frontend TTL doesn't remove the fallback — just stops
 * triggering it on the common case.
 */
const KEY_TTL_MS = 7 * 24 * 60 * 60 * 1000;

interface StoredEntry {
  privateKey: CryptoKey;
  publicJwk: JsonWebKey;
  expiresAt: number; // Unix ms
}

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 3); // version 3: ECDSA key pair
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE_NAME)) {
        db.createObjectStore(STORE_NAME);
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

/**
 * Generate an ECDSA P-256 key pair, store it in IndexedDB (private key
 * non-extractable), and return the public key JWK to send to the server.
 */
export async function generateAndStoreKeyPair(): Promise<JsonWebKey> {
  const keyPair = await crypto.subtle.generateKey(
    { name: 'ECDSA', namedCurve: 'P-256' },
    false, // private key non-extractable
    ['sign', 'verify'],
  );
  const publicJwk = await crypto.subtle.exportKey('jwk', keyPair.publicKey);
  const entry: StoredEntry = {
    privateKey: keyPair.privateKey,
    publicJwk,
    expiresAt: Date.now() + KEY_TTL_MS,
  };
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE_NAME, 'readwrite');
    tx.objectStore(STORE_NAME).put(entry, KEY_ID);
    tx.oncomplete = () => resolve(publicJwk);
    tx.onerror = () => reject(tx.error);
  });
}

/** Retrieve the private CryptoKey from IndexedDB. Returns null if expired or not found. */
export async function getSigningKey(): Promise<CryptoKey | null> {
  try {
    const db = await openDb();
    const entry: StoredEntry | undefined = await new Promise((resolve, reject) => {
      const tx = db.transaction(STORE_NAME, 'readonly');
      const req = tx.objectStore(STORE_NAME).get(KEY_ID);
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
    if (!entry?.privateKey) return null;
    // Check expiry
    if (Date.now() > entry.expiresAt) {
      await clearSigningKey();
      return null;
    }
    return entry.privateKey;
  } catch {
    return null;
  }
}

/** Delete the key pair from IndexedDB (logout / expiry). */
export async function clearSigningKey(): Promise<void> {
  // Also wipe the cross-tab register-keypair throttle so the next
  // login generates a fresh pair instead of inheriting a stale one
  // from a recent registration. localStorage write can throw in
  // private mode — non-fatal.
  try {
    localStorage.removeItem('thinkwatch_keypair_last_registered_ms');
  } catch {
    // ignore
  }
  try {
    const db = await openDb();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(STORE_NAME, 'readwrite');
      tx.objectStore(STORE_NAME).delete(KEY_ID);
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });
  } catch {
    // Ignore — DB may not exist yet
  }
}
