/// Shared types, constants, and pure helpers for the Models page.
/// Pulled out of the route's `index.tsx` so subcomponents
/// (`ModelRowCell`, `CostPreview`, `OutputGuardrailsCard`, …) can
/// reference them without the whole route having to re-export them.

// Decimal fields come back from sqlx as strings (rust_decimal's default
// Serialize) — keep them that way in TS so we don't lose precision on
// parse, and let the form work in string space too.
export interface ModelRow {
  id: string;
  model_id: string;
  display_name: string;
  /// Relative input-token cost factor. Absolute USD cost is
  /// `platform_pricing.input_price_per_token × input_weight × tokens`.
  input_weight: string;
  output_weight: string;
  route_count: number;
  enabled_route_count: number;
  /// Model-level kill switch. FALSE ⇒ all routes are skipped at the
  /// gateway regardless of per-route `enabled` state.
  enabled: boolean;
  /// Provider display names attached to this model, ordered by weight DESC.
  /// Joined server-side so the row can render the column without
  /// fetching per-row routes.
  providers: string[];
  /// Per-model routing override; null/undefined ⇒ use global default.
  routing_strategy?: RoutingStrategy | null;
  affinity_mode?: AffinityMode | null;
  affinity_ttl_secs?: number | null;
  /// Raw guardrails JSON from the server — discriminator-tagged
  /// objects. Decoded into known variants at edit-open via
  /// `parseGuardrails`; today only `max_length` lands.
  output_guardrails?: OutputGuardrail[] | null;
}

/// Output guardrail rule shape — discriminated on `type` to match
/// the Rust `#[serde(tag = "type", rename_all = "snake_case")]`
/// encoding in `crates/gateway/src/output_guardrails.rs`. Today only
/// `max_length` lands; other variants stay TODO in the roadmap.
export type OutputGuardrail = { type: 'max_length'; max_chars: number };

export function parseGuardrails(
  value: OutputGuardrail[] | null | undefined,
): OutputGuardrail[] {
  if (!Array.isArray(value)) return [];
  // Filter to known variants — keeps the form state strongly typed so
  // future additions (json_schema, toxicity) require an explicit branch.
  return value.filter((g): g is OutputGuardrail => g?.type === 'max_length');
}

/// Default for the inline add form. 4096 covers most chat-completion
/// caps without surprising the admin who immediately saves.
export const DEFAULT_MAX_CHARS = 4096;
/// Mirrors the server-side ceiling in
/// `crates/gateway/src/output_guardrails.rs::MAX_LENGTH_CAP_CEILING`.
export const MAX_CHARS_CEILING = 1_000_000;

export type RoutingStrategy = 'weighted' | 'latency' | 'health' | 'latency_health';
export type AffinityMode = 'none' | 'provider' | 'route';

/// All four strategies — used by the global Settings page picker. The
/// per-model UI splits this into "manual = weighted" vs "auto = one of
/// the other three picked via a sub-picker".
export const ROUTING_STRATEGIES: RoutingStrategy[] = [
  'weighted',
  'latency',
  'health',
  'latency_health',
];

/// Auto-mode targets shown in the per-model sub-picker. Order = display
/// order. `latency_health` first because it's the global default.
export const AUTO_TARGETS: RoutingStrategy[] = ['latency_health', 'latency', 'health'];

export const AFFINITY_MODES: AffinityMode[] = ['none', 'provider', 'route'];

export type BreakerState = 'closed' | 'open' | 'half_open';

export interface RouteHealth {
  state: BreakerState;
  total: number;
  errors: number;
  error_pct: number;
  ewma_latency_ms?: number | null;
  /// Cumulative all-time request count for this route. Outlives the
  /// rolling window — operators tuning weights use it to tell apart
  /// "no traffic yet" from "quiet right now".
  lifetime_requests: number;
}

export interface RouteHealthEntry {
  route_id: string;
  provider_id: string;
  provider_name: string;
  upstream_model: string;
  weight: number;
  enabled: boolean;
  health: RouteHealth;
}

export type ModelStatus = 'active' | 'disabled' | 'unrouted';

export function modelStatus(m: ModelRow): ModelStatus {
  if (m.route_count === 0) return 'unrouted';
  if (!m.enabled || m.enabled_route_count === 0) return 'disabled';
  return 'active';
}

export interface PlatformPricing {
  input_price_per_token: string;
  output_price_per_token: string;
  currency: string;
}

export interface RouteRow {
  id: string;
  model_id: string;
  provider_id: string;
  provider_name: string;
  upstream_model: string;
  weight: number;
  enabled: boolean;
  /// Optional human-readable identifier shown in the route table
  /// (e.g. "EU-primary"). Null when admin hasn't set one.
  label?: string | null;
  /// Free-form admin note. Surfaced only in the edit dialog.
  notes?: string | null;
  rpm_cap?: number | null;
  tpm_cap?: number | null;
}

export interface RouteHistoryBucket {
  ts: number;
  p50_ms: number | null;
  p95_ms: number | null;
  requests: number;
  errors: number;
}

export interface RouteHistoryResponse {
  buckets: RouteHistoryBucket[];
}

export interface ModelFormState {
  model_id: string;
  display_name: string;
  input_weight: string;
  output_weight: string;
  /// Empty string = inherit global default. Form serializes that
  /// to `null` on submit so the backend stores the override as NULL.
  routing_strategy: '' | RoutingStrategy;
  affinity_mode: '' | AffinityMode;
  affinity_ttl_secs: string;
  /// Per-model output guardrails. Replaced wholesale on submit
  /// (PATCH array semantics on the server). Empty = no guardrails.
  output_guardrails: OutputGuardrail[];
}

export interface RouteFormState {
  provider_id: string;
  upstream_model: string;
  enabled: boolean;
  /// Optional human-readable identifier — surfaced in the route table.
  label: string;
  /// Free-form admin note. Empty = none.
  notes: string;
  /// Empty string ⇒ unlimited (NULL).
  rpm_cap: string;
  tpm_cap: string;
}

export const emptyModelForm: ModelFormState = {
  model_id: '',
  display_name: '',
  input_weight: '1.0',
  output_weight: '1.0',
  routing_strategy: '',
  affinity_mode: '',
  affinity_ttl_secs: '',
  output_guardrails: [],
};

export const emptyRouteForm: RouteFormState = {
  provider_id: '',
  upstream_model: '',
  enabled: true,
  label: '',
  notes: '',
  rpm_cap: '',
  tpm_cap: '',
};
