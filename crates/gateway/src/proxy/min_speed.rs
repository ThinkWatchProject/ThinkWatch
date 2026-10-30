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
//!   `tokens in it / (window − long pauses in it)`; below the minimum, the
//!   stream is cut.
//! - A **long pause** is [`LONG_PAUSE`] or more without content. It and
//!   its (zero) tokens are left out of the window: an upstream that went
//!   quiet is not generating slowly, it is not generating at all, and a
//!   model thinking behind a hidden reasoning block looks the same. When
//!   the pauses leave less than half the window, that second is not
//!   judged — a few seconds of data are not enough to conclude on.
//!
//! # Counting tokens
//!
//! The text the upstream streams — answer text, reasoning text and tool
//! arguments, read in its own format with `tw_dialect::convert::Reader` —
//! estimated as the desktop gateway estimates its live speed: an ASCII
//! byte is a quarter of a token, any other character one token. Bytes
//! alone (four to a token, as [`crate::usage_estimate`] bills an answer
//! nobody reported) would count a Chinese character as three quarters of
//! a token where tokenizers give it one or more, and cut Chinese answers
//! that are not slow. Gemini reports its running output count in each
//! chunk; the larger of that and the estimate counts.
//!
//! What is billed is not this: a stream cut here is billed like any other
//! stream cut short (see `usage_estimate::complete`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tw_dialect::convert::Reader;
use tw_dialect::ir::{Delta, Dialect, Event};

/// A pause this long or longer is left out of the window.
pub const LONG_PAUSE: Duration = Duration::from_secs(5);

/// How often a stream's speed is judged.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);

/// The ranges the columns allow, checked again where the row is read.
pub const TOKENS_PER_SEC: std::ops::RangeInclusive<u32> = 1..=1000;
pub const WINDOW_SECS: std::ops::RangeInclusive<u32> = 5..=120;

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

/// One stream's speed, fed the upstream's bytes as they arrive.
pub struct SpeedWatch {
    min: MinSpeed,
    reader: Reader,
    text: Text,
    /// Gemini's running output count.
    reported: u64,
    gemini: bool,
    /// Tokens already put down in `arrivals`.
    counted: u64,
    /// When the first token arrived.
    first: Option<Instant>,
    /// The upstream said the answer is done (its stop reason): what it
    /// sends or keeps open after that is not generation.
    done: bool,
    /// Each piece of content: when, and how many tokens it brought. The
    /// window's, plus the last one before it (the pause that runs into
    /// the window starts there).
    arrivals: VecDeque<(Instant, u64)>,
}

impl SpeedWatch {
    pub fn new(min: MinSpeed, upstream: Dialect) -> Self {
        Self {
            min,
            reader: Reader::new(upstream),
            text: Text::default(),
            reported: 0,
            gemini: upstream == Dialect::Gemini,
            counted: 0,
            first: None,
            done: false,
            arrivals: VecDeque::new(),
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

    /// A piece of the upstream's stream (SSE, in its own format) arrived
    /// at `now`.
    pub fn feed(&mut self, chunk: &[u8], now: Instant) {
        let mut content = false;
        for e in self.reader.feed(chunk) {
            match e {
                Event::Delta {
                    delta: Delta::Text(t) | Delta::Thinking(t) | Delta::ToolInput(t),
                    ..
                } if !t.is_empty() => {
                    self.text.add(&t);
                    content = true;
                }
                Event::Usage(u) if self.gemini => self.reported = self.reported.max(u.output),
                Event::Stop(_) => self.done = true,
                _ => {}
            }
        }
        if content {
            self.arrived(now);
        }
    }

    fn arrived(&mut self, now: Instant) {
        let total = self.text.tokens().max(self.reported);
        let tokens = total.saturating_sub(self.counted);
        self.counted = self.counted.max(total);
        self.first.get_or_insert(now);
        self.arrivals.push_back((now, tokens));
    }

    /// The speed over the last window, in tokens per second, when it is
    /// below the minimum at `now` — `None` when it is not, or when it is
    /// too early to judge (see the module docs).
    pub fn too_slow(&mut self, now: Instant) -> Option<u32> {
        if self.done {
            return None;
        }
        let rate = self.rate(now)?;
        (rate < f64::from(self.min.tokens_per_sec)).then(|| rate.floor() as u32)
    }

    /// The speed over the last window at `now`, when it can be judged.
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
        let tokens: u64 = self
            .arrivals
            .iter()
            .filter(|(at, _)| *at > start)
            .map(|(_, n)| n)
            .sum();
        // The gaps between arrivals, and from the last one to now.
        let ends = self
            .arrivals
            .iter()
            .map(|(at, _)| *at)
            .skip(1)
            .chain(std::iter::once(now));
        let paused: Duration = self
            .arrivals
            .iter()
            .map(|(at, _)| *at)
            .zip(ends)
            .filter(|(from, to)| to.saturating_duration_since(*from) >= LONG_PAUSE)
            .map(|(from, to)| to.min(now).saturating_duration_since(from.max(start)))
            .sum();
        let measured = window.saturating_sub(paused);
        if measured < window / 2 {
            return None;
        }
        Some(tokens as f64 / measured.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn secs(n: f64) -> Duration {
        Duration::from_secs_f64(n)
    }

    fn min(tokens_per_sec: u32, window_secs: u32) -> MinSpeed {
        MinSpeed {
            tokens_per_sec,
            window_secs,
        }
    }

    /// A Chat chunk carrying `text`.
    fn chat(text: &str) -> Vec<u8> {
        format!(
            "data: {}\n\n",
            json!({"id": "c", "object": "chat.completion.chunk", "model": "m",
                   "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]})
        )
        .into_bytes()
    }

    /// `tokens` ASCII tokens of text.
    fn words(tokens: usize) -> String {
        "abcd".repeat(tokens)
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
        w.feed(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            t,
        );
        assert!(!w.started());
        assert_eq!(w.too_slow(t + secs(60.0)), None);
        w.feed(&chat("a"), t + secs(1.0));
        assert!(w.started());
        // One token in 19.9 seconds is slow, but not a window yet.
        for i in 0..19 {
            w.feed(&chat("a"), t + secs(2.0 + i as f64 * 0.5));
        }
        assert_eq!(w.too_slow(t + secs(20.9)), None);
        assert!(w.too_slow(t + secs(21.0)).is_some());
    }

    #[test]
    fn a_stream_under_the_minimum_is_cut_with_the_rate_it_had() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 10), Dialect::Chat);
        // Four tokens a second: two every half second.
        for i in 0..20 {
            w.feed(&chat(&words(2)), t + secs(i as f64 * 0.5));
        }
        assert_eq!(w.too_slow(t + secs(9.9)), None, "not a window yet");
        for i in 20..24 {
            w.feed(&chat(&words(2)), t + secs(i as f64 * 0.5));
        }
        // At 11.5 s the window holds (1.5, 11.5]: 20 arrivals, 40 tokens.
        assert_eq!(w.too_slow(t + secs(11.5)), Some(4));
    }

    #[test]
    fn a_stream_at_the_minimum_runs_on() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 10), Dialect::Chat);
        for i in 0..60 {
            let at = t + secs(i as f64 * 0.5);
            w.feed(&chat(&words(5)), at);
            assert_eq!(w.too_slow(at), None, "at {i}");
        }
    }

    #[test]
    fn a_long_pause_is_left_out_of_the_window() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(15, 20), Dialect::Chat);
        // 20 tokens a second for 12 s, then 8 s of silence: over the whole
        // 20 s window that is 12 a second, without the pause 20.
        for i in 0..=24 {
            w.feed(&chat(&words(10)), t + secs(i as f64 * 0.5));
        }
        assert_eq!(w.too_slow(t + secs(20.0)), None);
        assert_eq!(w.rate(t + secs(20.0)).map(f64::round), Some(20.0));
    }

    #[test]
    fn a_shorter_silence_slows_the_stream() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 10), Dialect::Chat);
        // 16 tokens a second until 10 s, then silent.
        for i in 0..=20 {
            w.feed(&chat(&words(8)), t + secs(i as f64 * 0.5));
        }
        // 4.9 s of silence is not a long pause: (4.9, 14.9] holds the
        // arrivals at 5.0..=10.0, 88 tokens over 10 s.
        assert_eq!(w.too_slow(t + secs(14.9)), Some(8));
        // At 5 s it is one: (5, 15] is 80 tokens over the 5 s before it.
        assert_eq!(w.too_slow(t + secs(15.0)), None);
        assert_eq!(w.rate(t + secs(15.0)).map(f64::round), Some(16.0));
    }

    /// An upstream can keep its stream open a while after the answer's
    /// end, or send its usage later: none of that is generating slowly.
    #[test]
    fn nothing_is_judged_once_the_answer_is_done() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 5), Dialect::Chat);
        for i in 0..10 {
            w.feed(&chat(&words(10)), t + secs(i as f64 * 0.5));
        }
        let stop = json!({"id": "c", "object": "chat.completion.chunk", "model": "m",
                          "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
        w.feed(format!("data: {stop}\n\n").as_bytes(), t + secs(5.0));
        assert!(!w.started());
        assert_eq!(w.too_slow(t + secs(9.0)), None);
    }

    #[test]
    fn a_window_mostly_paused_is_not_judged() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(10, 20), Dialect::Chat);
        w.feed(&chat("a"), t);
        w.feed(&chat("a"), t + secs(1.0));
        // From 1 s on the stream is silent: the pause covers 11 of the 20
        // seconds at 21 s, and only 9 are left.
        assert_eq!(w.too_slow(t + secs(21.0)), None);
        // A trickle after the pause: still mostly paused.
        w.feed(&chat("a"), t + secs(22.0));
        assert_eq!(w.too_slow(t + secs(22.5)), None);
    }

    #[test]
    fn a_pause_running_into_the_window_is_cut_at_its_start() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(1, 10), Dialect::Chat);
        // Steady until 8 s, silent until 14 s, steady after.
        for i in 0..=16 {
            w.feed(&chat(&words(4)), t + secs(i as f64 * 0.5));
        }
        for i in 0..=12 {
            w.feed(&chat(&words(4)), t + secs(14.0 + i as f64 * 0.5));
        }
        // Window (10, 20]: 4 s of the pause 8 → 14 are in it, 6 s are
        // measured, and the arrivals at 14..=20 bring 13 × 4 tokens.
        assert_eq!(
            w.rate(t + secs(20.0)).map(|r| (r * 100.0).round()),
            Some(867.0)
        );
    }

    #[test]
    fn reasoning_and_tool_arguments_count_and_signatures_do_not() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(1, 5), Dialect::Anthropic);
        let event =
            |v: serde_json::Value| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
        let stream = [
            event(json!({"type": "message_start", "message": {"id": "m", "type": "message",
                "role": "assistant", "model": "m", "content": [], "usage": {"input_tokens": 1, "output_tokens": 1}}})),
            event(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})),
            event(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "abcd"}})),
            event(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "x".repeat(400)}})),
            event(json!({"type": "content_block_stop", "index": 0})),
            event(json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t", "name": "ls", "input": {}}})),
            event(json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":1}"}})),
        ]
        .concat();
        w.feed(stream.as_bytes(), t);
        assert_eq!(w.counted, 3);
    }

    #[test]
    fn geminis_running_count_counts_when_it_is_higher() {
        let t = Instant::now();
        let mut w = SpeedWatch::new(min(1, 5), Dialect::Gemini);
        let chunk = json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "ab"}]}}],
                           "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 7}});
        w.feed(format!("data: {chunk}\n\n").as_bytes(), t);
        assert_eq!(w.counted, 7);
    }
}
