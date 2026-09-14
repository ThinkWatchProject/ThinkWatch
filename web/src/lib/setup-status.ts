import type { SetupStatus } from '@/lib/schemas';

/**
 * Whether this deployment still needs the first-run wizard.
 *
 * Cached at module scope because the answer flips exactly once in the life
 * of an installation, and every mount of the root component would otherwise
 * pay a round-trip before it can render anything at all.
 *
 * It lives here rather than next to the router because `router.tsx` may only
 * export route definitions — a module that exports both components and plain
 * values loses Fast Refresh.
 */
let cached: SetupStatus | null = null;

export function readSetupStatus(): SetupStatus | null {
  return cached;
}

export function rememberSetupStatus(status: SetupStatus): void {
  cached = status;
}

/** Force the next mount to re-fetch `/api/setup/status`. Called by the setup
 * wizard after a successful initialize so the user lands on the real app
 * immediately, without a hard refresh. */
export function invalidateSetupStatusCache(): void {
  cached = null;
}
