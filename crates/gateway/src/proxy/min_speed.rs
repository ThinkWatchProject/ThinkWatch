//! Cut a stream an upstream generates too slowly.
//!
//! A provider can set the slowest generation it may keep a stream at
//! (`providers.min_speed_tokens_per_sec`, averaged over
//! `providers.min_speed_window_secs`). Some upstreams — an overloaded
//! relay, a small self-hosted model — keep a stream open and dribble a
//! token every second or two: the client neither gets its answer nor an
//! error it could retry on. With a minimum set, such a stream is cut and
//! the client told why (503), so it can try again — elsewhere, since the
//! route has a failure against it and its affinity is dropped.
//!
//! # The rule
//!
//! - Only streams. A whole answer and a WebSocket turn are not measured.
//! - Counting starts at the first token. Until `window` has passed since
//!   then, nothing is judged.
//! - Once a second, the speed over the last `window` is
//!   `visible tokens in it / (window − time left out of it)`; below the
//!   minimum, the stream is cut.
//! - Left out of the window, with whatever arrived in it:
//!   - a **long pause**: [`long_pause`] or more without content — a
//!     quarter of the window, at most 5 s. An upstream that went quiet is
//!     not generating slowly, it is not generating at all, and a model
//!     thinking behind a hidden reasoning block looks the same;
//!   - a **summarised reasoning phase** (see below): the reasoning is
//!     generated at full speed, but the upstream sends a summary of it in
//!     bursts every few seconds.
//!
//!   When what is left is less than half the window, that second is not
//!   judged — a few seconds of data are not enough to conclude on.
//! - No second is judged while a summarised reasoning phase is open, nor
//!   one whose last content arrived more than [`CATCH_UP`] ago: an
//!   upstream often holds tokens back and sends them in one burst, and the
//!   burst is waited for.
//! - Nothing is judged once the upstream has said the answer is done (a
//!   stop reason, or its format's last frame), however long it keeps the
//!   connection open after.
//!
//! # Reasoning
//!
//! Reasoning streamed as it is generated counts like the answer: Chat's
//! `reasoning_content` and Responses' `response.reasoning_text.delta`.
//! Reasoning streamed as a summary does not, and its phase is left out:
//! Responses' `response.reasoning_summary_text.delta`, Anthropic's thinking
//! blocks (summarised on every model that streams them now), Gemini's
//! thought parts (thought summaries) and Bedrock's reasoning content
//! (Anthropic models' summaries, there too). A phase runs from the
//! reasoning block's start to its end, or to the first answer text or
//! tool call after it.
//!
//! # Counting tokens
//!
//! The visible text the upstream streams — answer text, reasoning text
//! that counts (above) and tool arguments, read in its own format with
//! `tw_dialect::convert::Reader` — estimated as the desktop gateway
//! estimates its live speed: an ASCII byte is a quarter of a token, any
//! other character one token. Bytes alone (four to a token, as
//! [`crate::usage_estimate`] bills an answer nobody reported) would count a
//! Chinese character as three quarters of a token where tokenizers give
//! it one or more, and cut Chinese answers that are not slow.
//!
//! Counts the upstream reports are not used: Gemini's running output count
//! includes the thoughts it does not show, and one that leaps ahead of the
//! text would leave the text behind it counting for nothing.
//!
//! What is billed is not this: a stream cut here is billed like any other
//! stream cut short (see `usage_estimate::complete`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tw_dialect::convert::Reader;
use tw_dialect::frame;
use tw_dialect::ir::{BlockKind, Delta, Dialect, Event};

/// The longest a pause has to be to be left out of the window.
pub const LONG_PAUSE_MAX: Duration = Duration::from_secs(5);

/// A second whose last content arrived longer ago than this is not
/// judged: the burst that catches up is waited for.
pub const CATCH_UP: Duration = Duration::from_secs(1);

/// How often a stream's speed is judged.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);

/// The ranges the columns allow, checked again where the row is read.
pub const TOKENS_PER_SEC: std::ops::RangeInclusive<u32> = 1..=1000;
pub const WINDOW_SECS: std::ops::RangeInclusive<u32> = 5..=120;

/// A pause this long or longer is left out of a `window`: a quarter of
/// it, at most [`LONG_PAUSE_MAX`].
pub fn long_pause(window: Duration) -> Duration {
    (window / 4).min(LONG_PAUSE_MAX)
}

/// The slowest generation a provider's streams may keep up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinSpeed {
    pub tokens_per_sec: u32,
    pub window_secs: u32,
}

impl MinSpeed {
    /// From the provider row: `None` when off, or when a value is out of
    /// range (the columns' CHECKs keep that from happening).
    pub fn from_row(tokens_per_sec: Option<i32>, window_secs: i32) -> Option<Self> {
        let tokens_per_sec = u32::try_from(tokens_per_sec?).ok()?;
        let window_secs = u32::try_from(window_secs).ok()?;
        (TOKENS_PER_SEC.contains(&tokens_per_sec) && WINDOW_SECS.contains(&window_secs)).then_some(
            Self {
                tokens_per_sec,
                window_secs,
            },
        )
    }

    fn window(&self) -> Duration {
        Duration::from_secs(u64::from(self.window_secs))
    }
}

/// Text tokens, estimated as the desktop gateway does: four ASCII bytes
/// to a token, one token for any other character.
#[derive(Debug, Default, Clone, Copy)]
struct Text {
    ascii: u64,
    other: u64,
}

impl Text {
    fn add(&mut self, s: &str) {
        for &b in s.as_bytes() {
            if b.is_ascii() {
                self.ascii += 1;
            } else if b & 0xC0 != 0x80 {
                // A character's first byte; continuation bytes are 10xxxxxx.
                self.other += 1;
            }
        }
    }

    fn tokens(&self) -> u64 {
        self.ascii.div_ceil(4) + self.other
    }
}

/// A reasoning block under way.
#[derive(Debug, Clone, Copy)]
struct Phase {
    /// The block's index, when its start was seen.
    index: Option<usize>,
    since: Instant,
    /// Its reasoning is streamed as generated, not summarised: it counts.
    raw: bool,
}

/// One stream's speed, fed the upstream's bytes as they arrive.
pub struct SpeedWatch {
    min: MinSpeed,
    upstream: Dialect,
    reader: Reader,
    frames: frame::Decoder,
    text: Text,
    /// Tokens already put down in `arrivals`.
    counted: u64,
    /// When the first content arrived.
    first: Option<Instant>,
    /// The upstream said the answer is done: what it sends or keeps open
    /// after that is not generation.
    done: bool,
    /// Each piece of visible content: when, and how many tokens it
    /// brought. The window's, plus the last one before it (the pause that
    /// runs into the window starts there).
    arrivals: VecDeque<(Instant, u64)>,
    /// The reasoning block under way, if any.
    phase: Option<Phase>,
    /// Summarised reasoning phases that ended: from, to.
    summarised: VecDeque<(Instant, Instant)>,
}

impl SpeedWatch {
    pub fn new(min: MinSpeed, upstream: Dialect) -> Self {
        Self {
            min,
            upstream,
            reader: Reader::new(upstream),
            frames: frame::Decoder::default(),
            text: Text::default(),
            counted: 0,
            first: None,
            done: false,
            arrivals: VecDeque::new(),
            phase: None,
            summarised: VecDeque::new(),
        }
    }

    pub fn min(&self) -> MinSpeed {
        self.min
    }

    /// Whether there is anything to judge: the first token is in, and the
    /// upstream has not said the answer is done.
    pub fn started(&self) -> bool {
        self.first.is_some() && !self.done
    }

    /// Reasoning in this format streams as generated, unless the frames
    /// say otherwise (Responses).
    fn raw_by_default(&self) -> bool {
        self.upstream == Dialect::Chat
    }

    /// A piece of the upstream's stream (SSE, in its own format) arrived
    /// at `now`.
    pub fn feed(&mut self, chunk: &[u8], now: Instant) {
        // The frames say what the parsed events do not: which reasoning a
        // Responses delta is, and the format's last frame.
        let mut raw_reasoning = false;
        for f in self.frames.feed(chunk) {
            if tw_dialect::convert::ends_answer(self.upstream, &f) {
                self.done = true;
            }
            if self.upstream == Dialect::Responses && is_raw_reasoning(&f) {
                raw_reasoning = true;
            }
        }
        let mut visible = false;
        for e in self.reader.feed(chunk) {
            match e {
                Event::BlockStart {
                    index,
                    kind: BlockKind::Thinking,
                } => self.open_phase(Some(index), now),
                Event::BlockStop { index } => {
                    if self.phase.is_some_and(|p| p.index == Some(index)) {
                        self.close_phase(now);
                    }
                }
                Event::Delta {
                    delta: Delta::Thinking(t),
                    ..
                } if !t.is_empty() => {
                    self.first.get_or_insert(now);
                    if self.phase.is_none() {
                        self.open_phase(None, now);
                    }
                    if let Some(p) = self.phase.as_mut() {
                        p.raw |= raw_reasoning;
                        if p.raw {
                            self.text.add(&t);
                            visible = true;
                        }
                    }
                }
                Event::Delta {
                    delta: Delta::Text(t) | Delta::ToolInput(t),
                    ..
                } if !t.is_empty() => {
                    // The answer began: any reasoning before it is over.
                    if self.phase.is_some() {
                        self.close_phase(now);
                    }
                    self.text.add(&t);
                    visible = true;
                }
                Event::Stop(_) => self.done = true,
                _ => {}
            }
        }
        if visible {
            self.arrived(now);
        }
    }

    fn open_phase(&mut self, index: Option<usize>, now: Instant) {
        if self.phase.is_some() {
            self.close_phase(now);
        }
        self.phase = Some(Phase {
            index,
            since: now,
            raw: self.raw_by_default(),
        });
    }

    fn close_phase(&mut self, now: Instant) {
        if let Some(p) = self.phase.take()
            && !p.raw
        {
            self.summarised.push_back((p.since, now));
        }
    }

    /// A summarised reasoning phase is open.
    fn summarising(&self) -> bool {
        self.phase.is_some_and(|p| !p.raw)
    }

    fn arrived(&mut self, now: Instant) {
        let total = self.text.tokens();
        let tokens = total.saturating_sub(self.counted);
        self.counted = self.counted.max(total);
        self.first.get_or_insert(now);
        self.arrivals.push_back((now, tokens));
    }

    /// The speed over the last window, in tokens per second, when it is
    /// below the minimum at `now` — `None` when it is not, or when it is
    /// not to be judged (see the module docs).
    pub fn too_slow(&mut self, now: Instant) -> Option<u32> {
        if self.done || self.summarising() {
            return None;
        }
        // The burst that catches up is waited for.
        let last = self.arrivals.back()?.0;
        if now.saturating_duration_since(last) > CATCH_UP {
            return None;
        }
        let rate = self.rate(now)?;
        (rate < f64::from(self.min.tokens_per_sec)).then(|| rate.floor() as u32)
    }

    /// The speed over the last window at `now`, when there is enough of
    /// the window left to judge it.
    fn rate(&mut self, now: Instant) -> Option<f64> {
        let first = self.first?;
        let window = self.min.window();
        if now.saturating_duration_since(first) < window {
            return None;
        }
        let start = now - window;
        // Keep one arrival at or before the window's start: a pause that
        // runs into the window is measured from it.
        while self.arrivals.len() >= 2 && self.arrivals[1].0 <= start {
            self.arrivals.pop_front();
        }
        while self.summarised.front().is_some_and(|(_, to)| *to <= start) {
            self.summarised.pop_front();
        }
        let tokens: u64 = self
            .arrivals
            .iter()
            .filter(|(at, _)| *at > start)
            .map(|(_, n)| n)
            .sum();
        // Left out: the long gaps between arrivals (and from the last one
        // to now), and the summarised reasoning phases.
        let pause = long_pause(window);
        let ends = self
            .arrivals
            .iter()
            .map(|(at, _)| *at)
            .skip(1)
            .chain(std::iter::once(now));
        let mut out: Vec<(Instant, Instant)> = self
            .arrivals
            .iter()
            .map(|(at, _)| *at)
            .zip(ends)
            .filter(|(from, to)| to.saturating_duration_since(*from) >= pause)
            .chain(self.summarised.iter().copied())
            .chain(self.phase.filter(|p| !p.raw).map(|p| (p.since, now)))
            .map(|(from, to)| (from.max(start), to.min(now)))
            .filter(|(from, to)| to > from)
            .collect();
        out.sort();
        let mut left_out = Duration::ZERO;
        let mut reach = start;
        for (from, to) in out {
            let from = from.max(reach);
            if to > from {
                left_out += to - from;
                reach = to;
            }
        }
        let measured = window.saturating_sub(left_out);
        if measured < window / 2 {
            return None;
        }
        Some(tokens as f64 / measured.as_secs_f64())
    }
}

/// A Responses frame of reasoning streamed as generated
/// (`response.reasoning_text.delta`), named by its event or its `type`.
fn is_raw_reasoning(f: &frame::Frame) -> bool {
    const RAW: &str = "response.reasoning_text.delta";
    match &f.event {
        Some(e) => e == RAW,
        None => {
            f.data.contains(RAW)
                && serde_json::from_str::<serde_json::Value>(&f.data)
                    .ok()
                    .is_some_and(|v| v.get("type").and_then(|t| t.as_str()) == Some(RAW))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn secs(n: f64) -> Duration {
        Duration::from_secs_f64(n)
    }

    fn min(tokens_per_sec: u32, window_secs: u32) -> MinSpeed {
        MinSpeed {
            tokens_per_sec,
            window_secs,
        }
    }

    fn chat_delta(delta: Value, finish: Option<&str>) -> Vec<u8> {
        format!(
            "data: {}\n\n",
            json!({"id": "c", "object": "chat.completion.chunk", "model": "m",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        )
        .into_bytes()
    }

    /// A Chat chunk carrying `text`.
    fn chat(text: &str) -> Vec<u8> {
        chat_delta(json!({"content": text}), None)
    }

    /// `tokens` ASCII tokens of text.
    fn words(tokens: usize) -> String {
        "abcd".repeat(tokens)
    }

    /// An SSE frame named by its `type`.
    fn event(v: Value) -> Vec<u8> {
        format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()).into_bytes()
    }

    fn at(t: Instant, s: f64) -> Instant {
        t + secs(s)
    }

    #[test]
    fn the_row_turns_it_on_only_with_a_speed_in_range() {
        assert_eq!(MinSpeed::from_row(None, 20), None);
        assert_eq!(MinSpeed::from_row(Some(15), 20), Some(min(15, 20)));
        assert_eq!(MinSpeed::from_row(Some(0), 20), None);
        assert_eq!(MinSpeed::from_row(Some(1001), 20), None);
        assert_eq!(MinSpeed::from_row(Some(15), 4), None);
        assert_eq!(MinSpeed::from_row(Some(15), 121), None);
    }

    #[test]
    fn a_long_pause_is_a_quarter_of_the_window_and_at_most_five_seconds() {
        assert_eq!(long_pause(secs(5.0)), secs(1.25));
        assert_eq!(long_pause(secs(10.0)), secs(2.5));
        assert_eq!(long_pause(secs(20.0)), secs(5.0));
        assert_eq!(long_pause(secs(120.0)), secs(5.0));
    }

    #[test]
    fn ascii_counts_by_the_quarter_and_other_characters_whole() {
        let mut t = Text::default();
        t.add("abcdefgh");
        assert_eq!(t.tokens(), 2);
        t.add("你好");
        assert_eq!(t.tokens(), 4);
        t.add("é");
        assert_eq!(t.tokens(), 5);
    }

    #[test]
    fn nothing_is_judged_before_the_first_token_or_within_a_window_of_it() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 20), Dialect::Chat);
        // The opening carries no content.
        w.feed(&chat_delta(json!({"role": "assistant"}), None), t);
        assert!(!w.started());
        assert_eq!(w.too_slow(at(t, 60.0)), None);
        // A character every half second from 1 s: slow, but not a window yet.
        for i in 0..40 {
            w.feed(&chat("a"), at(t, 1.0 + i as f64 * 0.5));
        }
        assert!(w.started());
        assert_eq!(w.too_slow(at(t, 20.9)), None);
        w.feed(&chat("a"), at(t, 21.0));
        assert!(w.too_slow(at(t, 21.0)).is_some());
    }

    #[test]
    fn a_stream_under_the_minimum_is_cut_with_the_rate_it_had() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 10), Dialect::Chat);
        // Four tokens a second: two every half second.
        for i in 0..20 {
            w.feed(&chat(&words(2)), at(t, i as f64 * 0.5));
        }
        assert_eq!(w.too_slow(at(t, 9.9)), None, "not a window yet");
        for i in 20..24 {
            w.feed(&chat(&words(2)), at(t, i as f64 * 0.5));
        }
        // At 11.5 s the window holds (1.5, 11.5]: 20 arrivals, 40 tokens.
        assert_eq!(w.too_slow(at(t, 11.5)), Some(4));
    }

    #[test]
    fn a_stream_at_the_minimum_runs_on() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 10), Dialect::Chat);
        for i in 0..60 {
            let now = at(t, i as f64 * 0.5);
            w.feed(&chat(&words(5)), now);
            assert_eq!(w.too_slow(now), None, "at {i}");
        }
    }

    #[test]
    fn a_long_pause_is_left_out_of_the_window() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(15, 20), Dialect::Chat);
        // 20 tokens a second until 12 s, 6 s of silence, then on.
        for i in 0..=24 {
            w.feed(&chat(&words(10)), at(t, i as f64 * 0.5));
        }
        for i in 0..=4 {
            w.feed(&chat(&words(10)), at(t, 18.0 + i as f64 * 0.5));
        }
        // (0, 20]: 29 arrivals, 290 tokens. Over 20 s that is 14.5 a
        // second; without the pause, over 14 s, 20.7.
        assert_eq!(w.too_slow(at(t, 20.0)), None);
        assert_eq!(w.rate(at(t, 20.0)).map(f64::round), Some(21.0));
    }

    /// The pause that counts as long shrinks with the window: a quarter of
    /// a short one. Five seconds would be half of an 8 s window.
    #[test]
    fn a_pause_of_a_quarter_window_is_long() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 8), Dialect::Chat);
        // 12 tokens a second until 8 s, 2.5 s of silence, then one piece.
        for i in 0..=16 {
            w.feed(&chat(&words(6)), at(t, i as f64 * 0.5));
        }
        w.feed(&chat(&words(6)), at(t, 10.5));
        // (2.5, 10.5]: 12 arrivals, 72 tokens, over the 5.5 s that are not
        // the pause. Counting the pause, 9 a second.
        assert_eq!(w.too_slow(at(t, 10.5)), None);
        assert_eq!(w.rate(at(t, 10.5)).map(f64::round), Some(13.0));
    }

    /// A second whose last content is more than a second old is not
    /// judged: upstreams hold tokens back and send them in a burst.
    #[test]
    fn the_burst_after_a_short_pause_is_waited_for() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 5), Dialect::Chat);
        // 12 tokens a second until 6 s, then quiet.
        for i in 0..=12 {
            w.feed(&chat(&words(6)), at(t, i as f64 * 0.5));
        }
        // At 7.2 s, (2.2, 7.2] holds 48 tokens over 5 s — under 10 a
        // second — but the last of them is 1.2 s old.
        assert_eq!(w.rate(at(t, 7.2)).map(|r| (r * 10.0).round()), Some(96.0));
        assert_eq!(w.too_slow(at(t, 7.2)), None);
        // The burst: the 1.3 s it took is a long pause for a 5 s window.
        w.feed(&chat(&words(30)), at(t, 7.3));
        assert_eq!(w.too_slow(at(t, 7.3)), None);
    }

    #[test]
    fn a_window_mostly_paused_is_not_judged() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 20), Dialect::Chat);
        w.feed(&chat("a"), t);
        w.feed(&chat("a"), at(t, 1.0));
        // From 1 s to 21 s the stream is silent: nothing is left to judge.
        w.feed(&chat("a"), at(t, 21.0));
        assert_eq!(w.too_slow(at(t, 21.0)), None);
        assert_eq!(w.too_slow(at(t, 21.5)), None);
    }

    #[test]
    fn a_pause_running_into_the_window_is_cut_at_its_start() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(1, 10), Dialect::Chat);
        // Steady until 8 s, silent until 14 s, steady after.
        for i in 0..=16 {
            w.feed(&chat(&words(4)), at(t, i as f64 * 0.5));
        }
        for i in 0..=12 {
            w.feed(&chat(&words(4)), at(t, 14.0 + i as f64 * 0.5));
        }
        // Window (10, 20]: 4 s of the pause 8 → 14 are in it, 6 s are
        // measured, and the arrivals at 14..=20 bring 13 × 4 tokens.
        assert_eq!(
            w.rate(at(t, 20.0)).map(|r| (r * 100.0).round()),
            Some(867.0)
        );
    }

    #[test]
    fn reasoning_streamed_as_generated_and_tool_arguments_count() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(1, 5), Dialect::Chat);
        w.feed(&chat_delta(json!({"reasoning_content": "abcd"}), None), t);
        w.feed(
            &chat_delta(
                json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                       "function": {"name": "ls", "arguments": "{\"a\":1}"}}]}),
                None,
            ),
            t,
        );
        assert_eq!(w.counted, 3);
    }

    /// Gemini's running count includes the thoughts it does not show: one
    /// that leaps ahead of the text must not leave the text counting for
    /// nothing.
    #[test]
    fn a_count_that_includes_hidden_reasoning_is_not_used() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(2, 5), Dialect::Gemini);
        let chunk = |text: &str, usage: Option<Value>| {
            let mut v =
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}}]});
            if let Some(u) = usage {
                v["usageMetadata"] = u;
            }
            format!("data: {v}\n\n").into_bytes()
        };
        w.feed(
            &chunk(
                &words(2),
                Some(json!({"promptTokenCount": 3, "candidatesTokenCount": 2, "thoughtsTokenCount": 2000})),
            ),
            t,
        );
        // Four visible tokens a second.
        for i in 1..=16 {
            let now = at(t, i as f64 * 0.5);
            w.feed(&chunk(&words(2), None), now);
            assert_eq!(w.too_slow(now), None, "at {i}");
        }
        assert_eq!(w.counted, 34);
    }

    fn responses(v: Value) -> Vec<u8> {
        event(v)
    }

    fn reasoning_item_added() -> Vec<u8> {
        responses(
            json!({"type": "response.output_item.added", "output_index": 0,
                         "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        )
    }

    fn message_item_added() -> Vec<u8> {
        [
            responses(json!({"type": "response.output_item.done", "output_index": 0,
                             "item": {"type": "reasoning", "id": "rs_1", "summary": []}})),
            responses(json!({"type": "response.output_item.added", "output_index": 1,
                             "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}})),
            responses(json!({"type": "response.content_part.added", "output_index": 1, "content_index": 0,
                             "part": {"type": "output_text", "text": ""}})),
        ]
        .concat()
    }

    fn output_text(text: &str) -> Vec<u8> {
        responses(
            json!({"type": "response.output_text.delta", "output_index": 1,
                         "content_index": 0, "delta": text}),
        )
    }

    /// A reasoning summary arrives in bursts while the model reasons at
    /// full speed: its phase is not judged, and is left out after.
    #[test]
    fn a_summarised_reasoning_phase_is_left_out() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 10), Dialect::Responses);
        w.feed(
            &[
                responses(json!({"type": "response.created",
                                 "response": {"id": "resp_1", "model": "m", "status": "in_progress"}})),
                reasoning_item_added(),
            ]
            .concat(),
            t,
        );
        // A burst of summary every two seconds until 12 s.
        for i in 0..=6 {
            let now = at(t, i as f64 * 2.0);
            w.feed(
                &responses(json!({"type": "response.reasoning_summary_text.delta",
                                  "output_index": 0, "summary_index": 0, "delta": words(4)})),
                now,
            );
            assert_eq!(w.too_slow(now), None, "while summarising, at {i}");
        }
        // The answer, 12 tokens a second from 12 s.
        w.feed(&message_item_added(), at(t, 12.0));
        for i in 1..=10 {
            let now = at(t, 12.0 + i as f64 * 0.5);
            w.feed(&output_text(&words(6)), now);
            assert_eq!(w.too_slow(now), None, "answering, at {i}");
        }
        // At 17 s, (7, 17] is 5 s of summary and 5 s of answer: 60 tokens
        // over 5 s. Counting the summary as content, 60 + 8 over 10 s.
        assert_eq!(w.rate(at(t, 17.0)).map(f64::round), Some(12.0));
    }

    #[test]
    fn reasoning_streamed_as_generated_is_judged() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 5), Dialect::Responses);
        w.feed(&reasoning_item_added(), t);
        // Four tokens a second of raw reasoning.
        let mut cut = None;
        for i in 0..=14 {
            let now = at(t, i as f64 * 0.5);
            w.feed(
                &responses(json!({"type": "response.reasoning_text.delta",
                                  "output_index": 0, "content_index": 0, "delta": words(2)})),
                now,
            );
            cut = cut.or(w.too_slow(now));
        }
        assert_eq!(cut, Some(4));
    }

    /// Anthropic's thinking is summarised: a slow trickle of it is not
    /// judged, however long it lasts.
    #[test]
    fn an_anthropic_thinking_block_is_not_judged_while_it_is_open() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 5), Dialect::Anthropic);
        w.feed(
            &[
                event(
                    json!({"type": "message_start", "message": {"id": "m", "type": "message",
                    "role": "assistant", "model": "m", "content": [],
                    "usage": {"input_tokens": 1, "output_tokens": 1}}}),
                ),
                event(json!({"type": "content_block_start", "index": 0,
                             "content_block": {"type": "thinking", "thinking": ""}})),
            ]
            .concat(),
            t,
        );
        for i in 0..=40 {
            let now = at(t, i as f64 * 0.5);
            w.feed(
                &event(json!({"type": "content_block_delta", "index": 0,
                              "delta": {"type": "thinking_delta", "thinking": "a"}})),
                now,
            );
            assert_eq!(w.too_slow(now), None, "at {i}");
        }
        // The block closes and the answer trickles: that is judged.
        w.feed(
            &event(json!({"type": "content_block_stop", "index": 0})),
            at(t, 20.0),
        );
        w.feed(
            &event(json!({"type": "content_block_start", "index": 1,
                          "content_block": {"type": "text", "text": ""}})),
            at(t, 20.0),
        );
        let mut cut = None;
        for i in 1..=14 {
            let now = at(t, 20.0 + i as f64 * 0.5);
            w.feed(
                &event(json!({"type": "content_block_delta", "index": 1,
                              "delta": {"type": "text_delta", "text": words(1)}})),
                now,
            );
            cut = cut.or(w.too_slow(now));
        }
        assert_eq!(cut, Some(2));
    }

    /// An upstream can keep its stream open a while after the answer's
    /// end, or send its usage later: none of that is generating slowly.
    /// The end is seen in each format, by its stop reason or its last frame.
    #[test]
    fn nothing_is_judged_once_the_answer_is_done() {
        let t = Instant::now();
        let ends: [(Dialect, Vec<u8>); 6] = [
            (Dialect::Chat, chat_delta(json!({}), Some("stop"))),
            (Dialect::Chat, b"data: [DONE]\n\n".to_vec()),
            (Dialect::Anthropic, event(json!({"type": "message_stop"}))),
            (
                Dialect::Anthropic,
                event(
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                             "usage": {"output_tokens": 9}}),
                ),
            ),
            (
                Dialect::Responses,
                responses(json!({"type": "response.completed",
                                 "response": {"id": "resp_1", "model": "m", "status": "completed", "output": []}})),
            ),
            (
                Dialect::Gemini,
                format!(
                    "data: {}\n\n",
                    json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "a"}]},
                                           "finishReason": "STOP"}]})
                )
                .into_bytes(),
            ),
        ];
        for (dialect, end) in ends {
            let mut w = SpeedWatch::new(min(1000, 5), dialect);
            // Some content first, in Chat for every format — only the end
            // matters here, and the content may not parse as `dialect`.
            w.first = Some(t);
            w.arrivals.push_back((t, 1));
            assert!(w.started());
            w.feed(&end, at(t, 5.0));
            assert!(!w.started(), "{dialect:?}");
            assert_eq!(w.too_slow(at(t, 6.0)), None, "{dialect:?}");
        }
    }
}
