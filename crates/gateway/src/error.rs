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
    /// Upstream returned 401 or 403: the gateway's credential for this
    /// route was refused. `status` is which, and `message` the upstream's
    /// own wording, truncated. Both are for operators and the import
    /// probe, and stay out of the text the caller sees — an AWS refusal
    /// names the account and the IAM principal.
    #[error("Authentication failed with upstream")]
    UpstreamAuthError { status: u16, message: String },
    /// Local rate limit / budget cap was hit. `label` names the limit
    /// so the response body can tell the caller WHICH one fired (e.g.
    /// `user:requests/5h`, `api_key_lineage:tokens/1d`,
    /// `user:budget/monthly`). Maps to 429 in `IntoResponse`, with
    /// `Retry-After: retry_after_secs`. `retry` is false for a spent
    /// budget: it frees only when its period ends, so SDKs that retry a
    /// 429 by themselves are told not to (`x-should-retry: false`).
    /// Build it with [`GatewayError::rate_limited`],
    /// [`GatewayError::budget_exhausted`] or
    /// [`GatewayError::limiter_unavailable`].
    #[error("Rate limited: {label}")]
    LocalRateLimited {
        label: String,
        retry_after_secs: u32,
        retry: bool,
    },
    /// Refused by the gateway's own policy — a content filter rule set to
    /// refuse matched what the caller sent, or a tool call the upstream
    /// returned matched a rule set to cut it. Not a malformed request (not
    /// 400) and not the upstream failing (not 502): the gateway will not
    /// pass it on. Maps to 403.
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
            GatewayError::UpstreamRateLimited { .. } | GatewayError::LocalRateLimited { .. } => 429,
            GatewayError::UpstreamAuthError { .. } => 401,
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
            GatewayError::LocalRateLimited { .. } => "LocalRateLimited",
            GatewayError::UpstreamAuthError { .. } => "UpstreamAuthError",
            GatewayError::PolicyBlocked(_) => "PolicyBlocked",
        }
    }

    /// A rate-limit window is full. It has room again in
    /// `retry_after_secs`, by itself.
    pub fn rate_limited(label: impl Into<String>, retry_after_secs: u64) -> Self {
        GatewayError::LocalRateLimited {
            label: label.into(),
            retry_after_secs: clamp_secs(retry_after_secs),
            retry: true,
        }
    }

    /// A budget is spent. It frees when its period ends, in
    /// `retry_after_secs` — far too long for an SDK's automatic retries.
    pub fn budget_exhausted(label: impl Into<String>, retry_after_secs: u64) -> Self {
        GatewayError::LocalRateLimited {
            label: label.into(),
            retry_after_secs: clamp_secs(retry_after_secs),
            retry: false,
        }
    }

    /// The limit counters can't be read and the gateway fails closed.
    /// Nothing says when they will be back; a short wait is a guess.
    pub fn limiter_unavailable(label: impl Into<String>) -> Self {
        const UNAVAILABLE_RETRY_SECS: u32 = 30;
        GatewayError::LocalRateLimited {
            label: label.into(),
            retry_after_secs: UNAVAILABLE_RETRY_SECS,
            retry: true,
        }
    }

    /// Hint, in seconds, for `Retry-After` on a 429 response. For
    /// upstream limits we echo the upstream's own header when present,
    /// capped at one hour to keep the header sane even when an upstream
    /// returns an absurd value. For local limits it is when the limit
    /// lets a request through again — for a budget, the end of its
    /// period, which can be weeks away.
    pub fn retry_after_secs(&self) -> Option<u32> {
        const HARD_CAP_SECS: u32 = 3600;
        match self {
            GatewayError::UpstreamRateLimited { retry_after_secs } => {
                retry_after_secs.map(|s| s.min(HARD_CAP_SECS))
            }
            GatewayError::LocalRateLimited {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            _ => None,
        }
    }

    /// `Some(false)` when the client must not retry on its own: sent as
    /// `x-should-retry`, which the OpenAI and Anthropic SDKs read before
    /// retrying a 429.
    pub fn should_retry(&self) -> Option<bool> {
        match self {
            GatewayError::LocalRateLimited { retry: false, .. } => Some(false),
            _ => None,
        }
    }
}

fn clamp_secs(secs: u64) -> u32 {
    u32::try_from(secs).unwrap_or(u32::MAX).max(1)
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

    #[test]
    fn a_full_window_says_when_it_frees_and_a_spent_budget_says_not_to_retry() {
        let window = GatewayError::rate_limited("user:requests/1m", 17);
        assert_eq!(window.status_code(), 429);
        assert_eq!(window.error_tag(), "LocalRateLimited");
        assert_eq!(window.retry_after_secs(), Some(17));
        assert_eq!(window.should_retry(), None);
        assert_eq!(window.to_string(), "Rate limited: user:requests/1m");

        // A month away is not capped like an upstream's hint.
        let budget = GatewayError::budget_exhausted("user:budget/monthly", 2_000_000);
        assert_eq!(budget.status_code(), 429);
        assert_eq!(budget.error_tag(), "LocalRateLimited");
        assert_eq!(budget.retry_after_secs(), Some(2_000_000));
        assert_eq!(budget.should_retry(), Some(false));

        let down = GatewayError::limiter_unavailable("rate_limiter_unavailable");
        assert_eq!(down.retry_after_secs(), Some(30));
        assert_eq!(down.should_retry(), None);
    }
}
