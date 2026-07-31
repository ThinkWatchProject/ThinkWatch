//! Gateway proxy module: shared state, identity, and the four AI
//! surface route handlers. Splits across files for readability —
//! see the leaf modules' docs for what lives where.

use arc_swap::ArcSwap;
use axum::Json;
use axum::response::IntoResponse;
use sqlx::PgPool;
use std::sync::Arc;

use crate::cache::ResponseCache;
use crate::content_filter::ContentFilter;
use crate::cost_tracker::CostTracker;
use crate::health::HealthTracker;
use crate::model_mapping::ModelMapper;
use crate::pii_redactor::PiiRedactor;
use crate::providers::traits::GatewayError;
use crate::quota::QuotaManager;
use crate::rate_limiter::RateLimiter;
use crate::router::ModelRouter;
use think_watch_common::dynamic_config::DynamicConfig;
use think_watch_common::limits::SurfaceConstraints;
use think_watch_common::limits::weight;

mod accounting;
mod body_capture;
mod handlers;
mod headers;
mod identity;
mod log_ctx;
mod pipeline;
mod protocol_relearn;
mod routing;

// pub(crate) re-exports — `lifecycle` module reaches in for these.
pub(crate) use accounting::{post_flight_account, stream_usage_or_estimate};
pub(crate) use body_capture::prepare_body_capture;
pub(crate) use log_ctx::emit_gateway_log_with_extra;
pub(crate) use routing::{SelectionRecord, finalize_health};

// pub re-exports — `server::app` mounts these as route handlers.
pub use handlers::{
    list_models_handler, proxy_anthropic_messages, proxy_chat_completion, proxy_responses,
};

/// Shared application state for the gateway proxy handlers.
#[derive(Clone)]
pub struct GatewayState {
    pub router: Arc<ArcSwap<ModelRouter>>,
    pub model_mapper: Arc<ModelMapper>,
    /// Hot-swappable so admins can update rules without restarting the gateway.
    pub content_filter: Arc<ArcSwap<ContentFilter>>,
    pub quota: Arc<QuotaManager>,
    pub cache: Arc<ResponseCache>,
    /// Hot-swappable so admins can update PII patterns without restarting.
    pub pii_redactor: Arc<ArcSwap<PiiRedactor>>,
    pub cost_tracker: Arc<CostTracker>,
    pub rate_limiter: Arc<RateLimiter>,
    /// PG pool — used to query enabled rate-limit rules and budget caps
    /// per request. Cached above the proxy via `WeightCache` for the
    /// model weights; raw rules go through a separate cache later.
    pub db: PgPool,
    /// Redis client used by the bucketed sliding-window engine and the
    /// natural-period budget counters. Same connection used by `quota`,
    /// `cache`, and the rest of the gateway.
    pub redis: fred::clients::Client,
    /// LRU cache mapping `model_id → (input_weight, output_weight)`.
    /// Looked up once per request to convert raw token counts into
    /// the weighted-token cost the engine consumes.
    pub weight_cache: weight::WeightCache,
    /// Dynamic system settings — read in the hot path to honor
    /// `security.rate_limit_fail_closed` (and other future toggles)
    /// without restarting the gateway. The cache is in-process so
    /// the lookup is a `RwLock::read` + `HashMap::get`.
    pub dynamic_config: Arc<DynamicConfig>,
    /// Audit sink — used to emit one `gateway_logs` row per completed
    /// request (trace_id, tokens, latency, status). Wired up from the
    /// server so the gateway crate doesn't have to build its own.
    pub audit: think_watch_common::audit::AuditLogger,
    /// Per-route rolling-window error / latency tracker. Drives the
    /// circuit-breaker filter at selection time and the `latency`
    /// strategy's weight calculation. Backed by Redis so all gateway
    /// replicas share the same view. See `crate::health` for details.
    pub health: Arc<HealthTracker>,
    /// Body-offload store for the audit pipeline. Oversize request /
    /// response bodies get uploaded to S3-compatible storage instead
    /// of landing inline in CH; the audit row stores an `s3://...`
    /// pointer that the body-viewer endpoints dereference. Defaults
    /// to `InlineStore` (no-op) when no S3 backend is configured —
    /// see `think_watch_common::blob_store` for the policy.
    pub blob_store: Arc<dyn think_watch_common::blob_store::BlobStore>,
}

/// Identity information extracted from the auth middleware.
///
/// Carries the resolved subject IDs the proxy needs in order to
/// query the `rate_limit_rules` / `budget_caps` engine.
#[derive(Debug, Clone, Default)]
pub struct GatewayRequestIdentity {
    pub user_id: Option<String>,
    pub user_email: Option<String>,
    pub api_key_id: Option<String>,
    /// Stable identity that survives api-key rotation. Carries the
    /// `api_keys.lineage_id` of the row that authenticated this
    /// request. Stamped onto every `gateway_logs` emit so the
    /// "this logical key's usage" rollup never has to recurse on
    /// PG via `rotated_from_id`.
    pub api_key_lineage_id: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    /// Merged-across-roles inline limits (most restrictive per
    /// surface+metric+window / surface+period). Computed once by the
    /// auth middleware via `rbac::compute_user_surface_constraints`
    /// and consumed directly here — no side-table lookups on the
    /// hot path.
    pub surface_constraints: SurfaceConstraints,
    /// Resolved client IP (honours `client_ip_source` + `trusted_proxies`).
    /// Populated by the API-key middleware via `extract_client_ip` so
    /// every `gateway_logs` row carries it without each handler reading
    /// headers themselves. `None` only if extraction failed.
    pub ip_address: Option<String>,
}

/// Thin wrapper kept for call-site readability; delegates to
/// `GatewayError::status_code` so the error-path `gateway_logs` row,
/// the streaming `StreamOutcome::UpstreamError` log row, and
/// `GatewayErrorResponse::into_response` all share one mapping.
pub(super) fn gateway_error_status(err: &GatewayError) -> i64 {
    err.status_code()
}

// ---------- Error adapter ----------

/// Newtype wrapper so we can implement `IntoResponse` for `GatewayError`.
pub struct GatewayErrorResponse(GatewayError);

impl From<GatewayError> for GatewayErrorResponse {
    fn from(err: GatewayError) -> Self {
        Self(err)
    }
}

impl IntoResponse for GatewayErrorResponse {
    fn into_response(self) -> axum::response::Response {
        use axum::http::{HeaderValue, StatusCode, header};

        let status =
            StatusCode::from_u16(self.0.status_code() as u16).unwrap_or(StatusCode::BAD_GATEWAY);
        let error_type = match &self.0 {
            GatewayError::ProviderError(_) => "provider_error",
            GatewayError::ProviderHttpError { .. } => "provider_http_error",
            GatewayError::ProviderTimeout(_) => "provider_timeout",
            GatewayError::ProviderInvalidResponse(_) => "provider_invalid_response",
            GatewayError::TransformError(_) => "transform_error",
            GatewayError::NetworkError(_) => "network_error",
            GatewayError::UpstreamRateLimited { .. } | GatewayError::LocalRateLimited(_) => {
                "rate_limited"
            }
            GatewayError::UpstreamAuthError => "auth_error",
        };

        let retry_after = self.0.retry_after_secs();
        let body = serde_json::json!({
            "error": {
                "message": self.0.to_string(),
                "type": error_type,
            }
        });

        let mut response = (status, Json(body)).into_response();
        // Echo the upstream's Retry-After (or our local default) so
        // well-behaved clients back off the right amount instead of
        // burning quota with tight 3× retries that all hit the same
        // open window.
        if let Some(secs) = retry_after
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, v);
        }
        response
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    /// `gateway_error_status` must agree with the HTTP status that
    /// `GatewayErrorResponse::into_response` returns — drift between
    /// them would make the gateway_logs `status_code` field disagree
    /// with the actual response code, and trace UI users would chase
    /// phantom 502s for what was actually a 429.
    #[test]
    fn gateway_error_status_matches_response_status() {
        for (err, expected) in [
            (GatewayError::ProviderError("x".into()), 502),
            (
                GatewayError::ProviderHttpError {
                    status: 418,
                    message: "teapot".into(),
                },
                418,
            ),
            (GatewayError::ProviderTimeout("x".into()), 504),
            (GatewayError::ProviderInvalidResponse("x".into()), 502),
            (GatewayError::TransformError("x".into()), 400),
            (GatewayError::NetworkError("x".into()), 502),
            (
                GatewayError::UpstreamRateLimited {
                    retry_after_secs: None,
                },
                429,
            ),
            (
                GatewayError::UpstreamRateLimited {
                    retry_after_secs: Some(45),
                },
                429,
            ),
            (GatewayError::LocalRateLimited("rule".into()), 429),
            (GatewayError::UpstreamAuthError, 401),
        ] {
            assert_eq!(
                gateway_error_status(&err),
                expected,
                "gateway_error_status mismatch for {err:?}"
            );
            // The wire-status path goes through IntoResponse — exercise
            // it so the two stay in lock-step even after either side is
            // refactored.
            let wire_status = GatewayErrorResponse::from(err)
                .into_response()
                .status()
                .as_u16() as i64;
            assert_eq!(
                wire_status, expected,
                "IntoResponse wire status disagrees with gateway_error_status"
            );
        }
    }

    /// The streaming on_done path historically hard-coded 502 for any
    /// mid-stream upstream failure, so a 429 from OpenRouter would
    /// land in gateway_logs as 502 and the dashboard would paint a
    /// healthy-but-throttled upstream red. Lock in that
    /// `StreamOutcome::UpstreamError` carries the underlying
    /// GatewayError's canonical status verbatim.
    #[test]
    fn stream_outcome_upstream_error_preserves_status() {
        use crate::streaming::StreamOutcome;
        for err in [
            GatewayError::UpstreamRateLimited {
                retry_after_secs: Some(12),
            },
            GatewayError::UpstreamAuthError,
            GatewayError::ProviderTimeout("slow".into()),
            GatewayError::ProviderError("boom".into()),
            GatewayError::ProviderHttpError {
                status: 503,
                message: "down".into(),
            },
        ] {
            let expected = err.status_code();
            let outcome = StreamOutcome::UpstreamError {
                error_type: err.error_tag().to_string(),
                message: err.to_string(),
                status_code: err.status_code(),
            };
            let (logged_status, detail) = outcome.logged_status_and_detail();
            assert_eq!(
                logged_status, expected,
                "stream-path status drift for {err:?}: got {logged_status}, expected {expected}"
            );
            assert!(
                detail.as_ref().and_then(|v| v.get("error_type")).is_some(),
                "stream outcome detail must carry error_type label"
            );
        }
    }

    #[test]
    fn stream_outcome_natural_and_cancelled_have_canonical_status() {
        use crate::streaming::StreamOutcome;
        assert_eq!(StreamOutcome::Natural.logged_status_and_detail().0, 200);
        assert_eq!(
            StreamOutcome::ClientCancelled.logged_status_and_detail().0,
            499
        );
    }

    /// 429 responses MUST carry a `Retry-After` header. Without one,
    /// naive SDKs (the field-observed case that triggered this fix)
    /// retry tightly and re-burn quota that was about to refill —
    /// turning a brief throttle into sustained pain. The upstream's
    /// own value wins; we fall back to a conservative default for
    /// local-limited responses.
    #[test]
    fn rate_limit_responses_carry_retry_after_header() {
        // Upstream provided a hint — echo it.
        let resp = GatewayErrorResponse::from(GatewayError::UpstreamRateLimited {
            retry_after_secs: Some(45),
        })
        .into_response();
        assert_eq!(resp.status().as_u16(), 429);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("45")
        );

        // Upstream silent — we still echo nothing, because we don't
        // know when the quota window opens. (The local-default only
        // applies to OUR own rate limiter, where we DO know.)
        let resp = GatewayErrorResponse::from(GatewayError::UpstreamRateLimited {
            retry_after_secs: None,
        })
        .into_response();
        assert!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none(),
            "no header when upstream didn't tell us — guessing would mislead clients"
        );

        // Local limit — we set our own conservative default so SDKs
        // see a number instead of immediately retrying.
        let resp = GatewayErrorResponse::from(GatewayError::LocalRateLimited("budget".into()))
            .into_response();
        assert_eq!(resp.status().as_u16(), 429);
        assert!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_some(),
            "local rate-limit must carry a Retry-After default"
        );

        // Non-429 responses must NOT carry Retry-After — would
        // confuse SDKs that special-case the header.
        let resp =
            GatewayErrorResponse::from(GatewayError::ProviderError("boom".into())).into_response();
        assert_eq!(resp.status().as_u16(), 502);
        assert!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none()
        );
    }

    #[test]
    fn retry_after_parser_handles_delta_seconds_and_garbage() {
        use crate::providers::traits::parse_retry_after_seconds;
        assert_eq!(parse_retry_after_seconds("30"), Some(30));
        assert_eq!(parse_retry_after_seconds("  45  "), Some(45));
        assert_eq!(parse_retry_after_seconds("0"), Some(0));
        // HTTP-date form — intentionally unsupported (rare in practice
        // for LLM providers). Treated as "no hint".
        assert_eq!(
            parse_retry_after_seconds("Wed, 21 Oct 2025 07:28:00 GMT"),
            None
        );
        assert_eq!(parse_retry_after_seconds(""), None);
        assert_eq!(parse_retry_after_seconds("abc"), None);
    }

    #[test]
    fn upstream_rate_limit_hint_is_capped() {
        // An upstream that claims "retry in 2 hours" mostly means "we
        // gave up estimating" — capping at one hour keeps the header
        // useful for retries while not promising to come back in 24h.
        let err = GatewayError::UpstreamRateLimited {
            retry_after_secs: Some(7200),
        };
        assert_eq!(err.retry_after_secs(), Some(3600));
    }
}
