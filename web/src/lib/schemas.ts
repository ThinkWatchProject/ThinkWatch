/**
 * Runtime validation schemas for the most critical API responses.
 *
 * These are intentionally **not** comprehensive — only endpoints whose
 * shape mismatch would silently break the dashboard or auth flow get a
 * schema. The rest still go through the unvalidated `api<T>(...)` path.
 *
 * Why zod and not "trust the TypeScript types"? TypeScript types are a
 * compile-time fiction; the backend can change a field type or rename
 * something and the frontend will happily `as T` it into a `NaN.toFixed()`
 * crash three rerenders later. Validating the most-trafficked endpoints
 * surfaces backend/frontend drift the moment it happens.
 */

import { z } from 'zod';

// --- /api/setup/status -----------------------------------------------------

export const SetupStatusSchema = z.object({
  initialized: z.boolean(),
  needs_setup: z.boolean(),
});
export type SetupStatus = z.infer<typeof SetupStatusSchema>;

// --- /api/auth/me ----------------------------------------------------------

export const UserResponseSchema = z.object({
  id: z.string().uuid(),
  email: z.string(),
  display_name: z.string(),
  avatar_url: z.string().nullable(),
  is_active: z.boolean(),
  permissions: z.array(z.string()).optional(),
  denied_permissions: z.array(z.string()).optional(),
});
export type UserResponse = z.infer<typeof UserResponseSchema>;

// --- /api/dashboard/live ---------------------------------------------------

export const ProviderHealthSchema = z.object({
  kind: z.union([z.literal('ai'), z.literal('mcp')]),
  provider: z.string(),
  requests: z.number(),
  avg_latency_ms: z.number(),
  /** % of non-throttled requests that succeeded. 429s are excluded so a
   *  quota-throttled-but-responsive upstream still reads as healthy.
   *  `null` when no traffic in the window — render as "—", NOT 100%,
   *  since "no data" and "all calls succeeded" must not look identical. */
  success_rate: z.number().nullable(),
  /** % of total requests rejected with 429. Distinct from success_rate —
   *  signals "fix your quota," not "upstream is broken." `null` when no
   *  traffic in the window. */
  throttled_rate: z.number().nullable(),
  cb_state: z.string(),
});

export const LiveLogRowSchema = z.object({
  kind: z.union([z.literal('api'), z.literal('mcp')]),
  id: z.string(),
  user_id: z.string(),
  subject: z.string(),
  status: z.string(),
  latency_ms: z.number(),
  tokens: z.number(),
  created_at: z.string(),
});

export const DashboardLiveSchema = z.object({
  providers: z.array(ProviderHealthSchema),
  rpm_buckets: z.array(z.number()),
  recent_logs: z.array(LiveLogRowSchema),
  max_rpm_limit: z.number().nullable(),
});

// --- /api/dashboard/top-users ----------------------------------------------

export const TopActiveUserSchema = z.object({
  user_id: z.string(),
  user_email: z.string(),
  request_count: z.number(),
  total_tokens: z.number(),
  last_active: z.string(),
});

export const TopActiveUsersResponseSchema = z.object({
  users: z.array(TopActiveUserSchema),
  /** Distinct active users in the selected window. May exceed
   *  `users.length` when the server-side cap clips the leaderboard. */
  total: z.number(),
});

export type TopActiveUser = z.infer<typeof TopActiveUserSchema>;
export type TopActiveUsersResponse = z.infer<typeof TopActiveUsersResponseSchema>;
export type ProviderHealth = z.infer<typeof ProviderHealthSchema>;
export type LiveLogRow = z.infer<typeof LiveLogRowSchema>;
export type DashboardLive = z.infer<typeof DashboardLiveSchema>;

// --- /api/dashboard/ws-ticket ----------------------------------------------

export const WsTicketSchema = z.object({
  ticket: z.string().min(1),
});
