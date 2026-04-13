/**
 * Stores the HMAC signing key as a non-extractable CryptoKey in IndexedDB.
 *
 * XSS cannot export the raw key material — crypto.subtle.exportKey()
 * throws on non-extractable keys. The attacker can only use the key
 * while their script is running in the page context.
 */

const DB_NAME = 'thinkwatch-keys';
const STORE_NAME = 'signing';
const KEY_ID = 'current';

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
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

/** Import a hex signing key as a non-extractable CryptoKey and store in IndexedDB. */
export async function storeSigningKey(hexKey: string): Promise<void> {
  const keyBytes = new Uint8Array(hexKey.match(/.{2}/g)!.map(h => parseInt(h, 16)));
  const cryptoKey = await crypto.subtle.importKey(
    'raw',
    keyBytes,
    { name: 'HMAC', hash: 'SHA-256' },
    false, // non-extractable
    ['sign'],
  );
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE_NAME, 'readwrite');
    tx.objectStore(STORE_NAME).put(cryptoKey, KEY_ID);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

/** Retrieve the CryptoKey from IndexedDB. Returns null if not found. */
export async function getSigningKey(): Promise<CryptoKey | null> {
  try {
    const db = await openDb();
    return new Promise((resolve, reject) => {
      const tx = db.transaction(STORE_NAME, 'readonly');
      const req = tx.objectStore(STORE_NAME).get(KEY_ID);
      req.onsuccess = () => resolve(req.result ?? null);
      req.onerror = () => reject(req.error);
    });
  } catch {
    return null;
  }
}

/** Delete the signing key from IndexedDB (logout). */
export async function clearSigningKey(): Promise<void> {
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
