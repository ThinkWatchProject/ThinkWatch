import { useCallback, useEffect, useRef, useState } from 'react';
import { api } from '@/lib/api';

interface PowChallenge {
  challenge_id: string;
  challenge_random: string;
  difficulty: number;
  issued_at: number;
}

export interface PowSolution {
  challenge_id: string;
  nonce: string;
}

type Status = 'idle' | 'fetching' | 'grinding' | 'ready' | 'error';

interface PowState {
  status: Status;
  /** Set when status === 'ready'. Pass to /api/auth/login. */
  solution: PowSolution | null;
  /** Approximate progress during grinding. */
  tried: number;
  /** Wall-clock grind time in ms. Final value when status==='ready'. */
  elapsedMs: number;
  /** Difficulty (leading zero bits) the server demanded. */
  difficulty: number;
  /** Last error message (network failure / refresh). */
  error: string | null;
}

/**
 * Fetch a fresh challenge and grind a nonce in a Web Worker.
 *
 * The hook runs on mount so the user typing their password gives the
 * grinder a head start — by the time they click Login the solution
 * is usually already `ready`. After a successful (or failed) login,
 * call `refresh()` to mint a new challenge for the next attempt.
 */
export function usePowChallenge(): PowState & { refresh: () => void } {
  const [status, setStatus] = useState<Status>('idle');
  const [solution, setSolution] = useState<PowSolution | null>(null);
  const [tried, setTried] = useState(0);
  const [elapsedMs, setElapsedMs] = useState(0);
  const [difficulty, setDifficulty] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const workerRef = useRef<Worker | null>(null);
  // Cancellation flag so a refresh() during an in-flight grind
  // discards the stale result instead of racing into setSolution.
  const epochRef = useRef(0);

  const start = useCallback(async () => {
    epochRef.current++;
    const epoch = epochRef.current;
    setStatus('fetching');
    setSolution(null);
    setTried(0);
    setElapsedMs(0);
    setError(null);

    // Tear down any in-flight worker.
    if (workerRef.current) {
      workerRef.current.terminate();
      workerRef.current = null;
    }

    let challenge: PowChallenge;
    try {
      challenge = await api<PowChallenge>('/api/auth/pow-challenge', {
        method: 'POST',
        body: {},
        no401Redirect: true,
      });
    } catch (err) {
      if (epoch !== epochRef.current) return;
      setStatus('error');
      setError(err instanceof Error ? err.message : 'Failed to fetch challenge');
      return;
    }
    if (epoch !== epochRef.current) return;

    setDifficulty(challenge.difficulty);
    setStatus('grinding');
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
        setElapsedMs(msg.elapsed_ms);
        setStatus('ready');
        // Worker self-closes after `done`; null the ref so a follow-
        // up refresh() doesn't try to terminate a dead worker.
        workerRef.current = null;
      }
    };
    worker.onerror = (e) => {
      if (epoch !== epochRef.current) return;
      setStatus('error');
      setError(e.message || 'PoW worker crashed');
    };
    worker.postMessage({
      type: 'start',
      challenge_random: challenge.challenge_random,
      difficulty: challenge.difficulty,
    });
  }, []);

  // Mint + grind on first mount.
  useEffect(() => {
    void start();
    return () => {
      // Bump the epoch and tear down so unmount never lands a stale
      // setState into a parent that's already navigated away.
      epochRef.current++;
      if (workerRef.current) {
        workerRef.current.terminate();
        workerRef.current = null;
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return { status, solution, tried, elapsedMs, difficulty, error, refresh: start };
}
