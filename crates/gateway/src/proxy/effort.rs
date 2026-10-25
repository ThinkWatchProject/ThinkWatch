//! Reasoning effort: the model's setting, and what an upstream refuses.
//!
//! # The setting (`models.reasoning_effort`)
//!
//! `off` turns reasoning off in a request that has it on (OpenAI's formats
//! write `none`; the others take the switch out, with the effort). A level
//! (`low` … `max`) changes the effort of a request **whose reasoning is
//! on**; a request without it — a title, a summary, a client that never
//! asked — goes out untouched: reasoning is never turned on for a caller.
//! Unset: requests go out as sent. It applies to each hop as it is
//! addressed, to the caller's request in its own format and for the model
//! named upstream (Gemini's budgets are capped per model); a request
//! converted for another format carries the changed effort the way that
//! format writes it. What reasoning is on, and where each format writes the
//! effort, is `tw_dialect::params`, shared with the desktop gateway.
//!
//! # What an upstream refuses
//!
//! Not every model takes every level, and an upstream that does not says
//! so with a 400 (`tw_dialect::think::effort_rejection`; for a `none` it
//! does not take, `none_rejection`). Both are read against the effort the
//! hop **wrote** (`tw_dialect::params::written_effort`), whether or not
//! reasoning is on — Claude with thinking off still refuses `xhigh` — and a
//! request that only carries a budget has none to refuse. The request then
//! goes to the **same upstream** once more, with the level it named or,
//! when it named none, without the effort (the model's default). That is
//! not a retry of a failure of the moment: it does not wait and does not
//! count against `gateway.retry_wait_secs` (`super::retry`). A second
//! refusal is the answer.
//!
//! A resend that is answered is remembered in Redis, per provider, format
//! sent and model named upstream, for [`MEMORY_TTL_SECS`]: later requests
//! that would write the refused level write the one that worked straight
//! away. A remembered fix that is refused itself is forgotten (and replaced
//! if the resend that follows is answered); so is one whose resend was
//! answered at the level first refused. After the week the refused level
//! is tried again — a model can gain one. A provider's entries go when its
//! URL or credential changes, a route of it relearns its protocol, or it
//! is deleted ([`forget_provider`]).
//!
//! What a hop sent differently from what it would have is said on the
//! request's `gateway_logs` row, as `"effort"` ([`EffortChange::detail`]).

use std::collections::BTreeMap;

use fred::interfaces::{HashesInterface, KeysInterface};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tw_dialect::ir::{Dialect, Effort};
use tw_dialect::params::WrittenEffort;
use tw_dialect::think::EffortFix;
use uuid::Uuid;

use crate::error::GatewayError;

/// A model's reasoning setting (`models.reasoning_effort`). Unset is no
/// setting at all: `Option<ReasoningSetting>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSetting {
    /// Turn reasoning off in requests that have it on.
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

    /// As stored: one of [`SETTINGS`].
    pub fn slug(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Level(e) => tw_dialect::think::openai(e),
        }
    }

    /// Apply to `body`, a request in `dialect` for `model` (the name sent
    /// upstream). Whether anything changed.
    pub(crate) fn apply(self, dialect: Dialect, body: &mut Value, model: &str) -> bool {
        match self {
            Self::Off => {
                if !tw_dialect::params::reasoning_on(dialect, body) {
                    return false;
                }
                let before = body.clone();
                tw_dialect::params::disable_thinking(dialect, body);
                *body != before
            }
            Self::Level(e) => tw_dialect::params::set_reasoning_effort(dialect, body, e, model),
        }
    }
}

/// What the caller's request asks for reasoning, read once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Asked {
    /// The model's setting, applied to each hop.
    pub setting: Option<ReasoningSetting>,
    /// The effort the caller asked for, with reasoning on: what a hop the
    /// setting changed is said against.
    pub effort: Option<Effort>,
    /// A hop can carry an effort an upstream could refuse: the caller has
    /// reasoning on or writes one. Otherwise no hop asks the memory.
    pub any: bool,
}

impl Asked {
    pub(crate) fn of(setting: Option<ReasoningSetting>, dialect: Dialect, body: &Value) -> Self {
        Self {
            setting,
            effort: tw_dialect::params::reasoning_effort(dialect, body),
            any: tw_dialect::params::reasoning_on(dialect, body)
                || tw_dialect::params::written_effort(dialect, body).is_some(),
        }
    }

    /// What applying the setting to a hop changed, said against what the
    /// caller asked: `after` is the hop's body in `dialect` once applied.
    /// Nothing to say when the caller named no effort or the hop sends the
    /// same.
    pub(crate) fn change(&self, dialect: Dialect, after: &Value) -> Option<EffortChange> {
        let asked = slug(self.effort?);
        let sent = shown(dialect, after);
        (sent != Some(asked)).then_some(EffortChange {
            asked,
            sent,
            reason: Reason::Setting,
        })
    }
}

/// Why a hop sent another effort.
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

/// A hop that sent another reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffortChange {
    /// What the hop would have sent before this change: for the setting,
    /// what the caller asked; for a refusal or the memory, the level it
    /// replaced. `none` is OpenAI's reasoning turned off.
    pub asked: &'static str,
    /// What it sent; `None`: the effort left out.
    pub sent: Option<&'static str>,
    pub reason: Reason,
}

impl EffortChange {
    /// As said on the log row: `{"asked": "xhigh", "sent": "high",
    /// "reason": "rejected"}`, `sent` `null` when the effort was left out.
    pub(crate) fn detail(&self) -> Value {
        serde_json::json!({
            "asked": self.asked,
            "sent": self.sent,
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

/// The effort `v`, in `dialect`, sends: the field written, else the one
/// its reasoning implies (a budget read as a level).
pub(crate) fn shown(dialect: Dialect, v: &Value) -> Option<&'static str> {
    tw_dialect::params::written_effort(dialect, v)
        .map(WrittenEffort::slug)
        .or_else(|| tw_dialect::params::reasoning_effort(dialect, v).map(slug))
}

/// The effort written in `body`, a hop in `dialect`.
pub(crate) fn written(dialect: Dialect, body: &[u8]) -> Option<WrittenEffort> {
    let v: Value = serde_json::from_slice(body).ok()?;
    tw_dialect::params::written_effort(dialect, &v)
}

/// Rewrite the effort written in `body` as `fix` says. What the hop sends
/// then ([`shown`]) when anything changed; `None` leaves `body` as it was.
pub(crate) fn apply_fix(
    dialect: Dialect,
    body: &mut Vec<u8>,
    fix: EffortFix,
) -> Option<Option<&'static str>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let changed = match fix {
        EffortFix::Use(e) => tw_dialect::params::replace_reasoning_effort(dialect, &mut v, e),
        EffortFix::Drop => tw_dialect::params::remove_reasoning_effort(dialect, &mut v),
    };
    if !changed {
        return None;
    }
    let bytes = serde_json::to_vec(&v).ok()?;
    *body = bytes;
    Some(shown(dialect, &v))
}

/// Did the upstream refuse the effort `sent`? How to send it again.
/// `label` is the upstream's name, which the error's message starts with
/// and which is not what it said.
pub(crate) fn rejection(err: &GatewayError, label: &str, sent: WrittenEffort) -> Option<EffortFix> {
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
    match sent {
        WrittenEffort::Level(e) => tw_dialect::think::effort_rejection(*status, said, e),
        WrittenEffort::Off => {
            tw_dialect::think::none_rejection(*status, said).then_some(EffortFix::Drop)
        }
    }
}

// ───────────────────────────────────────────── memory

/// How long a refused level is remembered: 7 days.
pub const MEMORY_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// One provider's memory: a hash, a field per format sent and model named
/// upstream ([`field`]). Hash-tagged on the provider, as the route keys
/// are on the route.
fn key(provider_id: Uuid) -> String {
    format!("effort_memory:{{provider:{provider_id}}}")
}

/// `openai-chat:gpt-5`: the format first, which has no `:` in it.
fn field(dialect: Dialect, model: &str) -> String {
    format!("{}:{model}", dialect.slug())
}

/// What one model, asked in one format, refused: the written effort refused
/// (`none` too) → what worked instead.
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

    /// How to rewrite a hop that writes `written`, as of `now`. Entries
    /// older than [`MEMORY_TTL_SECS`] are no longer taken.
    pub(crate) fn instead_of(&self, written: WrittenEffort, now: i64) -> Option<EffortFix> {
        let entry = self.0.get(written.slug())?;
        if now - entry.at >= MEMORY_TTL_SECS {
            return None;
        }
        Some(match &entry.instead {
            Some(s) => EffortFix::Use(tw_dialect::think::parse_openai(s).flatten()?),
            None => EffortFix::Drop,
        })
    }

    /// `rejected` was refused and the resend `fix` answered, at `now`. A
    /// fix that is `rejected` itself (the upstream took it after all) is
    /// forgotten instead. Expired entries go.
    pub(crate) fn note(&mut self, rejected: WrittenEffort, fix: EffortFix, now: i64) {
        self.0.retain(|_, e| now - e.at < MEMORY_TTL_SECS);
        let instead = match fix {
            EffortFix::Use(e) if WrittenEffort::Level(e) == rejected => {
                self.forget(rejected);
                return;
            }
            EffortFix::Use(e) => Some(slug(e).to_string()),
            EffortFix::Drop => None,
        };
        self.0
            .insert(rejected.slug().to_string(), Entry { instead, at: now });
    }

    /// The fix remembered for `rejected` was refused itself.
    pub(crate) fn forget(&mut self, rejected: WrittenEffort) {
        self.0.remove(rejected.slug());
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub(crate) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// What `provider_id` refused for `model` asked in `dialect`. A Redis that
/// cannot be read remembers nothing: the request goes out as it would have.
pub(crate) async fn recall(
    redis: &fred::clients::Client,
    provider_id: Uuid,
    dialect: Dialect,
    model: &str,
) -> Memory {
    match redis
        .hget::<Option<String>, _, _>(key(provider_id), field(dialect, model))
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

/// Change what `provider_id` remembers for `model` in `dialect`, and write
/// it back: emptied, the field goes. Best-effort.
async fn update(
    redis: &fred::clients::Client,
    provider_id: Uuid,
    dialect: Dialect,
    model: &str,
    change: impl FnOnce(&mut Memory),
) {
    let mut memory = recall(redis, provider_id, dialect, model).await;
    let before = memory.clone();
    change(&mut memory);
    if memory == before {
        return;
    }
    let (key, field) = (key(provider_id), field(dialect, model));
    let written = async {
        if memory.is_empty() {
            return redis.hdel::<(), _, _>(&key, &field).await;
        }
        let value = serde_json::to_string(&memory).unwrap_or_default();
        redis
            .hset::<(), _, _>(&key, (field.as_str(), value))
            .await?;
        redis.expire::<(), _>(&key, MEMORY_TTL_SECS, None).await
    }
    .await;
    if let Err(e) = written {
        tracing::warn!(error = %e, "could not write the reasoning effort memory");
    }
}

/// `provider_id` refused `rejected` for `model` in `dialect`, and the
/// resend `fix` was answered.
pub(crate) async fn remember(
    redis: &fred::clients::Client,
    provider_id: Uuid,
    dialect: Dialect,
    model: &str,
    rejected: WrittenEffort,
    fix: EffortFix,
) {
    update(redis, provider_id, dialect, model, |m| {
        m.note(rejected, fix, now())
    })
    .await;
}

/// The fix remembered for `rejected` was refused itself.
pub(crate) async fn forget(
    redis: &fred::clients::Client,
    provider_id: Uuid,
    dialect: Dialect,
    model: &str,
    rejected: WrittenEffort,
) {
    update(redis, provider_id, dialect, model, |m| m.forget(rejected)).await;
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

    const L: fn(Effort) -> WrittenEffort = WrittenEffort::Level;

    #[test]
    fn the_settings_are_off_and_five_levels() {
        for s in SETTINGS {
            let setting = ReasoningSetting::parse(s).unwrap();
            assert_eq!(setting.slug(), s);
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

    /// The setting applied to `body` (`dialect`), as one hop sends it, and
    /// what is said about it.
    fn applied(setting: &str, dialect: Dialect, mut body: Value) -> (Value, Option<EffortChange>) {
        let setting = ReasoningSetting::parse(setting);
        let asked = Asked::of(setting, dialect, &body);
        if let Some(s) = setting {
            s.apply(dialect, &mut body, "gpt-5");
        }
        let change = asked.change(dialect, &body);
        (body, change)
    }

    #[test]
    fn a_level_changes_only_a_request_with_reasoning_on() {
        let (body, change) = applied(
            "max",
            Dialect::Chat,
            json!({"model": "m", "reasoning_effort": "xhigh", "messages": []}),
        );
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(
            change.unwrap().detail(),
            json!({"asked": "xhigh", "sent": "max", "reason": "setting"})
        );

        // No reasoning: a title request goes out as sent.
        let title = json!({"model": "m", "messages": []});
        assert_eq!(applied("max", Dialect::Chat, title.clone()), (title, None));

        // Reasoning turned off by the caller stays off.
        let none = json!({"model": "m", "reasoning_effort": "none"});
        assert_eq!(applied("max", Dialect::Chat, none.clone()), (none, None));

        // Already at that level: nothing to say.
        let same = json!({"model": "m", "reasoning": {"effort": "max"}, "input": "hi"});
        assert_eq!(
            applied("max", Dialect::Responses, same.clone()),
            (same, None)
        );
    }

    #[test]
    fn a_level_is_written_where_the_callers_format_puts_it() {
        // Anthropic adaptive thinking, no effort named: one is added, and
        // there was nothing asked to say it against.
        let (body, change) = applied(
            "high",
            Dialect::Anthropic,
            json!({"model": "m", "max_tokens": 64000,
                   "thinking": {"type": "adaptive"}, "messages": []}),
        );
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(change, None);

        let (body, _) = applied(
            "high",
            Dialect::Responses,
            json!({"model": "m", "reasoning": {"effort": "low"}, "input": "hi"}),
        );
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn off_turns_reasoning_off_as_each_format_says_it() {
        let (claude, change) = applied(
            "off",
            Dialect::Anthropic,
            json!({"model": "m", "max_tokens": 64000,
                   "thinking": {"type": "adaptive"},
                   "output_config": {"effort": "max"}, "messages": []}),
        );
        assert!(claude.get("thinking").is_none(), "{claude}");
        assert!(claude.get("output_config").is_none(), "{claude}");
        assert_eq!(
            change.unwrap().detail(),
            json!({"asked": "max", "sent": null, "reason": "setting"})
        );

        // OpenAI's formats say `none`: left out, the model's default applies.
        let (chat, change) = applied(
            "off",
            Dialect::Chat,
            json!({"model": "m", "reasoning_effort": "high"}),
        );
        assert_eq!(chat["reasoning_effort"], "none");
        assert_eq!(
            change.unwrap().detail(),
            json!({"asked": "high", "sent": "none", "reason": "setting"})
        );

        // Nothing to turn off.
        let plain = json!({"model": "m", "messages": []});
        assert_eq!(applied("off", Dialect::Chat, plain.clone()), (plain, None));
    }

    #[test]
    fn a_fix_rewrites_the_effort_as_written() {
        let mut body =
            serde_json::to_vec(&json!({"model": "m", "reasoning_effort": "xhigh"})).unwrap();
        assert_eq!(written(Dialect::Chat, &body), Some(L(Effort::XHigh)));
        assert_eq!(
            apply_fix(Dialect::Chat, &mut body, EffortFix::Use(Effort::High)),
            Some(Some("high"))
        );
        assert_eq!(written(Dialect::Chat, &body), Some(L(Effort::High)));
        assert_eq!(
            apply_fix(Dialect::Chat, &mut body, EffortFix::Drop),
            Some(None)
        );
        assert_eq!(written(Dialect::Chat, &body), None);
        let before = body.clone();
        assert_eq!(apply_fix(Dialect::Chat, &mut body, EffortFix::Drop), None);
        assert_eq!(body, before);

        // Written while thinking is off: still the field the upstream reads.
        let mut off = serde_json::to_vec(&json!({"model": "m", "max_tokens": 9,
            "thinking": {"type": "disabled"}, "output_config": {"effort": "xhigh"}}))
        .unwrap();
        assert_eq!(written(Dialect::Anthropic, &off), Some(L(Effort::XHigh)));
        assert_eq!(
            apply_fix(Dialect::Anthropic, &mut off, EffortFix::Use(Effort::High)),
            Some(Some("high"))
        );

        // A budget only: no written effort.
        let budget = serde_json::to_vec(&json!({"model": "m", "max_tokens": 64000,
            "thinking": {"type": "enabled", "budget_tokens": 32000}}))
        .unwrap();
        assert_eq!(written(Dialect::Anthropic, &budget), None);
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
        let said = r#"{"error":{"message":"Unsupported value: 'xhigh' is not supported with this model. Supported values are: 'low', 'medium', and 'high'.","param":"reasoning_effort"}}"#;
        assert_eq!(
            rejection(&http(400, said), "relay", L(Effort::XHigh)),
            Some(EffortFix::Use(Effort::High))
        );
        // Claude with thinking off: a ceiling.
        let claude = r#"{"type":"error","error":{"type":"invalid_request_error","message":"effort 'xhigh' is not supported when thinking is disabled. Use effort 'high' or below."}}"#;
        assert_eq!(
            rejection(&http(400, claude), "relay", L(Effort::XHigh)),
            Some(EffortFix::Use(Effort::High))
        );
        // `none` not taken: left out.
        let none = r#"{"error":{"message":"Invalid value for reasoning_effort: 'none'"}}"#;
        assert_eq!(
            rejection(&http(400, none), "relay", WrittenEffort::Off),
            Some(EffortFix::Drop)
        );
        // Not a 400: an upstream failing is not a refusal.
        assert_eq!(rejection(&http(500, said), "relay", L(Effort::XHigh)), None);
        // A refusal of something else.
        assert_eq!(
            rejection(
                &http(
                    400,
                    r#"{"message":"text.verbosity must be one of low, medium, high"}"#
                ),
                "relay",
                L(Effort::XHigh)
            ),
            None
        );
        // The upstream's own name is not what it said.
        assert_eq!(
            rejection(
                &GatewayError::ProviderHttpError {
                    status: 400,
                    message: "high effort reasoning relay: messages: Field required".into(),
                    retry_after: None,
                },
                "high effort reasoning relay",
                L(Effort::High)
            ),
            None
        );
        assert_eq!(
            rejection(
                &GatewayError::UpstreamAuthError {
                    status: 403,
                    message: "reasoning effort".into()
                },
                "relay",
                L(Effort::High)
            ),
            None
        );
    }

    #[test]
    fn the_memory_keeps_each_refused_level_for_a_week() {
        let t0 = 1_800_000_000;
        let mut m = Memory::default();
        m.note(L(Effort::XHigh), EffortFix::Use(Effort::High), t0);
        m.note(WrittenEffort::Off, EffortFix::Drop, t0 + 10);
        assert_eq!(
            m.instead_of(L(Effort::XHigh), t0 + 60),
            Some(EffortFix::Use(Effort::High))
        );
        assert_eq!(
            m.instead_of(WrittenEffort::Off, t0 + 60),
            Some(EffortFix::Drop)
        );
        assert_eq!(m.instead_of(L(Effort::High), t0 + 60), None);
        // A week on, the refused level is tried again.
        assert_eq!(m.instead_of(L(Effort::XHigh), t0 + MEMORY_TTL_SECS), None);

        // As stored, and read back.
        let stored = serde_json::to_string(&m).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stored).unwrap(),
            json!({"xhigh": {"use": "high", "at": t0}, "none": {"use": null, "at": t0 + 10}})
        );
        assert_eq!(Memory::parse(&stored), m);
        // Anything else reads as nothing remembered.
        assert_eq!(Memory::parse("not json"), Memory::default());

        // A fix refused itself is forgotten; replaced when another works.
        m.forget(L(Effort::XHigh));
        assert_eq!(m.instead_of(L(Effort::XHigh), t0 + 60), None);
        m.note(L(Effort::XHigh), EffortFix::Use(Effort::Medium), t0 + 20);
        assert_eq!(
            m.instead_of(L(Effort::XHigh), t0 + 60),
            Some(EffortFix::Use(Effort::Medium))
        );
        // Answered at the level first refused: nothing to remember.
        m.note(L(Effort::XHigh), EffortFix::Use(Effort::XHigh), t0 + 30);
        assert_eq!(m.instead_of(L(Effort::XHigh), t0 + 60), None);

        // Noting drops what expired.
        m.note(
            L(Effort::Low),
            EffortFix::Use(Effort::Medium),
            t0 + MEMORY_TTL_SECS + 20,
        );
        assert_eq!(m.0.len(), 1, "{m:?}");
    }

    #[test]
    fn the_memory_is_keyed_on_the_provider_the_format_and_the_model() {
        let id = Uuid::nil();
        assert_eq!(
            key(id),
            "effort_memory:{provider:00000000-0000-0000-0000-000000000000}"
        );
        assert_eq!(field(Dialect::Chat, "gpt-5"), "openai-chat:gpt-5");
        assert_ne!(
            field(Dialect::Anthropic, "m"),
            field(Dialect::Responses, "m")
        );
    }
}
