//! Conversations whose history an upstream sealed.
//!
//! OpenAI's Responses API, asked not to store a conversation (`store:
//! false` — Codex always asks that), hands the client what the next turn
//! needs encrypted, and the client sends it back every turn: reasoning,
//! remote compactions, what Codex's agents said to each other, encrypted
//! tool output. The key is OpenAI's, per account. Sent to another provider
//! — another account, a relay — the whole request is refused
//! (`invalid_encrypted_content`), and since the client resends the whole
//! history, so is every turn after it. Which parts are sealed, and how an
//! upstream says it cannot read them, is `tw_dialect::sealed`, shared with
//! the desktop gateway.
//!
//! So each conversation — the authenticated user and the caller's session
//! id — remembers the provider that last answered it ([`Conversation`]),
//! for [`BINDING_TTL_SECS`], whatever the affinity settings say: this is
//! not a preference for a warm cache, it is the one provider that can read
//! the history. A request carrying sealed history is then routed by
//! [`decide`]:
//!
//! * only sealed reasoning ([`Sealed::Reasoning`]): the provider that holds
//!   it goes first; the others stay for failover — reasoning is the
//!   model's own notes, and the conversation goes on without it;
//! * anything else ([`Sealed::Content`]): only that provider. Another
//!   cannot read it, so there is no failover; one with no route for the
//!   model any more is refused up front, with what to do about it.
//!
//! A conversation with no session id, or none remembered, is routed as
//! any other request; an upstream that then cannot read the history says
//! so in plain words (see [`explain_refusal`]).

use fred::interfaces::KeysInterface;
use tw_dialect::ir::Dialect;
use tw_dialect::sealed::Sealed;
use uuid::Uuid;

use crate::error::GatewayError;

/// How long the provider that answered a conversation is remembered: 30
/// days. A conversation picked up again days later (Codex's `resume`) still
/// carries the history only that provider can read.
pub(crate) const BINDING_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// One conversation: an authenticated user's session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Conversation {
    key: String,
}

impl Conversation {
    /// The conversation a request belongs to. `None` without a user or a
    /// session id: nothing ties its turns together.
    pub(crate) fn of(user_id: Option<&str>, session_id: Option<&str>) -> Option<Self> {
        Some(Self {
            key: format!("sealed_holder:{}:{}", user_id?, session_id?),
        })
    }

    /// The provider that last answered this conversation, if remembered.
    /// A Redis that cannot be read remembers nothing: the request is routed
    /// as any other.
    pub(crate) async fn holder(&self, redis: &fred::clients::Client) -> Option<Holder> {
        let value: Option<String> = redis.get(&self.key).await.ok().flatten();
        Holder::parse(&value?)
    }

    /// `provider_id` (`provider_name`) answered this conversation.
    /// Best-effort.
    pub(crate) async fn answered_by(
        &self,
        redis: &fred::clients::Client,
        provider_id: Uuid,
        provider_name: &str,
    ) {
        let value = Holder {
            provider_id,
            provider_name: provider_name.to_string(),
        }
        .encode();
        if let Err(e) = redis
            .set::<(), _, _>(
                &self.key,
                value,
                Some(fred::types::Expiration::EX(BINDING_TTL_SECS)),
                None,
                false,
            )
            .await
        {
            tracing::warn!(error = %e, "could not remember which provider answered a conversation");
        }
    }
}

/// The provider that last answered a conversation. Its name is kept for
/// the refusal when it has no route left to name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Holder {
    pub provider_id: Uuid,
    pub provider_name: String,
}

impl Holder {
    /// `<provider id> <provider name>`: the id has no space in it.
    fn encode(&self) -> String {
        format!("{} {}", self.provider_id, self.provider_name)
    }

    fn parse(value: &str) -> Option<Self> {
        let (id, name) = value.split_once(' ')?;
        Some(Self {
            provider_id: Uuid::parse_str(id).ok()?,
            provider_name: name.to_string(),
        })
    }
}

/// Where a request carrying sealed history may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pin {
    /// Only this provider: no other can read the history. Its open breaker
    /// does not keep the request from it — there is nowhere else to go, and
    /// its own answer says more than "no route" would.
    Only(Uuid),
    /// This provider first; the others for failover.
    First(Uuid),
}

impl Pin {
    pub(crate) fn provider_id(self) -> Uuid {
        match self {
            Pin::Only(id) | Pin::First(id) => id,
        }
    }
}

/// Where a request goes, given what it carries sealed (`sealed`), the
/// provider that last answered its conversation (`holder`), and the
/// providers of the routes it may take (`providers`).
///
/// `Ok(None)`: routed as any other request — nothing sealed, or nobody
/// known to hold it. Sealed reasoning whose holder has no route left is
/// routed as any other too: the conversation goes on without it. Anything
/// else sealed by a provider with no route left is refused: no provider can
/// read it.
pub(crate) fn decide(
    sealed: Option<Sealed>,
    holder: Option<&Holder>,
    mut providers: impl Iterator<Item = Uuid>,
    model: &str,
) -> Result<Option<Pin>, GatewayError> {
    let (Some(sealed), Some(holder)) = (sealed, holder) else {
        return Ok(None);
    };
    let routed = providers.any(|p| p == holder.provider_id);
    match (sealed, routed) {
        (Sealed::Content, true) => Ok(Some(Pin::Only(holder.provider_id))),
        (Sealed::Content, false) => Err(GatewayError::SealedHistoryUnavailable {
            provider: holder.provider_name.clone(),
            model: model.to_string(),
        }),
        (Sealed::Reasoning, true) => Ok(Some(Pin::First(holder.provider_id))),
        (Sealed::Reasoning, false) => Ok(None),
    }
}

/// An upstream's refusal of sealed history, said plainly: the upstream's
/// own words ("Encrypted function output content could not be decrypted")
/// do not tell anyone that the conversation came from another provider.
///
/// `wire` is the format the request went out in. Only Responses is read,
/// as on the desktop: that is where the history is sealed per account.
/// `label` is the prefix the transport puts before the upstream's body
/// (see `transport::check_status`). Any other error is returned as it is.
pub(crate) fn explain_refusal(
    wire: Dialect,
    provider: &str,
    label: &str,
    e: GatewayError,
) -> GatewayError {
    match e {
        GatewayError::ProviderHttpError {
            status, message, ..
        } if (400..500).contains(&status) && refused(wire, &message) => {
            let said = message
                .strip_prefix(label)
                .and_then(|m| m.strip_prefix(": "))
                .unwrap_or(&message)
                .to_string();
            GatewayError::SealedHistoryRefused {
                status,
                provider: provider.to_string(),
                said,
            }
        }
        e => e,
    }
}

/// Whether `said`, an error an upstream sent in `wire`, refuses sealed
/// history.
pub(crate) fn refused(wire: Dialect, said: &str) -> bool {
    wire == Dialect::Responses && tw_dialect::sealed::refused(wire, said.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn holder(id: Uuid) -> Holder {
        Holder {
            provider_id: id,
            provider_name: "openai-team a".into(),
        }
    }

    #[test]
    fn a_conversation_needs_a_user_and_a_session() {
        assert!(Conversation::of(Some("u"), Some("s")).is_some());
        assert!(Conversation::of(None, Some("s")).is_none());
        assert!(Conversation::of(Some("u"), None).is_none());
        assert_ne!(
            Conversation::of(Some("u1"), Some("s")),
            Conversation::of(Some("u2"), Some("s")),
            "two users' sessions are two conversations"
        );
    }

    #[test]
    fn the_holder_round_trips_with_a_name_that_has_spaces() {
        let h = holder(Uuid::new_v4());
        assert_eq!(Holder::parse(&h.encode()), Some(h));
        assert_eq!(Holder::parse("not-a-uuid name"), None);
        assert_eq!(Holder::parse(""), None);
    }

    #[test]
    fn content_only_another_provider_cannot_read_goes_only_to_its_holder() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let h = holder(a);
        assert_eq!(
            decide(Some(Sealed::Content), Some(&h), [b, a].into_iter(), "m").unwrap(),
            Some(Pin::Only(a))
        );
        // The holder has no route left: nobody can read it.
        match decide(Some(Sealed::Content), Some(&h), [b].into_iter(), "gpt-5") {
            Err(e @ GatewayError::SealedHistoryUnavailable { .. }) => {
                assert_eq!(e.status_code(), 409);
                let said = e.to_string();
                assert!(
                    said.contains("`openai-team a`") && said.contains("`gpt-5`"),
                    "{said}"
                );
                assert!(said.contains("Start a new conversation"), "{said}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sealed_reasoning_prefers_its_holder_and_can_do_without_it() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let h = holder(a);
        assert_eq!(
            decide(Some(Sealed::Reasoning), Some(&h), [b, a].into_iter(), "m").unwrap(),
            Some(Pin::First(a))
        );
        assert_eq!(
            decide(Some(Sealed::Reasoning), Some(&h), [b].into_iter(), "m").unwrap(),
            None
        );
    }

    #[test]
    fn nothing_sealed_or_nobody_known_routes_as_usual() {
        let a = Uuid::new_v4();
        assert_eq!(
            decide(None, Some(&holder(a)), [a].into_iter(), "m").unwrap(),
            None
        );
        assert_eq!(
            decide(Some(Sealed::Content), None, [a].into_iter(), "m").unwrap(),
            None
        );
    }

    #[test]
    fn a_refusal_of_sealed_history_is_said_plainly() {
        let body = json!({"error": {
            "message": "The encrypted content for item rs_68 could not be verified.",
            "type": "invalid_request_error", "code": "invalid_encrypted_content"}})
        .to_string();
        let e = GatewayError::ProviderHttpError {
            status: 400,
            message: format!("relay-b: {body}"),
            retry_after: None,
        };
        let e = explain_refusal(Dialect::Responses, "relay-b", "relay-b", e);
        let GatewayError::SealedHistoryRefused {
            status,
            ref provider,
            ref said,
        } = e
        else {
            panic!("{e:?}");
        };
        assert_eq!((status, provider.as_str()), (400, "relay-b"));
        assert_eq!(said, &body, "the upstream's own words, without the label");
        assert_eq!(e.status_code(), 400);
        assert_eq!(e.error_tag(), "SealedHistoryRefused");
        let told = e.to_string();
        assert!(told.starts_with("Provider `relay-b` cannot read"), "{told}");
        assert!(told.contains("another provider encrypted it"), "{told}");
        assert!(told.contains("could not be verified"), "{told}");
        assert!(
            !crate::proxy::upstream_failed(e.error_tag(), e.status_code()),
            "a refusal, not the upstream failing"
        );
    }

    #[test]
    fn other_errors_and_other_formats_are_left_as_they_are() {
        let other = || GatewayError::ProviderHttpError {
            status: 400,
            message: "p: max_output_tokens is too large".into(),
            retry_after: None,
        };
        assert!(matches!(
            explain_refusal(Dialect::Responses, "p", "p", other()),
            GatewayError::ProviderHttpError { .. }
        ));
        let sealed = || GatewayError::ProviderHttpError {
            status: 400,
            message: "p: invalid_encrypted_content".into(),
            retry_after: None,
        };
        // Not sent as Responses: nothing of OpenAI's was sealed in it.
        assert!(matches!(
            explain_refusal(Dialect::Chat, "p", "p", sealed()),
            GatewayError::ProviderHttpError { .. }
        ));
        // A server error is the upstream failing, whatever it says.
        let e = GatewayError::ProviderHttpError {
            status: 500,
            message: "p: invalid_encrypted_content".into(),
            retry_after: None,
        };
        assert!(matches!(
            explain_refusal(Dialect::Responses, "p", "p", e),
            GatewayError::ProviderHttpError { .. }
        ));
    }
}
