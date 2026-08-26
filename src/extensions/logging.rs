//! Shipping the device's logs.
//!
//! The only extension the device starts on its own. Everything else answers a
//! question; this one runs a tail and pushes lines as they appear, which makes
//! it the one that can flood the connection.
//!
//! # Rate limiting is not optional
//!
//! NervesHub rate limits log lines per device — a few per second with a small
//! burst — and **silently drops** anything over. A device in a crash loop
//! writing hundreds of lines a second would have almost all of them discarded
//! server-side with nothing to say so, and the surviving lines would be an
//! arbitrary sample of the interesting ones.
//!
//! So the agent limits itself to the same rate and counts what it drops,
//! reporting the count in a line of its own. A gap someone can see beats a gap
//! they cannot.
//!
//! # Lines written before the platform is listening
//!
//! The tail starts with the process; the platform attaches logging a round
//! trip or two into a session, and not at all if the product has it turned
//! off. The lines written in between are worth having — they cover the boot,
//! and whatever crash the device is reconnecting from — so they wait in
//! [`Pending`] and go out in order once the attach lands. Bounded, and what
//! the bound costs is reported the same way the rate limiter's losses are.

use std::collections::VecDeque;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::config::{LoggingConfig, LoggingSource};
use crate::error::Error;

/// Tails the configured source and hands over lines ready to send.
///
/// Runs as its own task rather than being polled from the run loop: a log
/// source is a blocking read that may produce nothing for hours, and the run
/// loop has heartbeats to keep.
pub fn spawn(config: &LoggingConfig) -> Result<mpsc::Receiver<Value>, Error> {
    let (tx, rx) = mpsc::channel(64);

    let command = match &config.source {
        LoggingSource::Journald { unit } => {
            let mut command = "journalctl --follow --output=json --lines=0".to_string();

            if let Some(unit) = unit {
                command.push_str(&format!(" --unit={unit}"));
            }

            command
        }
        LoggingSource::Command(command) => command.clone(),
    };

    let journald = matches!(config.source, LoggingSource::Journald { .. });
    let mut limiter = RateLimiter::new(config.max_lines_per_second);

    tokio::spawn(async move {
        let mut child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                log::error!("logging: could not start {command:?}: {e}");
                return;
            }
        };

        let Some(stdout) = child.stdout.take() else {
            log::error!("logging: {command:?} produced no stdout");
            return;
        };

        log::info!("logging: tailing {command:?}");

        let mut lines = BufReader::new(stdout).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            log::trace!("logging: read {line}");

            if line.trim().is_empty() {
                continue;
            }

            match limiter.check() {
                Allowed::Yes => {}
                Allowed::No => continue,
                Allowed::AfterDropping(dropped) => {
                    let notice = drop_notice(dropped, "to stay under the rate limit", now_micros());

                    if tx.send(notice).await.is_err() {
                        return;
                    }
                }
            }

            let payload = if journald {
                journald_line(&line)
            } else {
                plain_line(&line)
            };

            if tx.send(payload).await.is_err() {
                return;
            }
        }

        log::warn!("logging: {command:?} ended");
    });

    Ok(rx)
}

/// A `journalctl -o json` record, reduced to what the server stores.
///
/// Falls back to treating the line as plain text when it is not the JSON we
/// expect — `journalctl` writes a non-record line now and then, and dropping
/// the log stream over one is worse than shipping it verbatim.
fn journald_line(raw: &str) -> Value {
    let Ok(record) = serde_json::from_str::<Value>(raw) else {
        return plain_line(raw);
    };

    let message = match record.get("MESSAGE") {
        Some(Value::String(message)) => message.clone(),
        // journald renders a binary message as an array of byte values.
        Some(Value::Array(bytes)) => bytes
            .iter()
            .filter_map(|b| b.as_u64())
            .map(|b| b as u8 as char)
            .collect(),
        _ => return plain_line(raw),
    };

    let mut payload = json!({
        "level": priority_to_level(record.get("PRIORITY")),
        "message": message,
    });

    // The journal's timestamp is microseconds since the epoch as a string,
    // which is exactly what the server's `meta.time` fallback parses.
    if let Some(timestamp) = record.get("__REALTIME_TIMESTAMP").and_then(Value::as_str) {
        let mut meta = serde_json::Map::new();
        meta.insert("time".into(), json!(timestamp));

        if let Some(unit) = record.get("_SYSTEMD_UNIT").and_then(Value::as_str) {
            meta.insert("unit".into(), json!(unit));
        }

        if let Some(object) = payload.as_object_mut() {
            object.insert("meta".into(), Value::Object(meta));
        }
    }

    payload
}

/// A line of plain text, timestamped now.
///
/// The timestamp is not optional, however much the schema's `required` list
/// suggests otherwise. A log line without one fails validation on the server
/// and is dropped **silently** — no error to the device, no line in the UI.
/// It goes in `meta.time` as microseconds since the epoch, which is the shape
/// the server parses, and matches where journald's own timestamp lands.
fn plain_line(raw: &str) -> Value {
    json!({
        "level": "info",
        "message": raw,
        "meta": { "time": now_micros() },
    })
}

/// A line saying what went missing, in the shape of a log line.
///
/// The count is the point of it: whatever was dropped, and wherever it was
/// dropped, a gap someone can see beats a gap they cannot. `time` is the
/// moment the gap opened rather than the moment the notice goes out, so the
/// server orders it ahead of the lines that survived it.
fn drop_notice(dropped: u64, why: &str, time: String) -> Value {
    json!({
        "level": "warning",
        "message": format!("nerves-hub-link-agent dropped {dropped} log lines {why}"),
        "meta": { "time": time },
    })
}

fn now_micros() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros().to_string())
        .unwrap_or_else(|_| "0".into())
}

/// syslog priority to the level names NervesHub shows.
fn priority_to_level(priority: Option<&Value>) -> &'static str {
    let priority = match priority {
        Some(Value::String(s)) => s.parse::<u8>().ok(),
        Some(Value::Number(n)) => n.as_u64().map(|n| n as u8),
        _ => None,
    };

    match priority {
        Some(0..=2) => "error",
        Some(3) => "error",
        Some(4) => "warning",
        Some(5) => "notice",
        Some(6) => "info",
        Some(7) => "debug",
        _ => "info",
    }
}

enum Allowed {
    Yes,
    No,
    /// Allowed, and this many were dropped since the last one that was.
    AfterDropping(u64),
}

/// A token bucket, matching the shape of the server's own limiter.
struct RateLimiter {
    per_second: u32,
    window_started: std::time::Instant,
    sent_this_window: u32,
    dropped_since_last_send: u64,
}

impl RateLimiter {
    fn new(per_second: u32) -> Self {
        Self {
            per_second,
            window_started: std::time::Instant::now(),
            sent_this_window: 0,
            dropped_since_last_send: 0,
        }
    }

    fn check(&mut self) -> Allowed {
        if self.window_started.elapsed() >= std::time::Duration::from_secs(1) {
            self.window_started = std::time::Instant::now();
            self.sent_this_window = 0;
        }

        if self.sent_this_window >= self.per_second {
            self.dropped_since_last_send += 1;
            return Allowed::No;
        }

        self.sent_this_window += 1;

        match std::mem::take(&mut self.dropped_since_last_send) {
            0 => Allowed::Yes,
            dropped => Allowed::AfterDropping(dropped),
        }
    }
}

/// How many lines wait for an attach.
///
/// Enough to cover a negotiation, including a slow one on a device that is
/// talking while it boots. Small enough that a device the platform never
/// attaches — logging turned off for the product — holds an unremarkable
/// amount of memory forever.
pub const PENDING_LINES: usize = 128;

/// Lines the tail produced before the platform attached logging.
///
/// The alternative to holding them is throwing them away, and the lines
/// written before an attach are not the boring ones: they are the boot, and
/// the crash that caused the reconnect the attach is part of.
///
/// The bound is what makes that safe. A device whose platform never attaches
/// keeps tailing regardless, so past the cap the oldest go and the count is
/// reported when the rest are finally sent — the same bargain the rate limiter
/// makes.
pub struct Pending {
    /// Oldest first. Holds only real lines; the notice for what was dropped is
    /// built on the way out, so that its count is whatever it ends up being.
    lines: VecDeque<Value>,
    capacity: usize,
    dropped: u64,
    /// When the first line was dropped, which is where the gap starts.
    gap_opened: Option<String>,
}

impl Pending {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            // A zero-length buffer would drop every line and hold the notice
            // saying so, which is a worse answer than the smallest real one.
            capacity: capacity.max(1),
            dropped: 0,
            gap_opened: None,
        }
    }

    pub fn push(&mut self, line: Value) {
        self.lines.push_back(line);

        while self.lines.len() > self.capacity {
            self.lines.pop_front();
            self.dropped += 1;
            self.gap_opened.get_or_insert_with(now_micros);
        }
    }

    /// The next thing to send, left where it is.
    ///
    /// Paired with [`Pending::sent`] rather than handing the line over
    /// outright. A flush is a run of sends that can fail half way through —
    /// the socket dying is one of the reasons a backlog exists at all — and a
    /// line taken out of here and then not sent is exactly the loss this
    /// buffer is for. What did not go waits for the next session.
    pub fn front(&self) -> Option<Value> {
        match self.dropped {
            0 => self.lines.front().cloned(),
            dropped => Some(drop_notice(
                dropped,
                "while waiting for the platform to attach logging",
                self.gap_opened.clone().unwrap_or_else(now_micros),
            )),
        }
    }

    /// Forget what [`Pending::front`] returned, now that it has gone.
    pub fn sent(&mut self) {
        if self.dropped > 0 {
            self.dropped = 0;
            self.gap_opened = None;
            return;
        }

        self.lines.pop_front();
    }

    /// How many sends it would take to empty this, notice included.
    pub fn len(&self) -> usize {
        self.lines.len() + usize::from(self.dropped > 0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_journald_record_becomes_a_log_line() {
        let raw = r#"{"PRIORITY":"3","MESSAGE":"it broke","__REALTIME_TIMESTAMP":"1700000000000000","_SYSTEMD_UNIT":"app.service"}"#;

        let line = journald_line(raw);

        assert_eq!(line["level"], "error");
        assert_eq!(line["message"], "it broke");
        assert_eq!(line["meta"]["time"], "1700000000000000");
        assert_eq!(line["meta"]["unit"], "app.service");
    }

    #[test]
    fn a_line_that_is_not_a_record_is_shipped_as_text() {
        let line = journald_line("-- Journal begins at Tue --");

        assert_eq!(line["level"], "info");
        assert_eq!(line["message"], "-- Journal begins at Tue --");
    }

    #[test]
    fn every_line_carries_a_timestamp() {
        // Without one the server drops the line and says nothing, so this is
        // worth asserting on rather than trusting.
        for line in [
            plain_line("anything"),
            journald_line("not json"),
            journald_line(
                r#"{"PRIORITY":"6","MESSAGE":"hi","__REALTIME_TIMESTAMP":"1700000000000000"}"#,
            ),
        ] {
            let time = line["meta"]["time"]
                .as_str()
                .expect("meta.time is a string");

            assert!(
                time.parse::<u64>().unwrap() > 0,
                "{time} should be microseconds"
            );
        }
    }

    #[test]
    fn priorities_map_onto_levels() {
        assert_eq!(priority_to_level(Some(&json!("0"))), "error");
        assert_eq!(priority_to_level(Some(&json!("4"))), "warning");
        assert_eq!(priority_to_level(Some(&json!("7"))), "debug");
        assert_eq!(priority_to_level(None), "info");
    }

    #[test]
    fn lines_written_before_an_attach_are_kept_in_order() {
        let mut pending = Pending::new(8);

        for message in ["one", "two", "three"] {
            pending.push(plain_line(message));
        }

        assert_eq!(pending.len(), 3);
        assert_eq!(drain(&mut pending), ["one", "two", "three"]);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_full_buffer_loses_the_oldest_lines_and_says_how_many() {
        let mut pending = Pending::new(2);

        for message in ["one", "two", "three", "four"] {
            pending.push(plain_line(message));
        }

        let sent = drain(&mut pending);

        // The notice first: the gap is before the lines that survived it, and
        // it carries the time the first line was dropped so the server orders
        // it there too.
        assert_eq!(
            sent,
            [
                "nerves-hub-link-agent dropped 2 log lines while waiting for the platform to \
                 attach logging",
                "three",
                "four",
            ]
        );
    }

    #[test]
    fn a_line_that_was_not_sent_waits_for_the_next_session() {
        // The socket dying part way through a flush is one of the reasons
        // there is a backlog at all, so it must not be how the backlog is
        // lost.
        let mut pending = Pending::new(8);

        pending.push(plain_line("one"));
        pending.push(plain_line("two"));

        assert_eq!(pending.front().unwrap()["message"], "one");
        assert_eq!(pending.front().unwrap()["message"], "one");

        pending.sent();

        assert_eq!(drain(&mut pending), ["two"]);
    }

    /// Every line the buffer would send, in order, as its message text.
    fn drain(pending: &mut Pending) -> Vec<String> {
        let mut sent = Vec::new();

        while let Some(line) = pending.front() {
            sent.push(line["message"].as_str().expect("a message").to_string());
            pending.sent();
        }

        sent
    }

    #[test]
    fn the_limiter_reports_what_it_dropped_rather_than_hiding_it() {
        let mut limiter = RateLimiter::new(2);

        assert!(matches!(limiter.check(), Allowed::Yes));
        assert!(matches!(limiter.check(), Allowed::Yes));
        assert!(matches!(limiter.check(), Allowed::No));
        assert!(matches!(limiter.check(), Allowed::No));

        limiter.window_started = std::time::Instant::now() - std::time::Duration::from_secs(2);

        match limiter.check() {
            Allowed::AfterDropping(2) => {}
            other => panic!("expected 2 dropped, got {}", matches!(other, Allowed::Yes)),
        }
    }
}
