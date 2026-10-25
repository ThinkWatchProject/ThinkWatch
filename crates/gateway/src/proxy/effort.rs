//! Reasoning effort: the model's setting, and what an upstream refuses.
//!
//! # The setting (`models.reasoning_effort`)
//!
//! `off` takes reasoning out of a request that has it on: the switch that
//! turns it on and the effort with it. A level (`low` … `max`) changes the
//! effort of a request **whose reasoning is on**; a request without it —
//! a title, a summary, a client that never asked — goes out untouched:
//! reasoning is never turned on for a caller. Unset: requests go out as
//! sent. It applies to the caller's request in its own format, once,
//! before the request is addressed to any route: a request converted for
//! another format carries the changed effort the way that format writes
//! it. What reasoning is on, and where each format writes the effort, is
//! `tw_dialect::params`, shared with the desktop gateway.
//!
//! # What an upstream refuses
//!
//! Not every model takes every level, and an upstream that does not says
//! so with a 400 (`tw_dialect::think::effort_rejection` reads it). The
//! request then goes to the **same upstream** once more, with the level it
//! named or, when it named none, without the effort (the model's default).
//! That is not a retry of a failure of the moment: it does not wait and
//! does not count against `gateway.retry_wait_secs` (`super::retry`). A
//! second refusal is the answer.
//!
//! A resend that is answered is remembered in Redis, per provider and the
//! model named upstream, for [`MEMORY_TTL_SECS`]: later requests that
//! would send the refused level send the one that worked straight away.
//! After that the refused level is tried again — a model can gain one.
//! A provider's entries go when its URL or credential changes, a route of
//! it relearns its protocol, or it is deleted ([`forget_provider`]).
//!
//! What a hop sent differently from what the caller asked is said on the
//! request's `gateway_logs` row, as `"effort"` ([`EffortChange::detail`]).

use std::collections::BTreeMap;

use fred::interfaces::{HashesInterface, KeysInterface};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tw_dialect::ir::{Dialect, Effort};
use tw_dialect::think::EffortFix;
use uuid::Uuid;

use crate::error::GatewayError;

/// A model's reasoning setting (`models.reasoning_effort`). Unset is no
/// setting at all: `Option<ReasoningSetting>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSetting {
    /// Take reasoning out of requests that have it on.
    Off,
    /// Change the effort of requests whose reasoning is on.
    Level(Effort),
}

/// The values `models.reasoning_effort` may hold, in display order.
pub const SETTINGS: [&str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];

impl ReasoningSetting {
    /// One of [`SETTINGS`]. `minimal` is not one: few models take it, and
    /// the ones that do not refuse it outright.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => Self::Off,
            "low" => Self::Level(Effort::Low),
            "medium" => Self::Level(Effort::Medium),
            "high" => Self::Level(Effort::High),
            "xhigh" => Self::Level(Effort::XHigh),
            "max" => Self::Level(Effort::Max),
            _ => return None,
        })
    }

    /// Apply to `body`, a request in `dialect`. What changed, if anything.
    pub(crate) fn apply(self, dialect: Dialect, body: &mut Value) -> Option<EffortChange> {
        if !tw_dialect::params::reasoning_on(dialect, body) {
            return None;
        }
        let asked = tw_dialect::params::reasoning_effort(dialect, body);
        let sent = match self {
            Self::Off => {
                tw_dialect::params::disable_thinking(dialect, body);
                Sent::Off
            }
            Self::Level(e) => {
                if !tw_dialect::params::set_reasoning_effort(dialect, body, e) {
                    return None;
                }
                Sent::of(tw_dialect::params::reasoning_effort(dialect, body))
            }
        };
        // A budget rewritten within the same level says nothing new.
        (sent != Sent::of(asked)).then_some(EffortChange {
            asked,
            sent,
            reason: Reason::Setting,
        })
    }
}

/// A request's reasoning, under the model's setting: what the caller
/// asked, and what the setting changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// The effort the caller asked for, before the setting: what a hop
    /// that sends another is said against.
    pub asked: Option<Effort>,
    /// Reasoning is on, after the setting: only then is there an effort
    /// an upstream can refuse.
    pub on: bool,
    /// What the setting changed, if anything.
    pub change: Option<EffortChange>,
}

impl Applied {
    /// Apply `setting` to `body`, the caller's request in `dialect`.
    pub(crate) fn apply(
        setting: Option<ReasoningSetting>,
        dialect: Dialect,
        body: &mut Value,
    ) -> Self {
        let asked = tw_dialect::params::reasoning_effort(dialect, body);
        let change = setting.and_then(|s| s.apply(dialect, body));
        Self {
            asked,
            on: tw_dialect::params::reasoning_on(dialect, body),
            change,
        }
    }
}

/// What a hop sent for reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sent {
    Level(Effort),
    /// Reasoning on, the effort left out: the model's default.
    Default,
    /// Reasoning taken out.
    Off,
}

impl Sent {
    /// A level, or the model's default.
    pub(crate) fn of(effort: Option<Effort>) -> Self {
        effort.map_or(Self::Default, Self::Level)
    }
}

/// Why a hop sent something else than the caller asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    /// The model's setting.
    Setting,
    /// The upstream refused what went out first, and the request went
    /// out again.
    Rejected,
    /// The upstream refused that level before ([`Memory`]).
    Remembered,
}

/// A hop that sent a different reasoning effort than the caller asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortChange {
    /// `None`: reasoning on, no effort named.
    pub asked: Option<Effort>,
    pub sent: Sent,
    pub reason: Reason,
}

impl EffortChange {
    /// As said on the log row: `{"asked": "xhigh", "sent": "max",
    /// "reason": "rejected"}`. `asked` is `null` for a request that named
    /// no effort; `sent` is `null` when the effort was left out and `"off"`
    /// when reasoning was taken out.
    pub(crate) fn detail(&self) -> Value {
        serde_json::json!({
            "asked": self.asked.map(slug),
            "sent": match self.sent {
                Sent::Level(e) => Value::from(slug(e)),
                Sent::Default => Value::Null,
                Sent::Off => Value::from("off"),
            },
            "reason": match self.reason {
                Reason::Setting => "setting",
                Reason::Rejected => "rejected",
                Reason::Remembered => "remembered",
            },
        })
    }
}

/// `minimal` … `max`, as OpenAI writes them.
fn slug(e: Effort) -> &'static str {
    tw_dialect::think::openai(e)
}

fn parse_slug(s: &str) -> Option<Effort> {
    tw_dialect::think::parse_openai(s).flatten()
}

/// The effort `body`, in `dialect`, sends: `None` when it names none or
/// reasoning is off.
pub(crate) fn sent_effort(dialect: Dialect, body: &[u8]) -> Option<Effort> {
    let v: Value = serde_json::from_slice(body).ok()?;
    tw_dialect::params::reasoning_effort(dialect, &v)
}

/// Write `to` into `body` (`None`: leave the effort out). False when
/// nothing changed — `body` is then as it was.
pub(crate) fn rewrite(dialect: Dialect, body: &mut Vec<u8>, to: Option<Effort>) -> bool {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let changed = match to {
        Some(e) => tw_dialect::params::set_reasoning_effort(dialect, &mut v, e),
        None => tw_dialect::params::remove_reasoning_effort(dialect, &mut v),
    };
    if !changed {
        return false;
    }
    match serde_json::to_vec(&v) {
        Ok(bytes) => {
            *body = bytes;
            true
        }
        Err(_) => false,
    }
}

/// Did the upstream refuse the effort `sent`? How to send it again.
/// `label` is the upstream's name, which the error's message starts with
/// and which is not what it said.
pub(crate) fn rejection(err: &GatewayError, label: &str, sent: Effort) -> Option<EffortFix> {
    let GatewayError::ProviderHttpError {
        status, message, ..
    } = err
    else {
        return None;
    };
    let said = message
        .strip_prefix(label)
        .and_then(|m| m.strip_prefix(": "))
        .unwrap_or(message);
    tw_dialect::think::effort_rejection(*status, said, sent)
}

// ───────────────────────────────────────────── memory

/// How long a refused level is remembered: 7 days.
pub const MEMORY_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// One provider's memory: a hash, a field per model named upstream.
/// Hash-tagged on the provider, as the route keys are on the route.
fn key(provider_id: Uuid) -> String {
    format!("effort_memory:{{provider:{provider_id}}}")
}

/// What one model refused: refused level → what worked instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Memory(BTreeMap<String, Entry>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    /// The level that was answered; `None`: the effort left out.
    #[serde(rename = "use")]
    instead: Option<String>,
    /// Unix seconds.
    at: i64,
}

impl Memory {
    fn parse(value: &str) -> Self {
        serde_json::from_str(value).unwrap_or_default()
    }

    /// What to send instead of `sent`, as of `now`: `Some(None)` is to
    /// leave the effort out. Entries older than [`MEMORY_TTL_SECS`] are no
    /// longer taken. A level that was itself refused since is followed to
    /// what worked for it.
    pub(crate) fn instead_of(&self, sent: Effort, now: i64) -> Option<Option<Effort>> {
        let mut found = None;
        let mut at = sent;
        for _ in 0..SETTINGS.len() {
            match self.one(at, now) {
                None => break,
                Some(None) => return Some(None),
                Some(Some(e)) => {
                    found = Some(Some(e));
                    if e == at {
                        break;
                    }
                    at = e;
                }
            }
        }
        found
    }

    fn one(&self, sent: Effort, now: i64) -> Option<Option<Effort>> {
        let entry = self.0.get(slug(sent))?;
        if now - entry.at >= MEMORY_TTL_SECS {
            return None;
        }
        Some(match &entry.instead {
            Some(s) => Some(parse_slug(s)?),
            None => None,
        })
    }

    /// `rejected` was refused and `instead` answered, at `now`. Expired
    /// entries go.
    pub(crate) fn note(&mut self, rejected: Effort, instead: Option<Effort>, now: i64) {
        self.0.retain(|_, e| now - e.at < MEMORY_TTL_SECS);
        self.0.insert(
            slug(rejected).to_string(),
            Entry {
                instead: instead.map(|e| slug(e).to_string()),
                at: now,
            },
        );
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// What `provider_id` refused for `model`. A Redis that cannot be read
/// remembers nothing: the request goes out as it would have.
pub(crate) async fn recall(
    redis: &fred::clients::Client,
    provider_id: Uuid,
    model: &str,
) -> Memory {
    match redis
        .hget::<Option<String>, _, _>(key(provider_id), model)
        .await
    {
        Ok(Some(v)) => Memory::parse(&v),
        Ok(None) => Memory::default(),
        Err(e) => {
            tracing::debug!(error = %e, "could not read the reasoning effort memory");
            Memory::default()
        }
    }
}

/// `provider_id` refused `rejected` for `model`, and answered `instead`.
/// Best-effort.
pub(crate) async fn remember(
    redis: &fred::clients::Client,
    provider_id: Uuid,
    model: &str,
    rejected: Effort,
    instead: Option<Effort>,
) {
    let mut memory = recall(redis, provider_id, model).await;
    memory.note(rejected, instead, now());
    if memory.is_empty() {
        return;
    }
    let Ok(value) = serde_json::to_string(&memory) else {
        return;
    };
    let key = key(provider_id);
    let written = async {
        redis.hset::<(), _, _>(&key, (model, value)).await?;
        redis.expire::<(), _>(&key, MEMORY_TTL_SECS, None).await
    }
    .await;
    if let Err(e) = written {
        tracing::warn!(error = %e, "could not remember a refused reasoning effort");
    }
}

/// Forget what `provider_id` refused: it may be another upstream now.
/// Best-effort.
pub async fn forget_provider(redis: &fred::clients::Client, provider_id: Uuid) {
    if let Err(e) = redis.del::<(), _>(key(provider_id)).await {
        tracing::warn!(error = %e, %provider_id, "could not clear the reasoning effort memory");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_settings_are_off_and_five_levels() {
        for s in SETTINGS {
            assert!(ReasoningSetting::parse(s).is_some(), "{s}");
        }
        assert_eq!(ReasoningSetting::parse("off"), Some(ReasoningSetting::Off));
        assert_eq!(
            ReasoningSetting::parse("xhigh"),
            Some(ReasoningSetting::Level(Effort::XHigh))
        );
        for bad in ["minimal", "none", "on", "", "MAX"] {
            assert_eq!(ReasoningSetting::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_level_changes_only_a_request_with_reasoning_on() {
        let max = ReasoningSetting::Level(Effort::Max);
        let mut body = json!({"model": "m", "reasoning_effort": "xhigh", "messages": []});
        let change = max.apply(Dialect::Chat, &mut body).unwrap();
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(
            change.detail(),
            json!({"asked": "xhigh", "sent": "max", "reason": "setting"})
        );

        // No reasoning: a title request goes out as sent.
        let mut title = json!({"model": "m", "messages": []});
        assert_eq!(max.apply(Dialect::Chat, &mut title), None);
        assert_eq!(title, json!({"model": "m", "messages": []}));

        // Reasoning turned off by the caller stays off.
        let mut none = json!({"model": "m", "reasoning_effort": "none"});
        assert_eq!(max.apply(Dialect::Chat, &mut none), None);
        assert_eq!(none["reasoning_effort"], "none");

        // Already at that level: nothing to say.
        let mut same = json!({"model": "m", "reasoning": {"effort": "max"}, "input": "hi"});
        assert_eq!(max.apply(Dialect::Responses, &mut same), None);
    }

    #[test]
    fn a_level_is_written_where_the_callers_format_puts_it() {
        let high = ReasoningSetting::Level(Effort::High);
        // Anthropic adaptive thinking, no effort named: one is added.
        let mut adaptive = json!({"model": "m", "max_tokens": 64000,
                                  "thinking": {"type": "adaptive"}, "messages": []});
        let change = high.apply(Dialect::Anthropic, &mut adaptive).unwrap();
        assert_eq!(adaptive["output_config"]["effort"], "high");
        assert_eq!(change.asked, None);
        assert_eq!(change.sent, Sent::Level(Effort::High));

        let mut responses = json!({"model": "m", "reasoning": {"effort": "low"}, "input": "hi"});
        high.apply(Dialect::Responses, &mut responses).unwrap();
        assert_eq!(responses["reasoning"]["effort"], "high");
    }

    #[test]
    fn off_takes_reasoning_and_its_effort_out() {
        let mut claude = json!({"model": "m", "max_tokens": 64000,
                                "thinking": {"type": "adaptive"},
                                "output_config": {"effort": "max"}, "messages": []});
        let change = ReasoningSetting::Off
            .apply(Dialect::Anthropic, &mut claude)
            .unwrap();
        assert!(claude.get("thinking").is_none(), "{claude}");
        assert!(claude.get("output_config").is_none(), "{claude}");
        assert_eq!(
            change.detail(),
            json!({"asked": "max", "sent": "off", "reason": "setting"})
        );

        let mut chat = json!({"model": "m", "reasoning_effort": "high"});
        ReasoningSetting::Off
            .apply(Dialect::Chat, &mut chat)
            .unwrap();
        assert!(chat.get("reasoning_effort").is_none());

        // Nothing to take out.
        let mut plain = json!({"model": "m", "messages": []});
        assert_eq!(ReasoningSetting::Off.apply(Dialect::Chat, &mut plain), None);
    }

    #[test]
    fn a_hops_body_is_rewritten_in_its_own_format() {
        let mut body =
            serde_json::to_vec(&json!({"model": "m", "reasoning_effort": "xhigh"})).unwrap();
        assert_eq!(sent_effort(Dialect::Chat, &body), Some(Effort::XHigh));
        assert!(rewrite(Dialect::Chat, &mut body, Some(Effort::High)));
        assert_eq!(sent_effort(Dialect::Chat, &body), Some(Effort::High));
        assert!(rewrite(Dialect::Chat, &mut body, None));
        assert_eq!(sent_effort(Dialect::Chat, &body), None);
        let before = body.clone();
        assert!(!rewrite(Dialect::Chat, &mut body, None));
        assert_eq!(body, before);
    }

    fn http(status: u16, said: &str) -> GatewayError {
        GatewayError::ProviderHttpError {
            status,
            message: format!("relay: {said}"),
            retry_after: None,
        }
    }

    #[test]
    fn a_refusal_of_the_effort_is_read_off_the_upstreams_words() {
        let said = r#"{"error":{"message":"Unsupported value: 'xhigh' is not supported with this model. Supported values are: 'low', 'medium', and 'high'.","param":"reasoning.effort"}}"#;
        assert_eq!(
            rejection(&http(400, said), "relay", Effort::XHigh),
            Some(EffortFix::Use(Effort::High))
        );
        // Not a 400: an upstream failing is not a refusal.
        assert_eq!(rejection(&http(500, said), "relay", Effort::XHigh), None);
        // A refusal of something else.
        assert_eq!(
            rejection(
                &http(
                    400,
                    r#"{"message":"messages: at least one message is required"}"#
                ),
                "relay",
                Effort::XHigh
            ),
            None
        );
        // The upstream's own name is not what it said.
        assert_eq!(
            rejection(
                &GatewayError::ProviderHttpError {
                    status: 400,
                    message: "high effort relay: messages: Field required".into(),
                    retry_after: None,
                },
                "high effort relay",
                Effort::High
            ),
            None
        );
        assert_eq!(
            rejection(
                &GatewayError::UpstreamAuthError {
                    status: 403,
                    message: "effort".into()
                },
                "relay",
                Effort::High
            ),
            None
        );
    }

    #[test]
    fn the_memory_keeps_each_refused_level_for_a_week() {
        let t0 = 1_800_000_000;
        let mut m = Memory::default();
        m.note(Effort::XHigh, Some(Effort::High), t0);
        m.note(Effort::Max, None, t0 + 10);
        assert_eq!(
            m.instead_of(Effort::XHigh, t0 + 60),
            Some(Some(Effort::High))
        );
        assert_eq!(m.instead_of(Effort::Max, t0 + 60), Some(None));
        assert_eq!(m.instead_of(Effort::High, t0 + 60), None);
        // A week on, the refused level is tried again.
        assert_eq!(m.instead_of(Effort::XHigh, t0 + MEMORY_TTL_SECS), None);
        assert_eq!(
            m.instead_of(Effort::Max, t0 + MEMORY_TTL_SECS),
            Some(None),
            "noted ten seconds later"
        );

        // As stored, and read back.
        let stored = serde_json::to_string(&m).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored).unwrap(),
            json!({"xhigh": {"use": "high", "at": t0}, "max": {"use": null, "at": t0 + 10}})
        );
        assert_eq!(Memory::parse(&stored), m);
        // Anything else reads as nothing remembered.
        assert_eq!(Memory::parse("not json"), Memory::default());

        // xhigh worked out as high, and high was refused since: medium.
        m.note(Effort::High, Some(Effort::Medium), t0 + 20);
        assert_eq!(
            m.instead_of(Effort::XHigh, t0 + 60),
            Some(Some(Effort::Medium))
        );

        // Noting drops what expired.
        m.note(Effort::Low, Some(Effort::Medium), t0 + MEMORY_TTL_SECS + 5);
        assert_eq!(m.0.len(), 3, "{m:?}");
        assert!(!m.0.contains_key("xhigh"));
    }

    #[test]
    fn the_memory_key_is_tagged_on_the_provider() {
        let id = Uuid::nil();
        assert_eq!(
            key(id),
            "effort_memory:{provider:00000000-0000-0000-0000-000000000000}"
        );
    }
}
