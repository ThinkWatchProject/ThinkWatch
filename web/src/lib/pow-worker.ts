// Proof-of-work grinder running in a Web Worker. Spawned by
// `usePowChallenge` so the SHA-256 brute-force doesn't block the
// main thread while the user fills out the login form.
//
// Protocol:
//   - main → worker: { challenge_random, email, difficulty }
//   - worker → main: { type: 'progress', tried } (every 50k iters)
//   - worker → main: { type: 'done', nonce, tried, elapsed_ms }
//
// Hash input is `challenge_random || ":" || email || ":" || nonce` —
// the email is part of the salt so a challenge cannot be ground for
// one account and reused against another. Caller must pass the
// same email the mint endpoint was called with (server stored it
// at mint, will compare on verify).
//
// The worker terminates itself after posting `done`. Main thread
// reuses one worker per challenge (cheaper than re-spawn for the
// progress channel; the worker is < 1 KB after build).

interface StartMessage {
  type: 'start';
  challenge_random: string;
  email: string;
  difficulty: number;
}

type Inbound = StartMessage;

self.onmessage = async (e: MessageEvent<Inbound>) => {
  const msg = e.data;
  if (msg.type !== 'start') return;
  const { challenge_random, email, difficulty } = msg;

  const encoder = new TextEncoder();
  const prefix = encoder.encode(`${challenge_random}:${email}:`);
  // SubtleCrypto.digest is async; doing one call per nonce works but
  // adds ~50µs of promise overhead each. Fine at our difficulty
  // (~250k iters), and saves us a JS SHA-256 implementation.
  const subtle = self.crypto.subtle;

  const start = performance.now();
  let tried = 0;
  let nonce = 0;

  // 50k-iter status updates so the UI can show "verifying…" with
  // progress instead of a frozen indicator.
  const PROGRESS_EVERY = 50_000;

  while (true) {
    tried++;
    const nonceStr = nonce.toString();
    const nonceBytes = encoder.encode(nonceStr);
    const buf = new Uint8Array(prefix.length + nonceBytes.length);
    buf.set(prefix, 0);
    buf.set(nonceBytes, prefix.length);
    const digestBuf = await subtle.digest('SHA-256', buf);
    const digest = new Uint8Array(digestBuf);
    if (leadingZeroBits(digest) >= difficulty) {
      const elapsed_ms = performance.now() - start;
      (self as unknown as Worker).postMessage({
        type: 'done',
        nonce: nonceStr,
        tried,
        elapsed_ms,
      });
      // Free the worker — caller terminates anyway, but explicit
      // close speeds up the message queue flush.
      self.close();
      return;
    }
    nonce++;
    if (tried % PROGRESS_EVERY === 0) {
      (self as unknown as Worker).postMessage({ type: 'progress', tried });
    }
  }
};

function leadingZeroBits(bytes: Uint8Array): number {
  let n = 0;
  for (let i = 0; i < bytes.length; i++) {
    const b = bytes[i];
    if (b === 0) {
      n += 8;
      continue;
    }
    // Count leading zeros in the byte (8-bit).
    let zeros = 0;
    for (let mask = 0x80; mask > 0; mask >>>= 1) {
      if ((b & mask) !== 0) break;
      zeros++;
    }
    n += zeros;
    return n;
  }
  return n;
}

export {};
