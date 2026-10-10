// An API key's own rate-limit rules and budgets with what each has used,
// from the limits endpoints with `kind = api_key` (they resolve the key id
// to its lineage, so the limits follow the key across rotations):
//
//   GET /api/admin/limits/api_key/{id}/rules
//   GET /api/admin/limits/api_key/{id}/budgets
//   GET /api/admin/limits/api_key/{id}/usage
//
// All three need `rate_limits:read` in a scope that covers the key.

import { api } from '@/lib/api';

export type KeyLimitSurface = 'ai_gateway' | 'mcp_gateway';
export type KeyLimitMetric = 'requests' | 'tokens';
export type KeyLimitPeriod = 'daily' | 'weekly' | 'monthly';

export interface KeyRule {
  id: string;
  surface: KeyLimitSurface;
  metric: KeyLimitMetric;
  window_secs: number;
  max_count: number;
  enabled: boolean;
  expires_at?: string | null;
  reason?: string | null;
  /** The window's count now. */
  current: number;
}

export interface KeyCap {
  id: string;
  period: KeyLimitPeriod;
  limit_tokens: number;
  enabled: boolean;
  expires_at?: string | null;
  reason?: string | null;
  /** The period's weighted tokens so far. */
  current: number;
}

export interface KeyLimits {
  rules: KeyRule[];
  caps: KeyCap[];
}

interface Items<T> {
  items: T[];
}

interface UsageResponse {
  rules: { rule_id: string; current: number }[];
  caps: { cap_id: string; current: number }[];
}

export function keyLimitsQueryKey(keyId: string) {
  return ['admin', 'limits', 'api_key', keyId] as const;
}

export function keyLimitsQuery(keyId: string) {
  return {
    queryKey: keyLimitsQueryKey(keyId),
    queryFn: async ({ signal }: { signal: AbortSignal }): Promise<KeyLimits> => {
      const base = `/api/admin/limits/api_key/${keyId}`;
      const [rules, caps, usage] = await Promise.all([
        api<Items<Omit<KeyRule, 'current'>>>(`${base}/rules`, { signal }),
        api<Items<Omit<KeyCap, 'current'>>>(`${base}/budgets`, { signal }),
        api<UsageResponse>(`${base}/usage`, { signal }),
      ]);
      const ruleUsed = new Map(usage.rules.map((u) => [u.rule_id, u.current]));
      const capUsed = new Map(usage.caps.map((u) => [u.cap_id, u.current]));
      return {
        rules: rules.items.map((r) => ({ ...r, current: ruleUsed.get(r.id) ?? 0 })),
        caps: caps.items.map((c) => ({ ...c, current: capUsed.get(c.id) ?? 0 })),
      };
    },
  };
}

/** The caller holds `rate_limits:read`, but not in a scope covering this key. */
export function isForbidden(error: unknown): boolean {
  return (error as { status?: number } | null)?.status === 403;
}
