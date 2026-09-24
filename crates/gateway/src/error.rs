//! The gateway's error, and the one header parse that feeds it.

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// Catch-all upstream failure that doesn't fit one of the more
    /// specific variants below. Prefer `ProviderHttpError` /
    /// `ProviderTimeout` / `ProviderInvalidResponse` when the cause
    /// is known so dashboards can split errors by class instead of
    /// regex'ing the message.
    #[error("Provider error: {0}")]
    ProviderError(String),
    /// Upstream returned a non-2xx, non-429, non-401 status. The
    /// status is kept structured so error-classifier metrics stay
    /// readable and the gateway can classify retry-eligible 5xx
    /// versus poison 4xx without parsing the message.
    #[error("Provider HTTP {status}: {message}")]
    ProviderHttpError { status: u16, message: String },
    /// Upstream took longer than the configured timeout. Distinct
    /// from a network drop because the request reached the upstream
    /// — only the response was missing in time.
    #[error("Provider timeout: {0}")]
    ProviderTimeout(String),
    /// Upstream responded but the body wasn't parseable as the
    /// expected schema (chat completion / messages / etc.). Almost
    /// always indicates an upstream incident or a model-specific
    /// quirk, and is poison for retries — failover should still
    /// happen but retry against the SAME upstream is pointless.
    #[error("Provider returned invalid response: {0}")]
    ProviderInvalidResponse(String),
    #[error("Request transform error: {0}")]
    TransformError(String),
    #[error("Network error: {0}")]
    NetworkError(String),
    /// Upstream returned 429. `retry_after_secs` captures the value
    /// parsed off the upstream's `Retry-After` header (delta-seconds
    /// form per RFC 7231) so we can echo it to our client and stop
    /// clients spinning into a tight retry loop while quota is still
    /// burning. `None` means the upstream didn't tell us — we pick a
    /// conservative default downstream.
    #[error("Rate limited by upstream")]
    UpstreamRateLimited { retry_after_secs: Option<u32> },
    #[error("Authentication failed with upstream")]
    UpstreamAuthError,
    /// Local rate limit / budget cap was hit. The String is the rule
    /// label so the response body can tell the caller WHICH limit
    /// fired (e.g. "user requests/5h", "api_key tokens/1d",
    /// "monthly budget"). Maps to 429 in `IntoResponse`.
    #[error("Rate limited: {0}")]
    LocalRateLimited(String),
    /// Refused by the gateway's own policy — a tool call the upstream
    /// returned matched a rule set to cut it. Neither the caller's fault
    /// (not 400) nor the upstream failing (not 502): the answer exists
    /// and the gateway will not hand it over. Maps to 403.
    #[error("Blocked by policy: {0}")]
    PolicyBlocked(String),
}

impl GatewayError {
    /// Canonical HTTP status code for this error variant. Single source
    /// of truth shared between the response wire status
    /// (`GatewayErrorResponse::into_response`), the non-streaming log
    /// row writer, and the streaming `StreamOutcome::UpstreamError`
    /// path — drift between any of these would make the gateway_logs
    /// `status_code` field disagree with what the client saw, leading
    /// operators to chase phantom 502s for what was actually a 429.
    pub fn status_code(&self) -> i64 {
        match self {
            GatewayError::ProviderError(_) => 502,
            GatewayError::ProviderHttpError { status, .. } => i64::from(*status),
            GatewayError::ProviderTimeout(_) => 504,
            GatewayError::ProviderInvalidResponse(_) => 502,
            GatewayError::TransformError(_) => 400,
            GatewayError::NetworkError(_) => 502,
            GatewayError::UpstreamRateLimited { .. } | GatewayError::LocalRateLimited(_) => 429,
            GatewayError::UpstreamAuthError => 401,
            GatewayError::PolicyBlocked(_) => 403,
        }
    }

    /// Short stable tag derived from the variant name. Used as a
    /// dashboard-friendly label (Prometheus value, gateway_logs
    /// `error_type` field). Never localize — operators grep on these.
    pub fn error_tag(&self) -> &'static str {
        match self {
            GatewayError::ProviderError(_) => "ProviderError",
            GatewayError::ProviderHttpError { .. } => "ProviderHttpError",
            GatewayError::ProviderTimeout(_) => "ProviderTimeout",
            GatewayError::ProviderInvalidResponse(_) => "ProviderInvalidResponse",
            GatewayError::TransformError(_) => "TransformError",
            GatewayError::NetworkError(_) => "NetworkError",
            GatewayError::UpstreamRateLimited { .. } => "UpstreamRateLimited",
            GatewayError::LocalRateLimited(_) => "LocalRateLimited",
            GatewayError::UpstreamAuthError => "UpstreamAuthError",
            GatewayError::PolicyBlocked(_) => "PolicyBlocked",
        }
    }

    /// Hint, in seconds, for `Retry-After` on a 429 response. For
    /// upstream limits we echo the upstream's own header when present;
    /// for local limits we fall back to a conservative 30s so naive
    /// clients don't spin into a tight retry loop while the bucket is
    /// still refilling. Capped at one hour to keep the header sane
    /// even when an upstream returns an absurd value.
    pub fn retry_after_secs(&self) -> Option<u32> {
        const HARD_CAP_SECS: u32 = 3600;
        const LOCAL_DEFAULT_SECS: u32 = 30;
        match self {
            GatewayError::UpstreamRateLimited { retry_after_secs } => {
                retry_after_secs.map(|s| s.min(HARD_CAP_SECS))
            }
            GatewayError::LocalRateLimited(_) => Some(LOCAL_DEFAULT_SECS),
            _ => None,
        }
    }
}

/// Parse RFC 7231 `Retry-After` (delta-seconds form). HTTP-date is
/// intentionally not supported — the absolute-time variant is
/// effectively unused by upstream LLM providers and would require
/// dragging in a date parser plus clock-skew handling for a vanishingly
/// rare path. Bad input silently maps to None, mirroring how a missing
/// header is treated; a malformed header is no better than no header.
pub fn parse_retry_after_seconds(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_refusal_is_forbidden_not_a_bad_request_or_an_upstream_failure() {
        let e = GatewayError::PolicyBlocked("the tool call matched a rule".into());
        assert_eq!(e.status_code(), 403);
        assert_eq!(e.error_tag(), "PolicyBlocked");
        assert_eq!(e.retry_after_secs(), None);
    }
}
