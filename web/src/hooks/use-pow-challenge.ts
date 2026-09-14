import { useCallback, useEffect, useRef, useState } from 'react';
import { useResetOnChange } from '@/hooks/use-reset-on-change';
import { api } from '@/lib/api';
import i18n from '@/i18n';

interface PowChallenge {
  challenge_id: string;
  challenge_random: string;
  difficulty: number;
  issued_at: number;
  ttl_secs: number;
}

export interface PowSolution {
  challenge_id: string;
  nonce: string;
}

type Status = 'idle' | 'fetching' | 'grinding' | 'ready' | 'expired' | 'error';

interface PowState {
  status: Status;
  /** Set when status === 'ready'. Pass to /api/auth/login. */
  solution: PowSolution | null;
  /** Hash count emitted by the worker; drives the progress bar. */
  tried: number;
  /** Difficulty (leading zero bits) the server demanded. */
  difficulty: number;
  /** Last error message (network failure / refresh). */
  error: string | null;
  /** Absolute wall-clock ms at which the server-side challenge dies.
   *  Submit handlers should re-check `Date.now() >= expiresAt` because
   *  backgrounded-tab setTimeout throttling can delay the in-hook
   *  expiry transition by minutes. 0 when no challenge is in flight. */
  expiresAt: number;
}

/**
 * Cheap client-side check matching the server's email validator
 * shape: must contain `@`, the local-part non-empty, the domain
 * must be ≥ 3 chars and contain a dot not at either edge. We don't
 * need to be strict here — the server re-validates on mint. We
 * just need a gate that prevents us from minting on every keystroke
 * the user types before they finish their address.
 */
function looksLikeEmail(s: string): boolean {
  const at = s.indexOf('@');
  if (at <= 0 || at !== s.lastIndexOf('@')) return false;
  const domain = s.slice(at + 1);
  if (domain.length < 3) return false;
  if (domain.startsWith('.') || domain.endsWith('.')) return false;
  return domain.includes('.');
}

/**
 * Fetch a fresh challenge bound to `email` and grind a nonce in a
 * Web Worker.
 *
 * The PoW is **email-bound** server-side — the challenge_random is
 * mixed with the email at hash time, so a challenge ground for
 * `alice@x` can't be replayed against `bob@x`. That means we can't
 * pre-mint blindly: we wait for the user to type a plausible
 * email, debounce 400ms, then start. If they change their email,
 * the in-flight challenge is torn down and a new one starts.
 *
 * Each challenge has a server-side Redis TTL (`ttl_secs`). The hook
 * schedules a timer at receive-time to flip into `expired` before
 * the server starts rejecting the solution as stale.
 */
export function usePowChallenge(email: string): PowState & { refresh: () => void } {
  const [status, setStatus] = useState<Status>('idle');
  const [solution, setSolution] = useState<PowSolution | null>(null);
  const [tried, setTried] = useState(0);
  const [difficulty, setDifficulty] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [expiresAt, setExpiresAt] = useState(0);
  const workerRef = useRef<Worker | null>(null);
  const expiryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const expiresAtRef = useRef(0);
  // Cancellation flag so a refresh() during an in-flight grind
  // discards the stale result instead of racing into setSolution.
  const epochRef = useRef(0);

  // Normalize to match what the server stores (trim + lowercase).
  // Hashing this exact form ensures verify on the server side
  // computes the same SHA-256 input.
  const normalizedEmail = email.trim().toLowerCase();
  const emailReady = looksLikeEmail(normalizedEmail);

  const clearExpiryTimer = useCallback(() => {
    if (expiryTimerRef.current) {
      clearTimeout(expiryTimerRef.current);
      expiryTimerRef.current = null;
    }
  }, []);

  const teardown = useCallback(() => {
    epochRef.current++;
    if (workerRef.current) {
      workerRef.current.terminate();
      workerRef.current = null;
    }
    clearExpiryTimer();
    expiresAtRef.current = 0;
  }, [clearExpiryTimer]);

  const start = useCallback(
    async (mintEmail: string) => {
      epochRef.current++;
      const epoch = epochRef.current;
      setStatus('fetching');
      setSolution(null);
      setTried(0);
      setError(null);

      if (workerRef.current) {
        workerRef.current.terminate();
        workerRef.current = null;
      }
      clearExpiryTimer();
      // Reset expiresAtRef BEFORE the network fetch. The
      // visibilitychange listener reads this ref to decide whether
      // to flip to `expired`; if we leave the previous challenge's
      // expiry value in place during the fetch, a tab-resume in
      // that window could see a stale "still valid" timestamp and
      // make an incorrect decision. Matches the discipline of the
      // setTimeout body and visibility handler (which both clear
      // the ref before mutating state).
      expiresAtRef.current = 0;

      let challenge: PowChallenge;
      try {
        challenge = await api<PowChallenge>('/api/auth/pow-challenge', {
          method: 'POST',
          body: { email: mintEmail },
          no401Redirect: true,
        });
      } catch (err) {
        if (epoch !== epochRef.current) return;
        setStatus('error');
        setError(err instanceof Error ? err.message : i18n.t('common.error'));
        return;
      }
      if (epoch !== epochRef.current) return;

      setDifficulty(challenge.difficulty);
      setStatus('grinding');

      // Record absolute expiry. The setTimeout below is the primary
      // expiry trip; the absolute timestamp is a belt-and-suspenders
      // check for the submit handler and the visibilitychange
      // listener, both of which want a reliable "is this still live"
      // signal when background-tab throttling can stall the timer.
      const expiresAtMs = Date.now() + challenge.ttl_secs * 1000;
      expiresAtRef.current = expiresAtMs;
      setExpiresAt(expiresAtMs);

      // Arm the expiry trip from receive-time. Fires whether the
      // worker is still grinding or already done — in both cases
      // the challenge is dead server-side and the user must retry.
      //
      // CRITICAL: bump `epochRef.current` BEFORE clearing state.
      // A worker that already posted `done` leaves the message in
      // the main thread's queue even after we terminate it. Without
      // the epoch bump, that queued message would race past our
      // `expired` setState and overwrite status back to `ready` —
      // pointing at a server-evicted challenge. login.tsx's
      // expiresAt guard wouldn't catch it (we cleared expiresAt to
      // 0 here, so the `>= 0` comparison passes), and the user
      // gets a confusing 400 on submit.
      expiryTimerRef.current = setTimeout(() => {
        if (epoch !== epochRef.current) return;
        epochRef.current++;
        if (workerRef.current) {
          workerRef.current.terminate();
          workerRef.current = null;
        }
        expiresAtRef.current = 0;
        setExpiresAt(0);
        setSolution(null);
        setStatus('expired');
      }, challenge.ttl_secs * 1000);

      const worker = new Worker(new URL('@/lib/pow-worker.ts', import.meta.url), {
        type: 'module',
      });
      workerRef.current = worker;
      worker.onmessage = (e: MessageEvent) => {
        if (epoch !== epochRef.current) return;
        const msg = e.data as
          | { type: 'progress'; tried: number }
          | { type: 'done'; nonce: string; tried: number; elapsed_ms: number };
        if (msg.type === 'progress') {
          setTried(msg.tried);
          return;
        }
        if (msg.type === 'done') {
          setSolution({ challenge_id: challenge.challenge_id, nonce: msg.nonce });
          setTried(msg.tried);
          setStatus('ready');
          workerRef.current = null;
        }
      };
      worker.onerror = (e) => {
        if (epoch !== epochRef.current) return;
        // Bump epoch BEFORE mutating state, matching the setTimeout
        // and visibility handlers. A worker that crashed could
        // theoretically have already posted a `done` message before
        // erroring; without the bump, the queued `done` would beat
        // our `error` setState and the chip would mis-report 'ready'
        // for a dead worker.
        epochRef.current++;
        if (workerRef.current) {
          workerRef.current.terminate();
          workerRef.current = null;
        }
        clearExpiryTimer();
        expiresAtRef.current = 0;
        setExpiresAt(0);
        setStatus('error');
        setError(e.message || 'PoW worker crashed');
      };
      worker.postMessage({
        type: 'start',
        challenge_random: challenge.challenge_random,
        email: mintEmail,
        difficulty: challenge.difficulty,
      });
    },
    [clearExpiryTimer],
  );

  // Mint when the email looks valid; tear down + go idle when not.
  // Debounce 400ms so per-keystroke typing doesn't burn the per-IP
  // mint rate limit (60/min).
  //
  // IMPORTANT: teardown() runs SYNCHRONOUSLY on every re-run (not
  // just the !emailReady branch). If the email transitions from one
  // valid form to another while a solution is already `ready`, we
  // must clear that solution and bump the epoch immediately —
  // otherwise the form remains submittable for up to 400ms with a
  // solution bound to the previous email. The server would reject
  // it (defense in depth via the email-binding check) but the user
  // sees "Invalid credentials" instead of an honest retry.
  // Two halves, deliberately split. Invalidating the solution is state
  // alignment and belongs in render: the whole point of this block is that
  // the form must **stop being submittable immediately**, and an effect
  // leaves it submittable for the frame in between.
  useResetOnChange(`${emailReady}\u0000${normalizedEmail}`, () => {
    setSolution(null);
    setTried(0);
    setError(null);
    setExpiresAt(0);
    setStatus(emailReady ? 'fetching' : 'idle');
  });

  // Tearing down the worker and scheduling the next fetch are side effects,
  // and stay here.
  useEffect(() => {
    teardown();
    if (!emailReady) return;
    const handle = setTimeout(() => void start(normalizedEmail), 400);
    return () => clearTimeout(handle);
  }, [emailReady, normalizedEmail, start, teardown]);

  // Backgrounded-tab safety net: setTimeout is throttled (Chrome
  // clamps to ≥1s, Firefox similar) and can drift by minutes when
  // the tab isn't visible. When the user comes back, force-check
  // whether the challenge has died and flip to `expired` immediately
  // so the submit button disables before they click.
  useEffect(() => {
    const onVisibility = () => {
      if (document.visibilityState !== 'visible') return;
      const expiry = expiresAtRef.current;
      if (expiry === 0) return;
      if (Date.now() < expiry) return;
      // Bump epoch before mutating state — same race as the
      // setTimeout body: a queued `done` from the (still-listening)
      // worker would otherwise overwrite our `expired` flip back to
      // `ready` after we returned.
      epochRef.current++;
      if (workerRef.current) {
        workerRef.current.terminate();
        workerRef.current = null;
      }
      clearExpiryTimer();
      expiresAtRef.current = 0;
      setExpiresAt(0);
      setSolution(null);
      setStatus('expired');
    };
    document.addEventListener('visibilitychange', onVisibility);
    return () => document.removeEventListener('visibilitychange', onVisibility);
  }, [clearExpiryTimer]);

  // Tear down on unmount.
  useEffect(() => {
    return () => teardown();
  }, [teardown]);

  return {
    status,
    solution,
    tried,
    difficulty,
    error,
    expiresAt,
    refresh: () => {
      if (emailReady) void start(normalizedEmail);
    },
  };
}
