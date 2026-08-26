//! Shipping the device's logs.
//!
//! The only extension the device starts on its own. Everything else answers a
//! question; this one runs a tail and pushes lines as they appear, which makes
//! it the one that can flood the connection.
//!
//! # One message a second, not one a line
//!
//! NervesHub rate limits how often a device may send, not how much it may say:
//! a few messages per second per device, and **silently dropped** past that.
//! So the agent does not send a line when it has one. Lines go into
//! [`Pending`], and once a second whatever is there goes out as a single
//! `logging:send` carrying a `lines` array.
//!
//! That is what makes a crash loop reportable. A device writing hundreds of
//! lines a second used to lose almost all of them to the limiter, and the
//! survivors were an arbitrary sample of the interesting ones; now a second's
//! worth arrives together, up to the batch cap.
//!
//! Sending a batch at all requires a platform that can read one, which is what
//! the extension's `0.1.0` version declares. A NervesHub without that version
//! of the extension does not attach logging at all for a device offering it,
//! so there is no version of this where the agent sends batches into the dark.
//!
//! # What the cap costs is reported
//!
//! [`Pending`] is bounded, and so is a batch. Whatever does not fit is
//! dropped, oldest first, and the count goes out as a line of its own ahead of
//! the survivors. A gap someone can see beats a gap they cannot.

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

/// How many lines one message may carry.
///
/// The platform's own cap. Anything past it is dropped server-side, so there
/// is nothing to gain by sending more, and the drop notice this agent writes
/// counts against it like any other line.
pub const MAX_LINES_PER_BATCH: usize = 100;

/// How long the agent collects before sending.
pub const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How many flushes' worth of lines wait at once.
///
/// A burst then drains over the next few seconds instead of being thrown away,
/// while a device the platform never attaches -- logging turned off for the
/// product -- holds a bounded and unremarkable amount of memory forever.
pub const PENDING_BATCHES: usize = 5;

/// How many lines wait for the next flush, for a given batch size.
pub const fn pending_lines(max_lines_per_batch: usize) -> usize {
    max_lines_per_batch.saturating_mul(PENDING_BATCHES)
}

/// Lines waiting to be sent.
///
/// Everything the tail produces lands here, whether or not the platform has
/// attached logging yet. Holding them is the point: lines written before an
/// attach are not the boring ones -- they are the boot, and the crash that
/// caused the reconnect the attach is part of -- and lines written between
/// flushes are how one message comes to carry a second's worth.
///
/// The bound is what makes that safe. The tail keeps running whatever the
/// platform does, so past the cap the oldest go and the count is reported with
/// the rest.
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

    /// The next message's worth, left where it is.
    ///
    /// Oldest first, at most `max` of them, the drop notice ahead of the
    /// survivors it accounts for and counted against `max` like any other
    /// line -- the platform caps what one message may carry, and a notice that
    /// pushed the batch over the cap would be dropped by the very rule it
    /// exists to report.
    ///
    /// Paired with [`Pending::sent`] rather than handing the lines over
    /// outright. A send can fail -- the socket dying is one of the reasons a
    /// backlog exists at all -- and lines taken out of here and then not sent
    /// are exactly the loss this buffer is for. What did not go waits for the
    /// next flush.
    pub fn batch(&self, max: usize) -> Vec<Value> {
        let mut batch = Vec::with_capacity(max.min(self.len()));

        if self.dropped > 0 {
            batch.push(drop_notice(
                self.dropped,
                "to stay inside its buffer",
                self.gap_opened.clone().unwrap_or_else(now_micros),
            ));
        }

        batch.extend(
            self.lines
                .iter()
                .take(max.saturating_sub(batch.len()))
                .cloned(),
        );
        batch.truncate(max);

        batch
    }

    /// Forget the first `count` of what [`Pending::batch`] returned, now that
    /// it has gone.
    pub fn sent(&mut self, count: usize) {
        let mut count = count;

        if self.dropped > 0 && count > 0 {
            self.dropped = 0;
            self.gap_opened = None;
            count -= 1;
        }

        for _ in 0..count {
            let _ = self.lines.pop_front();
        }
    }

    /// How many lines are waiting, the drop notice included.
    pub fn len(&self) -> usize {
        self.lines.len() + usize::from(self.dropped > 0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The payload of a `logging:send`.
///
/// An object with one array in it rather than a bare array: Phoenix payloads
/// are maps everywhere else in this protocol, and a key leaves room for
/// whatever a later version of the extension wants to say alongside the lines.
pub fn batch_payload(lines: Vec<Value>) -> Value {
    json!({ "lines": lines })
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
    fn a_flush_carries_everything_waiting_in_the_order_it_was_written() {
        let mut pending = Pending::new(8);

        for message in ["one", "two", "three"] {
            pending.push(plain_line(message));
        }

        assert_eq!(pending.len(), 3);

        let batch = pending.batch(MAX_LINES_PER_BATCH);

        assert_eq!(messages(&batch), ["one", "two", "three"]);

        pending.sent(batch.len());

        assert!(pending.is_empty());
    }

    #[test]
    fn a_batch_stops_at_the_cap_and_the_rest_goes_next_time() {
        // The platform drops whatever a message carries past its cap, so this
        // is the one place the agent may not simply send what it has.
        let mut pending = Pending::new(100);

        for line in 1..=5 {
            pending.push(plain_line(&format!("line {line}")));
        }

        let batch = pending.batch(2);
        assert_eq!(messages(&batch), ["line 1", "line 2"]);

        pending.sent(batch.len());

        assert_eq!(messages(&pending.batch(2)), ["line 3", "line 4"]);
    }

    #[test]
    fn a_full_buffer_loses_the_oldest_lines_and_says_how_many() {
        let mut pending = Pending::new(2);

        for message in ["one", "two", "three", "four"] {
            pending.push(plain_line(message));
        }

        // The notice first: the gap is before the lines that survived it, and
        // it carries the time the first line was dropped so the server orders
        // it there too.
        assert_eq!(
            messages(&pending.batch(MAX_LINES_PER_BATCH)),
            [
                "nerves-hub-link-agent dropped 2 log lines to stay inside its buffer",
                "three",
                "four",
            ]
        );
    }

    #[test]
    fn the_drop_notice_counts_against_the_cap_like_any_other_line() {
        // Otherwise the line reporting the gap is the one the platform drops
        // for making the message too long, which would hide exactly what it
        // exists to show.
        let mut pending = Pending::new(2);

        for message in ["one", "two", "three"] {
            pending.push(plain_line(message));
        }

        let batch = pending.batch(2);

        assert_eq!(batch.len(), 2);
        assert!(batch[0]["message"]
            .as_str()
            .unwrap()
            .contains("dropped 1 log lines"));
        assert_eq!(batch[1]["message"], "two");
    }

    #[test]
    fn lines_that_were_not_sent_wait_for_the_next_flush() {
        // The socket dying part way through is one of the reasons there is a
        // backlog at all, so it must not be how the backlog is lost.
        let mut pending = Pending::new(8);

        pending.push(plain_line("one"));
        pending.push(plain_line("two"));

        assert_eq!(messages(&pending.batch(8)), ["one", "two"]);
        assert_eq!(messages(&pending.batch(8)), ["one", "two"]);

        pending.sent(1);

        assert_eq!(messages(&pending.batch(8)), ["two"]);
    }

    #[test]
    fn the_payload_is_an_object_holding_the_lines() {
        let payload = batch_payload(vec![plain_line("one")]);

        assert_eq!(payload["lines"][0]["message"], "one");
        assert_eq!(payload["lines"].as_array().unwrap().len(), 1);
    }

    /// The message text of each line, in order.
    fn messages(lines: &[Value]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line["message"].as_str().expect("a message").to_string())
            .collect()
    }
}
