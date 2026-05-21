/**
 * Wire shapes + UI-state types shared across the dashboard panels.
 *
 * The Stats / Usage / Cost interfaces here mirror the corresponding
 * server response shapes — they're NOT regenerated from OpenAPI
 * because they cover three independent endpoints the dashboard
 * happens to fan out to in parallel.
 */

export interface DashboardStats {
  total_requests: number;
  active_providers: number;
  active_api_keys: number;
  connected_mcp_servers: number;
  active_keys_buckets: number[];
  range: string;
  prev_total_requests?: number;
  prev_active_api_keys?: number;
}

export interface UsageStats {
  total_tokens: number;
  total_requests: number;
  tokens_buckets: number[];
  range: string;
  prev_total_tokens?: number;
  prev_total_requests?: number;
}

export interface CostStats {
  /**
   * Decimal string on the wire (CH `Decimal(18, 10)`). Dashboard
   * converts to Number for the card widgets — the backend is the
   * source of truth for billing-grade precision; the dashboard just
   * needs the rough value for display.
   */
  total_cost: string;
  budget_usage_pct: number | null;
  cost_buckets: string[];
  range: string;
  total_cost_mtd: string;
  prev_total_cost?: string;
}

export type TimeRange = '24h' | '7d' | '30d';
export const TIME_RANGES: readonly TimeRange[] = ['24h', '7d', '30d'] as const;

/** Persisted layout payload — stat-card ordering + future widget prefs. */
export interface LayoutPayload {
  stat_order?: string[];
}

/** Provider-health filter tab state. */
export type ProviderFilter = 'all' | 'ai' | 'mcp';
