//! Upstream wire protocol — which API dialect the gateway speaks to a
//! given route's upstream.
//!
//! This used to be implied by `providers.provider_type`: one provider
//! record meant one adapter for every model behind it. That breaks on
//! aggregators that serve several model families over one host and one
//! credential but expose a *different* API per family — e.g. an
//! endpoint that answers `anthropic.*` only on `/v1/messages` while
//! everything else lives on `/v1/chat/completions`. Modelling the
//! protocol per route lets one provider record cover all of them.
//!
//! `model_routes.upstream_protocol` stores the resolved value; NULL
//! means "not determined yet" and the runtime falls back to
//! [`UpstreamProtocol::default_for_provider_type`].

use std::fmt;

/// Wire dialect used when talking to an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpstreamProtocol {
    /// `POST {base}/v1/chat/completions` — OpenAI Chat Completions.
    OpenAiChat,
    /// `POST {base}/v1/responses` — OpenAI Responses API (2025+).
    OpenAiResponses,
    /// `POST {base}/v1/messages` — Anthropic Messages.
    AnthropicMessages,
    /// `POST {base}/v1beta/models/{model}:generateContent` — Gemini.
    GoogleGenerate,
    /// AWS Bedrock Runtime with SigV4 — no base URL, region-derived host.
    BedrockNative,
}

impl UpstreamProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GoogleGenerate => "google_generate",
            Self::BedrockNative => "bedrock_native",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai_chat" => Some(Self::OpenAiChat),
            "openai_responses" => Some(Self::OpenAiResponses),
            "anthropic_messages" => Some(Self::AnthropicMessages),
            "google_generate" => Some(Self::GoogleGenerate),
            "bedrock_native" => Some(Self::BedrockNative),
            _ => None,
        }
    }

    /// What a provider of this type speaks unless a route says
    /// otherwise. Preserves the pre-per-route-protocol behaviour, so an
    /// un-probed route behaves exactly as it did before.
    pub fn default_for_provider_type(provider_type: &str) -> Self {
        match provider_type {
            "anthropic" => Self::AnthropicMessages,
            "google" => Self::GoogleGenerate,
            "bedrock" => Self::BedrockNative,
            // openai, azure_openai, custom, and anything unknown.
            _ => Self::OpenAiChat,
        }
    }

    /// Ordered candidates to try for `upstream_model` behind a provider
    /// of `provider_type`, best guess first.
    ///
    /// Used by the import-time probe and by the runtime relearn path.
    /// The ordering is a heuristic on the model id — never a decision
    /// on its own, always confirmed by an actual upstream response.
    ///
    /// Providers whose transport is fixed (Bedrock SigV4, Gemini) get a
    /// single candidate: there is no second dialect to fall back to on
    /// the same host, so probing them would just burn a request.
    pub fn candidates_for(provider_type: &str, upstream_model: &str) -> Vec<Self> {
        let default = Self::default_for_provider_type(provider_type);
        if matches!(default, Self::BedrockNative | Self::GoogleGenerate) {
            return vec![default];
        }

        let model = upstream_model.to_ascii_lowercase();
        // Match on the family segment, not the whole id: aggregators
        // prefix the vendor (`anthropic.claude-…`, `openai.gpt-…`)
        // while first-party endpoints don't (`claude-…`, `gpt-…`).
        let native = if model.contains("claude") || model.starts_with("anthropic") {
            Self::AnthropicMessages
        } else if model.contains("gemini") || model.starts_with("google") {
            Self::GoogleGenerate
        } else {
            Self::OpenAiChat
        };

        // The native guess first, then the OpenAI dialects — nearly
        // every aggregator speaks at least one of them, and Responses
        // is the one newer models are increasingly exposed on.
        let mut out = vec![native];
        for fallback in [Self::OpenAiChat, Self::OpenAiResponses, default] {
            if !out.contains(&fallback) {
                out.push(fallback);
            }
        }
        out
    }
}

impl fmt::Display for UpstreamProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_its_string_form() {
        for p in [
            UpstreamProtocol::OpenAiChat,
            UpstreamProtocol::OpenAiResponses,
            UpstreamProtocol::AnthropicMessages,
            UpstreamProtocol::GoogleGenerate,
            UpstreamProtocol::BedrockNative,
        ] {
            assert_eq!(UpstreamProtocol::parse(p.as_str()), Some(p));
        }
        assert_eq!(UpstreamProtocol::parse("nonsense"), None);
    }

    #[test]
    fn unprobed_route_keeps_the_provider_type_behaviour() {
        assert_eq!(
            UpstreamProtocol::default_for_provider_type("custom"),
            UpstreamProtocol::OpenAiChat
        );
        assert_eq!(
            UpstreamProtocol::default_for_provider_type("anthropic"),
            UpstreamProtocol::AnthropicMessages
        );
    }

    #[test]
    fn candidates_lead_with_the_model_family_then_cover_the_rest() {
        // The case this whole mechanism exists for: a `custom`
        // (OpenAI-compatible) aggregator serving Anthropic models that
        // only answer on /v1/messages.
        let c = UpstreamProtocol::candidates_for("custom", "anthropic.claude-opus-4");
        assert_eq!(c[0], UpstreamProtocol::AnthropicMessages);
        assert!(c.contains(&UpstreamProtocol::OpenAiChat));
        assert!(c.contains(&UpstreamProtocol::OpenAiResponses));

        let c = UpstreamProtocol::candidates_for("custom", "openai.gpt-oss-20b");
        assert_eq!(c[0], UpstreamProtocol::OpenAiChat);
        assert!(c.contains(&UpstreamProtocol::OpenAiResponses));
    }

    #[test]
    fn fixed_transport_providers_get_a_single_candidate() {
        // Nothing to fall back to on the same host — probing would
        // only burn a request.
        assert_eq!(
            UpstreamProtocol::candidates_for("bedrock", "anthropic.claude-opus-4"),
            vec![UpstreamProtocol::BedrockNative]
        );
        assert_eq!(
            UpstreamProtocol::candidates_for("google", "gemini-2.0-flash"),
            vec![UpstreamProtocol::GoogleGenerate]
        );
    }

    #[test]
    fn candidate_lists_never_repeat_a_protocol() {
        for (ty, model) in [
            ("custom", "claude-3-5-sonnet"),
            ("custom", "gpt-4o"),
            ("openai", "gpt-4o"),
            ("anthropic", "claude-3-5-sonnet"),
            ("azure_openai", "my-deployment"),
        ] {
            let c = UpstreamProtocol::candidates_for(ty, model);
            let mut seen = c.clone();
            seen.sort_by_key(|p| p.as_str());
            seen.dedup();
            assert_eq!(seen.len(), c.len(), "duplicate candidate for {ty}/{model}");
        }
    }
}
