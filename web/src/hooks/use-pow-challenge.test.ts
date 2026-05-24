import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';

// The hook spawns a real Web Worker via `new Worker(new URL(...))`.
// jsdom has neither Worker nor a module URL resolver, so we replace
// the Worker constructor with a controllable fake that lets each
// test choose whether to emit `progress`, `done`, or stall. The fake
// records the last-spawned instance so a test can poke at its
// onmessage handler directly.

interface FakeWorker {
  postMessage: ReturnType<typeof vi.fn>;
  terminate: ReturnType<typeof vi.fn>;
  onmessage: ((e: MessageEvent) => void) | null;
  onerror: ((e: ErrorEvent) => void) | null;
  terminated: boolean;
  postedMessages: unknown[];
}

const workers: FakeWorker[] = [];

class WorkerStub {
  postMessage = vi.fn();
  terminate = vi.fn();
  onmessage: ((e: MessageEvent) => void) | null = null;
  onerror: ((e: ErrorEvent) => void) | null = null;
  terminated = false;
  postedMessages: unknown[] = [];

  constructor(_url: URL | string, _opts?: WorkerOptions) {
    this.postMessage = vi.fn((msg: unknown) => {
      this.postedMessages.push(msg);
    });
    this.terminate = vi.fn(() => {
      this.terminated = true;
    });
    workers.push(this as unknown as FakeWorker);
  }
}

// Mock `@/lib/api` so the hook doesn't try to fetch the real
// /api/auth/pow-challenge endpoint. Each test controls what mint
// returns by reassigning `mintImpl` before calling the hook.
let mintImpl: (body: { email: string }) => Promise<{
  challenge_id: string;
  challenge_random: string;
  difficulty: number;
  issued_at: number;
  ttl_secs: number;
}> = async () => ({
  challenge_id: 'cid-stub',
  challenge_random: 'rnd-stub',
  difficulty: 19,
  issued_at: 1_700_000_000,
  ttl_secs: 300,
});

vi.mock('@/lib/api', () => ({
  api: vi.fn(
    async (
      path: string,
      opts: { method?: string; body?: { email: string } },
    ) => {
      if (path !== '/api/auth/pow-challenge') {
        throw new Error(`unexpected api call: ${path}`);
      }
      return mintImpl(opts.body!);
    },
  ),
}));

// i18n bundle pulls in real locale JSON in setup.ts. The hook only
// reads `i18n.t('common.error')` on the catch branch — we don't test
// that copy directly, but the import must succeed.

let originalWorker: typeof Worker | undefined;

beforeEach(() => {
  workers.length = 0;
  originalWorker = globalThis.Worker;
  // jsdom's Worker prototype is undefined; assign our stub.
  (globalThis as unknown as { Worker: typeof Worker }).Worker =
    WorkerStub as unknown as typeof Worker;
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  if (originalWorker === undefined) {
    delete (globalThis as Partial<{ Worker: typeof Worker }>).Worker;
  } else {
    (globalThis as unknown as { Worker: typeof Worker }).Worker = originalWorker;
  }
  mintImpl = async () => ({
    challenge_id: 'cid-stub',
    challenge_random: 'rnd-stub',
    difficulty: 19,
    issued_at: 1_700_000_000,
    ttl_secs: 300,
  });
});

// Imported AFTER the mocks above so the hook's `api` import resolves
// to the mock.
const { usePowChallenge } = await import('./use-pow-challenge');

describe('usePowChallenge', () => {
  it('stays idle until the email looks valid', () => {
    const { result } = renderHook(({ email }) => usePowChallenge(email), {
      initialProps: { email: '' },
    });
    expect(result.current.status).toBe('idle');
    // No worker spawned, no debounce armed.
    expect(workers).toHaveLength(0);
  });

  it('does not mint on a half-typed email', () => {
    renderHook(({ email }) => usePowChallenge(email), {
      initialProps: { email: 'half@' },
    });
    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(workers).toHaveLength(0);
  });

  it('debounces 400ms before minting on a valid email', async () => {
    const apiModule = await import('@/lib/api');
    const apiSpy = apiModule.api as unknown as ReturnType<typeof vi.fn>;
    apiSpy.mockClear();

    renderHook(({ email }) => usePowChallenge(email), {
      initialProps: { email: 'alice@example.com' },
    });
    // Below 400ms: no mint yet.
    act(() => {
      vi.advanceTimersByTime(399);
    });
    expect(apiSpy).not.toHaveBeenCalled();
    // Cross the threshold.
    await act(async () => {
      vi.advanceTimersByTime(1);
      await Promise.resolve();
    });
    expect(apiSpy).toHaveBeenCalledTimes(1);
  });

  it('passes the normalized email (trim + lowercase) to mint and to the worker', async () => {
    const captured: { email: string }[] = [];
    mintImpl = async (body) => {
      captured.push(body);
      return {
        challenge_id: 'cid',
        challenge_random: 'rnd',
        difficulty: 19,
        issued_at: 1,
        ttl_secs: 300,
      };
    };
    renderHook(({ email }) => usePowChallenge(email), {
      initialProps: { email: '  ALICE@Example.COM  ' },
    });
    await act(async () => {
      vi.advanceTimersByTime(400);
      // Let microtasks (mint promise resolution + setState) flush.
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(captured).toEqual([{ email: 'alice@example.com' }]);
    expect(workers).toHaveLength(1);
    const startMsg = workers[0].postedMessages[0] as {
      type: string;
      email: string;
      challenge_random: string;
    };
    expect(startMsg.email).toBe('alice@example.com');
  });

  it('clears the prior solution synchronously when email changes after grind completed', async () => {
    const { result, rerender } = renderHook(
      ({ email }) => usePowChallenge(email),
      { initialProps: { email: 'alice@example.com' } },
    );

    // Land alice's mint + grind.
    await act(async () => {
      vi.advanceTimersByTime(400);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(workers).toHaveLength(1);

    // Simulate the worker producing a solution for alice. Real
    // worker self-closes after `done`, so the hook nulls its ref —
    // we mirror that here by not asserting `.terminated` on this
    // particular worker. The next test covers the mid-grind case
    // where termination MUST happen.
    act(() => {
      workers[0].onmessage?.({
        data: { type: 'done', nonce: '42', tried: 100, elapsed_ms: 50 },
      } as MessageEvent);
    });
    expect(result.current.status).toBe('ready');
    expect(result.current.solution).toEqual({
      challenge_id: 'cid-stub',
      nonce: '42',
    });

    // Now user edits the email. The hook MUST drop the stale
    // solution synchronously — otherwise the form can submit alice's
    // solution against bob's email during the 400ms debounce.
    rerender({ email: 'bob@example.com' });
    expect(result.current.solution).toBeNull();
    expect(result.current.status).toBe('fetching');
  });

  it('terminates the in-flight worker when email changes mid-grind', async () => {
    const { rerender } = renderHook(({ email }) => usePowChallenge(email), {
      initialProps: { email: 'alice@example.com' },
    });

    // Mint completes, worker is spawned and grinding. We do NOT
    // emit `done` — the worker is still alive.
    await act(async () => {
      vi.advanceTimersByTime(400);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(workers).toHaveLength(1);
    expect(workers[0].terminated).toBe(false);

    // User edits the email. The synchronous teardown in the effect
    // body MUST call worker.terminate() so the dead worker isn't
    // left burning CPU for the full debounce window.
    rerender({ email: 'bob@example.com' });
    expect(workers[0].terminated).toBe(true);
  });

  it('flips to expired when ttl elapses', async () => {
    mintImpl = async () => ({
      challenge_id: 'cid',
      challenge_random: 'rnd',
      difficulty: 19,
      issued_at: 1,
      ttl_secs: 2,
    });

    const { result } = renderHook(({ email }) => usePowChallenge(email), {
      initialProps: { email: 'alice@example.com' },
    });
    await act(async () => {
      vi.advanceTimersByTime(400);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(result.current.status).toBe('grinding');

    // 2-second TTL elapses BEFORE the worker emits `done` — hook
    // must abandon the grind and flip to `expired`.
    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(result.current.status).toBe('expired');
    expect(result.current.solution).toBeNull();
    expect(workers[0].terminated).toBe(true);
  });

  it('terminates the worker on worker.onerror', async () => {
    const { result } = renderHook(({ email }) => usePowChallenge(email), {
      initialProps: { email: 'alice@example.com' },
    });
    await act(async () => {
      vi.advanceTimersByTime(400);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(workers).toHaveLength(1);

    act(() => {
      workers[0].onerror?.({ message: 'boom' } as ErrorEvent);
    });
    expect(result.current.status).toBe('error');
    expect(result.current.error).toBe('boom');
    expect(workers[0].terminated).toBe(true);
  });

  it('exposes expiresAt as a future timestamp after mint succeeds', async () => {
    mintImpl = async () => ({
      challenge_id: 'cid',
      challenge_random: 'rnd',
      difficulty: 19,
      issued_at: 1,
      ttl_secs: 300,
    });
    const { result } = renderHook(({ email }) => usePowChallenge(email), {
      initialProps: { email: 'alice@example.com' },
    });
    expect(result.current.expiresAt).toBe(0);
    await act(async () => {
      vi.advanceTimersByTime(400);
      await Promise.resolve();
      await Promise.resolve();
    });
    // Hook called Date.now() while fake timers were active, so
    // expiresAt is in the *fake* timeline. It must equal "now in
    // the fake timeline" + ttl. Don't use waitFor here — it uses
    // real timers internally and would never resolve under fake.
    expect(result.current.expiresAt).toBe(Date.now() + 300_000);
  });
});
